use sd_core_prisma_helpers::DevicePubId;

use sd_prisma::{
	prisma::{crdt_operation, PrismaClient, SortOrder},
	prisma_sync::ModelSyncData,
};
use sd_sync::{
	CRDTOperation, CRDTOperationData, CompressedCRDTOperation, CompressedCRDTOperationsPerModel,
	CompressedCRDTOperationsPerModelPerDevice, ModelId, OperationKind, RecordId,
};

use std::{
	collections::BTreeMap,
	num::NonZeroU128,
	ops::Deref,
	pin::pin,
	sync::{atomic::Ordering, Arc},
	time::SystemTime,
};

use async_channel as chan;
use futures::{stream, FutureExt, StreamExt};
use futures_concurrency::{future::TryJoin, stream::Merge};
use prisma_client_rust::chrono::{DateTime, Utc};
use tokio::sync::oneshot;
use tracing::{debug, error, instrument, trace, warn};
use uhlc::{Timestamp, NTP64};
use uuid::Uuid;

use super::{
	actor::{create_actor_io, ActorIO, ActorTypes, HandlerIO},
	db_operation::write_crdt_op_to_db,
	Error, SharedState,
};

#[derive(Debug)]
#[must_use]
/// Stuff that can be handled outside the actor
pub enum Request {
	Messages {
		timestamps: Vec<(DevicePubId, NTP64)>,
		tx: oneshot::Sender<()>,
	},
	FinishedIngesting,
}

/// Stuff that the actor consumes
#[derive(Debug)]
pub enum Event {
	Notification,
	Messages(MessagesEvent),
}

#[derive(Debug, Default)]
pub enum State {
	#[default]
	WaitingForNotification,
	RetrievingMessages,
	Ingesting(MessagesEvent),
}

/// The single entrypoint for sync operation ingestion.
/// Requests sync operations in a given timestamp range,
/// and attempts to write them to the sync operations table along with
/// the actual cell that the operation points to.
///
/// If this actor stops running, no sync operations will
/// be applied to the database, independent of whether systems like p2p
/// or cloud are exchanging messages.
pub struct Actor {
	state: Option<State>,
	shared: Arc<SharedState>,
	io: ActorIO<Self>,
}

impl Actor {
	#[instrument(skip(self), fields(old_state = ?self.state))]
	async fn tick(&mut self) {
		let state = match self
			.state
			.take()
			.expect("ingest actor in inconsistent state")
		{
			State::WaitingForNotification => self.waiting_for_notification_state_transition().await,
			State::RetrievingMessages => self.retrieving_messages_state_transition().await,
			State::Ingesting(event) => self.ingesting_state_transition(event).await,
		};

		trace!(?state, "Actor state transitioned;");

		self.state = Some(state);
	}

	async fn waiting_for_notification_state_transition(&self) -> State {
		self.shared.active.store(false, Ordering::Relaxed);
		self.shared.active_notify.notify_waiters();

		loop {
			match self
				.io
				.event_rx
				.recv()
				.await
				.expect("sync actor receiver unexpectedly closed")
			{
				Event::Notification => {
					trace!("Received notification");
					break;
				}
				Event::Messages(event) => {
					trace!(
						?event,
						"Ignored event message as we're waiting for a `Event::Notification`"
					);
				}
			}
		}

		self.shared.active.store(true, Ordering::Relaxed);
		self.shared.active_notify.notify_waiters();

		State::RetrievingMessages
	}

	async fn retrieving_messages_state_transition(&self) -> State {
		enum StreamMessage {
			NewEvent(Event),
			AckedRequest(Result<(), oneshot::error::RecvError>),
		}

		let (tx, rx) = oneshot::channel::<()>();

		let timestamps = self
			.shared
			.timestamp_per_device
			.read()
			.await
			.iter()
			.map(|(uid, &timestamp)| (uid.clone(), timestamp))
			.collect();

		if self
			.io
			.send(Request::Messages { timestamps, tx })
			.await
			.is_err()
		{
			warn!("Failed to send messages request");
		}

		let mut msg_stream = pin!((
			self.io.event_rx.clone().map(StreamMessage::NewEvent),
			stream::once(rx.map(StreamMessage::AckedRequest)),
		)
			.merge());

		loop {
			if let Some(msg) = msg_stream.next().await {
				match msg {
					StreamMessage::NewEvent(event) => {
						if let Event::Messages(messages) = event {
							trace!(?messages, "Received messages;");
							break State::Ingesting(messages);
						}
					}
					StreamMessage::AckedRequest(res) => {
						if res.is_err() {
							debug!("messages request ignored");
							break State::WaitingForNotification;
						}
					}
				}
			} else {
				break State::WaitingForNotification;
			}
		}
	}

