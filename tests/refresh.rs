use shipments::{
    models::{CreateShipment, InputTracking, Shipment},
    store::Store,
};

struct Fixture {
    _directory: tempfile::TempDir,
    store: Store,
    shipment: Shipment,
}

impl Fixture {
    async fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(&directory.path().join("shipments.sqlite3"))
            .await
            .unwrap();
        let shipment = store
            .create(
                CreateShipment {
                    direction: shipments::models::Direction::Inbound,
                    title: "Refresh test parcel".into(),
                    merchant: None,
                    order_reference: None,
                    signature_required: None,
                    tracking: InputTracking {
                        recipient_url: None,
                        number: "9400111899560000000000".into(),
                        carrier: Some("usps".parse().unwrap()),
                    },
                    source: None,
                },
                "test",
                "refresh-test",
                None,
            )
            .await
            .unwrap();
        Self {
            _directory: directory,
            store,
            shipment,
        }
    }

    async fn refresh(&self) -> Shipment {
        self.store
            .refresh(self.shipment.id, self.shipment.revision, "test")
            .await
            .unwrap()
    }

    async fn now(&self) -> i64 {
        sqlx::query_scalar("SELECT unixepoch()")
            .fetch_one(self.store.pool())
            .await
            .unwrap()
    }
}

#[tokio::test]
async fn refresh_cooldown_covers_success_failures_and_terminal_shipments() {
    let fixture = Fixture::new().await;
    for (terminal, failed, already_due) in [
        (false, false, false),
        (false, false, true),
        (true, false, false),
        (true, true, false),
    ] {
        let attempted: i64 = sqlx::query_scalar(
            "UPDATE shipments SET last_attempt_at=unixepoch(),
                 carrier_status=CASE WHEN ? THEN '\"delivered\"' ELSE '\"in_transit\"' END,
                 last_error=CASE WHEN ? THEN 'Temporary provider failure' END,
                 next_attempt_at=CASE WHEN ? THEN NULL WHEN ? THEN unixepoch()-10 ELSE unixepoch()+1800 END
             WHERE id=? RETURNING last_attempt_at",
        )
        .bind(terminal)
        .bind(failed)
        .bind(terminal)
        .bind(already_due)
        .bind(fixture.shipment.id.to_string())
        .fetch_one(fixture.store.pool())
        .await
        .unwrap();

        let refreshed = fixture.refresh().await;
        assert_eq!(refreshed.sync.next_attempt_at, Some(attempted + 60));
        assert_eq!(
            fixture.refresh().await.sync.next_attempt_at,
            refreshed.sync.next_attempt_at,
            "repeated refreshes must coalesce"
        );
    }
}

#[tokio::test]
async fn refresh_preserves_longer_retry_backoff_and_imminent_checks() {
    let fixture = Fixture::new().await;
    for (failed, delay) in [(true, 900_i64), (false, 5_i64)] {
        let due: i64 = sqlx::query_scalar(
            "UPDATE shipments SET last_attempt_at=unixepoch(),
                 last_error=CASE WHEN ? THEN 'Temporary provider failure' END,
                 next_attempt_at=unixepoch()+? WHERE id=? RETURNING next_attempt_at",
        )
        .bind(failed)
        .bind(delay)
        .bind(fixture.shipment.id.to_string())
        .fetch_one(fixture.store.pool())
        .await
        .unwrap();
        assert_eq!(fixture.refresh().await.sync.next_attempt_at, Some(due));
        assert_eq!(fixture.refresh().await.sync.next_attempt_at, Some(due));
    }
}

#[tokio::test]
async fn refresh_leaves_an_active_claim_and_its_schedule_intact() {
    let fixture = Fixture::new().await;
    let (due, lease): (i64, i64) = sqlx::query_as(
        "UPDATE shipments SET last_attempt_at=unixepoch(), next_attempt_at=unixepoch()-10,
             lease_until=unixepoch()+60, lease_token='active-claim' WHERE id=?
         RETURNING next_attempt_at,lease_until",
    )
    .bind(fixture.shipment.id.to_string())
    .fetch_one(fixture.store.pool())
    .await
    .unwrap();
    assert_eq!(fixture.refresh().await.sync.next_attempt_at, Some(due));
    let actual: (i64, String) =
        sqlx::query_as("SELECT lease_until,lease_token FROM shipments WHERE id=?")
            .bind(fixture.shipment.id.to_string())
            .fetch_one(fixture.store.pool())
            .await
            .unwrap();
    assert_eq!(actual, (lease, "active-claim".into()));
}

#[tokio::test]
async fn refresh_queues_now_when_never_attempted_or_cooldown_expired() {
    let fixture = Fixture::new().await;
    for never_attempted in [true, false] {
        sqlx::query(
            "UPDATE shipments SET
                 last_attempt_at=CASE WHEN ? THEN NULL ELSE unixepoch()-61 END,
                 last_error=NULL,next_attempt_at=unixepoch()+1800 WHERE id=?",
        )
        .bind(never_attempted)
        .bind(fixture.shipment.id.to_string())
        .execute(fixture.store.pool())
        .await
        .unwrap();
        let before = fixture.now().await;
        let due = fixture.refresh().await.sync.next_attempt_at.unwrap();
        let after = fixture.now().await;
        assert!((before..=after).contains(&due));
    }
}
