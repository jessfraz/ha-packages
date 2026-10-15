use std::{sync::Arc, time::Duration};

use shipments::{
    models::{
        CarrierEvent, CarrierTracking, CreateShipment, Direction, EventKind, InputTracking,
        RegistrationState, SetOverride, Shipment, Status,
    },
    shipping_api::{
        PreparedRegistration, ShippingApi,
        types::{ApiFuture, ProviderId, Registration, Tracker},
    },
    store::Store,
    sync::SyncWorker,
};
use tokio::sync::{
    Semaphore,
    mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel},
};

const NUMBER: &str = "9400100000000000000001";
const OPAQUE_ID: &str = "parcel/vendor:alpha";
const OLD_INTENT: i64 = 946684800;

#[derive(Debug, PartialEq, Eq)]
enum Call {
    Reconcile { number: String, started_at: i64 },
    Prepare,
    Register,
    Retrieve { id: String, number: String },
    Cancel,
}

// An independent implementation exercises the worker contract without using
// EasyPost's client, ID format, pacing, or three-month recovery policy. Its
// configured identity stays EasyPost until another adapter is supported.
#[derive(Clone)]
struct ContractApi {
    calls: UnboundedSender<Call>,
    existing: bool,
    reservation_gate: Option<Arc<Semaphore>>,
}

impl ContractApi {
    fn new(existing: bool) -> (Self, UnboundedReceiver<Call>) {
        let (calls, receiver) = unbounded_channel();
        (
            Self {
                calls,
                existing,
                reservation_gate: None,
            },
            receiver,
        )
    }
}

impl ShippingApi for ContractApi {
    fn id(&self) -> ProviderId {
        ProviderId::EasyPost
    }

    fn blocked(&self) -> bool {
        false
    }

    fn for_request(&self, request_id: &str, shipment_id: Option<&str>) -> Arc<dyn ShippingApi> {
        uuid::Uuid::parse_str(request_id).unwrap();
        uuid::Uuid::parse_str(shipment_id.unwrap()).unwrap();
        Arc::new(self.clone())
    }

    fn check_connection(&self) -> ApiFuture<'_, ()> {
        Box::pin(async { Ok(()) })
    }

    fn reconcile_registration<'a>(
        &'a self,
        registration: &'a Registration,
    ) -> ApiFuture<'a, Option<Tracker>> {
        Box::pin(async move {
            assert_eq!(registration.carrier, Some("USPS".parse().unwrap()));
            self.calls
                .send(Call::Reconcile {
                    number: registration.tracking_number.clone(),
                    started_at: registration.started_at,
                })
                .unwrap();
            Ok(self.existing.then(|| observation(Status::InTransit)))
        })
    }

    fn prepare_registration(
        &self,
        registration: Registration,
    ) -> ApiFuture<'_, Box<dyn PreparedRegistration>> {
        Box::pin(async move {
            assert_eq!(registration.tracking_number, NUMBER);
            self.calls.send(Call::Prepare).unwrap();
            if let Some(gate) = &self.reservation_gate {
                gate.acquire().await.unwrap().forget();
            }
            let reservation: Box<dyn PreparedRegistration> = Box::new(Reservation {
                calls: self.calls.clone(),
                registered: false,
            });
            Ok(reservation)
        })
    }

    fn retrieve<'a>(
        &'a self,
        tracker_id: &'a str,
        tracking_number: &'a str,
    ) -> ApiFuture<'a, Tracker> {
        Box::pin(async move {
            self.calls
                .send(Call::Retrieve {
                    id: tracker_id.into(),
                    number: tracking_number.into(),
                })
                .unwrap();
            Ok(observation(Status::Delivered))
        })
    }
}

struct Reservation {
    calls: UnboundedSender<Call>,
    registered: bool,
}

impl PreparedRegistration for Reservation {
    fn register(mut self: Box<Self>) -> ApiFuture<'static, Tracker> {
        self.registered = true;
        let calls = self.calls.clone();
        Box::pin(async move {
            calls.send(Call::Register).unwrap();
            Ok(observation(Status::InTransit))
        })
    }
}