	async fn ingesting_state_transition(&mut self, event: MessagesEvent) -> State {
		debug!(
			messages_count = event.messages.len(),
			first_message = ?DateTime::<Utc>::from(
				event.messages
					.first()
					.map_or(SystemTime::UNIX_EPOCH, |m| m.3.timestamp.to_system_time())
			),
			last_message = ?DateTime::<Utc>::from(
				event.messages
					.last()
					.map_or(SystemTime::UNIX_EPOCH, |m| m.3.timestamp.to_system_time())
			),
			"Ingesting operations;",
		);

		for (device_pub_id, CompressedCRDTOperationsPerModel(data)) in event.messages.0 {
			for (model, data) in data {
				for (record, ops) in data {
					if let Err(e) = self
						.process_crdt_operations(device_pub_id.into(), model, record, ops)
						.await
					{
						error!(?e, "Failed to ingest CRDT operations;");
					}
				}
			}
		}

		if let Some(tx) = event.wait_tx {
			if tx.send(()).is_err() {
				warn!("Failed to send wait_tx signal");
			}
		}

		if event.has_more {
			State::RetrievingMessages
		} else {
			{
				if self.io.send(Request::FinishedIngesting).await.is_err() {
					error!("Failed to send finished ingesting request");
				}

				State::WaitingForNotification
			}
		}
	}

	pub async fn declare(shared: Arc<SharedState>) -> Handler {
		let (io, HandlerIO { event_tx, req_rx }) = create_actor_io::<Self>();

		// shared
		// 	.actors
		// 	.declare("Sync Ingest", {
		// 		let shared = Arc::clone(&shared);
		// 		move |stop| async move {
		// 			enum Race {
		// 				Ticked,
		// 				Stopped,
		// 			}

		// 			let mut this = Self {
		// 				state: Some(State::default()),
		// 				io,
		// 				shared,
		// 			};

		// 			while matches!(
		// 				(
		// 					this.tick().map(|()| Race::Ticked),
		// 					stop.into_future().map(|()| Race::Stopped),
		// 				)
		// 					.race()
		// 					.await,
		// 				Race::Ticked
		// 			) { /* Everything is Awesome! */ }
		// 		}
		// 	})
		// 	.await;

		Handler { event_tx, req_rx }
	}

	// where the magic happens
	#[instrument(skip(self, ops), fields(operations_count = %ops.len()), err)]
	async fn process_crdt_operations(
		&self,
		device_pub_id: DevicePubId,
		model: ModelId,
		record_id: RecordId,
		mut ops: Vec<CompressedCRDTOperation>,
	) -> Result<(), Error> {
		let db = &self.shared.db;

		ops.sort_by_key(|op| op.timestamp);

		let new_timestamp = ops.last().expect("Empty ops array").timestamp;

		// first, we update the HLC's timestamp with the incoming one.
		// this involves a drift check + sets the last time of the clock
		self.shared
			.clock
			.update_with_timestamp(&Timestamp::new(
				new_timestamp,
				uhlc::ID::from(
					NonZeroU128::new(Uuid::from(&device_pub_id).to_u128_le()).expect("Non zero id"),
				),
			))
			.expect("timestamp has too much drift!");

		// Delete - ignores all other messages
		if let Some(delete_op) = ops
			.iter()
			.rev()
			.find(|op| matches!(op.data, CRDTOperationData::Delete))
		{
			trace!("Deleting operation");
			handle_crdt_deletion(db, &device_pub_id, model, record_id, delete_op).await?;
		}
		// Create + > 0 Update - overwrites the create's data with the updates
		else if let Some(timestamp) = ops
			.iter()
			.rev()
			.find_map(|op| matches!(&op.data, CRDTOperationData::Create(_)).then_some(op.timestamp))
		{
			trace!("Create + Updates operations");

			// conflict resolution
			let delete = db
				.crdt_operation()
				.find_first(vec![
					crdt_operation::model::equals(i32::from(model)),
					crdt_operation::record_id::equals(rmp_serde::to_vec(&record_id)?),
					crdt_operation::kind::equals(OperationKind::Delete.to_string()),
				])
				.order_by(crdt_operation::timestamp::order(SortOrder::Desc))
				.exec()
				.await?;

			if delete.is_some() {
				debug!("Found a previous delete operation with the same SyncId, will ignore these operations");
				return Ok(());
			}

			handle_crdt_create_and_updates(db, &device_pub_id, model, record_id, ops, timestamp)
				.await?;
		}
		// > 0 Update - batches updates with a fake Create op
		else {
			trace!("Updates operation");

			let mut data = BTreeMap::new();

			for op in ops.into_iter().rev() {
				let CRDTOperationData::Update { field, value } = op.data else {
					unreachable!("Create + Delete should be filtered out!");
				};

				data.insert(field, (value, op.timestamp));
			}

			// conflict resolution
			let (create, updates) = db
				._batch((
					db.crdt_operation()
						.find_first(vec![
							crdt_operation::model::equals(i32::from(model)),
							crdt_operation::record_id::equals(rmp_serde::to_vec(&record_id)?),
							crdt_operation::kind::equals(OperationKind::Create.to_string()),
						])
						.order_by(crdt_operation::timestamp::order(SortOrder::Desc)),
					data.iter()
						.map(|(k, (_, timestamp))| {
							Ok(db
								.crdt_operation()
								.find_first(vec![
									crdt_operation::timestamp::gt({
										#[allow(clippy::cast_possible_wrap)]
										// SAFETY: we had to store using i64 due to SQLite limitations
										{
											timestamp.as_u64() as i64
										}
									}),
									crdt_operation::model::equals(i32::from(model)),
									crdt_operation::record_id::equals(rmp_serde::to_vec(
										&record_id,
									)?),
									crdt_operation::kind::equals(
										OperationKind::Update(k).to_string(),
									),
								])
								.order_by(crdt_operation::timestamp::order(SortOrder::Desc)))
						})
						.collect::<Result<Vec<_>, Error>>()?,
				))
				.await?;

			if create.is_none() {
				warn!("Failed to find a previous create operation with the same SyncId");
				return Ok(());
			}

			handle_crdt_updates(db, &device_pub_id, model, record_id, data, updates).await?;
		}

		// read the timestamp for the operation's device, or insert one if it doesn't exist
		let current_last_timestamp = self
			.shared
			.timestamp_per_device
			.read()
			.await
			.get(&device_pub_id)
			.copied();

		// update the stored timestamp for this device - will be derived from the crdt operations table on restart
		let new_ts = NTP64::max(current_last_timestamp.unwrap_or_default(), new_timestamp);

		self.shared
			.timestamp_per_device
			.write()
			.await
			.insert(device_pub_id, new_ts);

		Ok(())
	}
}

async fn handle_crdt_updates(
	db: &PrismaClient,
	device_pub_id: &DevicePubId,
	model: u16,
	record_id: rmpv::Value,
	mut data: BTreeMap<String, (rmpv::Value, NTP64)>,
	updates: Vec<Option<crdt_operation::Data>>,
) -> Result<(), Error> {
	let keys = data.keys().cloned().collect::<Vec<_>>();
	let device_pub_id = sd_sync::DevicePubId::from(device_pub_id);

	// does the same thing as processing ops one-by-one and returning early if a newer op was found
	for (update, key) in updates.into_iter().zip(keys) {
		if update.is_some() {
			data.remove(&key);
		}
	}

	db._transaction()
		.with_timeout(30 * 1000)
		.run(|db| async move {
			// fake operation to batch them all at once
			ModelSyncData::from_op(CRDTOperation {
				device_pub_id,
				model_id: model,
				record_id: record_id.clone(),
				timestamp: NTP64(0),
				data: CRDTOperationData::Create(
					data.iter()
						.map(|(k, (data, _))| (k.clone(), data.clone()))
						.collect(),
				),
			})
			.ok_or(Error::InvalidModelId(model))?
			.exec(&db)
			.await?;

			// need to only apply ops that haven't been filtered out
			data.into_iter()
				.map(|(field, (value, timestamp))| {
					let record_id = record_id.clone();
					let db = &db;

					async move {
						write_crdt_op_to_db(
							&CRDTOperation {
								device_pub_id,
								model_id: model,
								record_id,
								timestamp,
								data: CRDTOperationData::Update { field, value },
							},
							db,
						)
						.await
					}
				})
				.collect::<Vec<_>>()
				.try_join()
				.await
				.map(|_| ())
		})
		.await
}