impl Drop for Reservation {
    fn drop(&mut self) {
        if !self.registered {
            let _ = self.calls.send(Call::Cancel);
        }
    }
}

fn observation(status: Status) -> Tracker {
    Tracker {
        id: OPAQUE_ID.into(),
        tracking_code: NUMBER.into(),
        carrier: "USPS".parse().unwrap(),
        status,
        public_url: Some("https://example.com/parcel/alpha".into()),
        carrier_tracking: CarrierTracking {
            events: vec![CarrierEvent {
                status: Some(status),
                message: Some(format!("Parcel is {status:?}")),
                datetime: Some(
                    if status == Status::Delivered {
                        "2026-01-02T12:00:00Z"
                    } else {
                        "2026-01-01T12:00:00Z"
                    }
                    .into(),
                ),
                ..Default::default()
            }],
            ..Default::default()
        },
    }
}

struct Fixture {
    _directory: tempfile::TempDir,
    store: Store,
    shipment: Shipment,
}

impl Fixture {
    async fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(&directory.path().join("contract.sqlite3"))
            .await
            .unwrap();
        let shipment = store
            .create(
                CreateShipment {
                    title: "Provider contract parcel".into(),
                    direction: Direction::Inbound,
                    tracking: InputTracking {
                        number: NUMBER.into(),
                        carrier: Some("USPS".parse().unwrap()),
                    },
                    merchant: None,
                    order_reference: None,
                    signature_required: None,
                    source: None,
                },
                "owner",
                "contract-create",
                None,
            )
            .await
            .unwrap();
        sqlx::query("UPDATE shipments SET registration_started_at=? WHERE id=?")
            .bind(OLD_INTENT)
            .bind(shipment.id.to_string())
            .execute(store.pool())
            .await
            .unwrap();
        Self {
            _directory: directory,
            store,
            shipment,
        }
    }
}

async fn next(calls: &mut UnboundedReceiver<Call>) -> Call {
    tokio::time::timeout(Duration::from_secs(2), calls.recv())
        .await
        .unwrap()
        .unwrap()
}

fn lookup() -> Call {
    Call::Reconcile {
        number: NUMBER.into(),
        started_at: OLD_INTENT,
    }
}

#[tokio::test]
async fn worker_uses_provider_recovery_policy_and_treats_ids_as_opaque() {
    let fixture = Fixture::new().await;
    let (api, mut calls) = ContractApi::new(false);
    let worker = SyncWorker::new(fixture.store.pool().clone(), Arc::new(api));
    assert!(worker.tick().await.unwrap());
    assert_eq!(next(&mut calls).await, lookup());
    assert_eq!(next(&mut calls).await, Call::Prepare);
    assert_eq!(next(&mut calls).await, Call::Register);
    let registered = fixture.store.get(fixture.shipment.id).await.unwrap();
    assert_eq!(registered.sync.provider, ProviderId::EasyPost);
    assert_eq!(registered.sync.tracker_id.as_deref(), Some(OPAQUE_ID));
    assert_eq!(registered.sync.registration_started_at, Some(OLD_INTENT));
    assert_eq!(
        registered.sync.registration_state,
        RegistrationState::Registered
    );
    assert_eq!(registered.status, Status::InTransit);
    assert_eq!(registered.carrier_tracking.events.len(), 1);

    sqlx::query("UPDATE shipments SET next_attempt_at=0 WHERE id=?")
        .bind(fixture.shipment.id.to_string())
        .execute(fixture.store.pool())
        .await
        .unwrap();
    assert!(worker.tick().await.unwrap());
    assert_eq!(
        next(&mut calls).await,
        Call::Retrieve {
            id: OPAQUE_ID.into(),
            number: NUMBER.into(),
        }
    );
    let delivered = fixture.store.get(fixture.shipment.id).await.unwrap();
    assert_eq!(delivered.status, Status::Delivered);
    assert_eq!(delivered.carrier_tracking.events.len(), 2);
    assert!(delivered.sync.next_attempt_at.is_none());
    let events = fixture.store.events(delivered.id, 0, 100).await.unwrap();
    let updates: Vec<_> = events
        .iter()
        .filter(|event| matches!(event.kind, EventKind::CarrierUpdated))
        .collect();
    assert_eq!(updates.len(), 2);
    for event in updates {
        assert_eq!(event.actor_id, ProviderId::EasyPost.as_str());
        assert_eq!(
            event.snapshot.as_ref().unwrap().sync.tracker_id.as_deref(),
            Some(OPAQUE_ID)
        );
    }
    assert!(calls.try_recv().is_err());
    fixture.store.close().await.unwrap();
}

#[tokio::test]
async fn worker_adopts_a_reconciled_tracker_without_reserving_a_write() {
    let fixture = Fixture::new().await;
    let (api, mut calls) = ContractApi::new(true);
    let worker = SyncWorker::new(fixture.store.pool().clone(), Arc::new(api));
    assert!(worker.tick().await.unwrap());
    assert_eq!(next(&mut calls).await, lookup());
    let registered = fixture.store.get(fixture.shipment.id).await.unwrap();
    assert_eq!(registered.sync.tracker_id.as_deref(), Some(OPAQUE_ID));
    assert_eq!(
        registered.sync.registration_state,
        RegistrationState::Registered
    );
    assert!(calls.try_recv().is_err());
    fixture.store.close().await.unwrap();
}

#[tokio::test]
async fn terminal_correction_while_reserving_drops_registration_without_writing() {
    let fixture = Fixture::new().await;
    let (mut api, mut calls) = ContractApi::new(false);
    let gate = Arc::new(Semaphore::new(0));
    api.reservation_gate = Some(gate.clone());
    let worker = SyncWorker::new(fixture.store.pool().clone(), Arc::new(api));
    let task = tokio::spawn(async move { worker.tick().await });
    assert_eq!(next(&mut calls).await, lookup());
    assert_eq!(next(&mut calls).await, Call::Prepare);
    fixture
        .store
        .set_override(
            fixture.shipment.id,
            fixture.shipment.revision,
            Some(SetOverride {
                status: Status::Delivered,
                reason: "Received before registration".into(),
            }),
            "owner",
        )
        .await
        .unwrap();
    gate.add_permits(1);
    assert!(task.await.unwrap().unwrap());
    assert_eq!(next(&mut calls).await, Call::Cancel);
    let cancelled = fixture.store.get(fixture.shipment.id).await.unwrap();
    assert_eq!(cancelled.status, Status::Delivered);
    assert!(cancelled.sync.tracker_id.is_none());
    assert!(cancelled.sync.next_attempt_at.is_none());
    assert!(calls.try_recv().is_err());
    fixture.store.close().await.unwrap();
}

#[tokio::test]
async fn worker_leaves_records_owned_by_another_provider_untouched() {
    let fixture = Fixture::new().await;
    sqlx::query("UPDATE shipments SET tracking_provider='other' WHERE id=?")
        .bind(fixture.shipment.id.to_string())
        .execute(fixture.store.pool())
        .await
        .unwrap();
    let (api, mut calls) = ContractApi::new(false);
    let worker = SyncWorker::new(fixture.store.pool().clone(), Arc::new(api));
    assert!(!worker.tick().await.unwrap());
    let state = sqlx::query_as::<_, (String, Option<i64>, Option<String>)>(
        "SELECT registration_state,last_attempt_at,lease_token FROM shipments WHERE id=?",
    )
    .bind(fixture.shipment.id.to_string())
    .fetch_one(fixture.store.pool())
    .await
    .unwrap();
    assert_eq!(state, ("pending".into(), None, None));
    assert!(calls.try_recv().is_err());
    fixture.store.close().await.unwrap();
}