async fn handle_crdt_create_and_updates(
	db: &PrismaClient,
	device_pub_id: &DevicePubId,
	model_id: ModelId,
	record_id: rmpv::Value,
	ops: Vec<CompressedCRDTOperation>,
	timestamp: NTP64,
) -> Result<(), Error> {
	let mut data = BTreeMap::new();
	let device_pub_id = sd_sync::DevicePubId::from(device_pub_id);

	let mut applied_ops = vec![];

	// search for all Updates until a Create is found
	for op in ops.into_iter().rev() {
		match &op.data {
			CRDTOperationData::Delete => unreachable!("Delete can't exist here!"),
			CRDTOperationData::Create(create_data) => {
				for (k, v) in create_data {
					data.entry(k.clone()).or_insert_with(|| v.clone());
				}

				applied_ops.push(op);

				break;
			}
			CRDTOperationData::Update { field, value } => {
				data.insert(field.clone(), value.clone());
				applied_ops.push(op);
			}
		}
	}

	db._transaction()
		.with_timeout(30 * 1000)
		.run(|db| async move {
			// fake a create with a bunch of data rather than individual insert
			ModelSyncData::from_op(CRDTOperation {
				device_pub_id,
				model_id,
				record_id: record_id.clone(),
				timestamp,
				data: CRDTOperationData::Create(data),
			})
			.ok_or(Error::InvalidModelId(model_id))?
			.exec(&db)
			.await?;

			applied_ops
				.into_iter()
				.map(|CompressedCRDTOperation { timestamp, data }| {
					let record_id = record_id.clone();
					let db = &db;
					async move {
						write_crdt_op_to_db(
							&CRDTOperation {
								device_pub_id,
								timestamp,
								model_id,
								record_id,
								data,
							},
							db,
						)
						.await
					}
				})
				.collect::<Vec<_>>()
				.try_join()
				.await
				.map(|_| ())
		})
		.await
}

async fn handle_crdt_deletion(
	db: &PrismaClient,
	device_pub_id: &DevicePubId,
	model: u16,
	record_id: rmpv::Value,
	delete_op: &CompressedCRDTOperation,
) -> Result<(), Error> {
	// deletes are the be all and end all, no need to check anything
	let op = CRDTOperation {
		device_pub_id: device_pub_id.into(),
		model_id: model,
		record_id,
		timestamp: delete_op.timestamp,
		data: CRDTOperationData::Delete,
	};

	db._transaction()
		.with_timeout(30 * 1000)
		.run(|db| async move {
			ModelSyncData::from_op(op.clone())
				.ok_or(Error::InvalidModelId(model))?
				.exec(&db)
				.await?;

			write_crdt_op_to_db(&op, &db).await
		})
		.await
}

pub struct Handler {
	pub event_tx: chan::Sender<Event>,
	pub req_rx: chan::Receiver<Request>,
}

#[derive(Debug)]
pub struct MessagesEvent {
	pub device_pub_id: DevicePubId,
	pub messages: CompressedCRDTOperationsPerModelPerDevice,
	pub has_more: bool,
	pub wait_tx: Option<oneshot::Sender<()>>,
}

impl ActorTypes for Actor {
	type Event = Event;
	type Request = Request;
	type Handler = Handler;
}

#[cfg(test)]
mod test {
	use std::{sync::atomic::AtomicBool, time::Duration};

	use tokio::sync::Notify;
	use uhlc::HLCBuilder;

	use super::*;

	async fn new_actor() -> (Handler, Arc<SharedState>) {
		let device_pub_id = Uuid::now_v7();
		let shared = Arc::new(SharedState {
			db: sd_prisma::test_db().await,
			device_pub_id: device_pub_id.into(),
			clock: HLCBuilder::new()
				.with_id(uhlc::ID::from(
					NonZeroU128::new(device_pub_id.to_u128_le()).expect("Non zero id"),
				))
				.build(),
			timestamp_per_device: Arc::default(),
			emit_messages_flag: Arc::new(AtomicBool::new(true)),
			active: AtomicBool::default(),
			active_notify: Notify::default(),
		});

		(Actor::declare(Arc::clone(&shared)).await, shared)
	}

	/// If messages tx is dropped, actor should reset and assume no further messages
	/// will be sent
	#[tokio::test]
	async fn messages_request_drop() -> Result<(), ()> {
		let (ingest, _) = new_actor().await;

		for _ in 0..10 {
			ingest.event_tx.send(Event::Notification).await.unwrap();

			let Ok(Request::Messages { .. }) = ingest.req_rx.recv().await else {
				panic!("bruh")
			};

			// without this the test hangs, idk
			tokio::time::sleep(Duration::from_millis(0)).await;
		}

		Ok(())
	}
}
