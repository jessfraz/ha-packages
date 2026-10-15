//! Produce a portable database through the same storage path as verified API
//! admission. Container smoke tests never need a provider key or an API bypass.

use shipments::{
    models::{CreateShipment, Direction, InputTracking, RegistrationState, Status},
    shipping_api::types::Tracker,
    store::Store,
};

#[tokio::test]
#[ignore = "exports the database requested by the container smoke script"]
async fn export_container_fixture() {
    let path = std::path::PathBuf::from(
        std::env::var_os("SHIPMENTS_CONTAINER_FIXTURE")
            .expect("set SHIPMENTS_CONTAINER_FIXTURE to a new database path"),
    );
    assert!(
        !path.exists(),
        "fixture export must not overwrite a database"
    );
    let store = Store::open(&path).await.unwrap();
    let mut ids = Vec::new();
    for (title, direction, number, carrier, tracker_id, merchant) in [
        (
            "Container persistence check",
            Direction::Inbound,
            "9400111899223856926381",
            "USPS",
            "trk_container_persistence",
            Some("Test fixture"),
        ),
        (
            "Agent shipment",
            Direction::Outbound,
            "1Z999AA10123456784",
            "UPS",
            "trk_container_agent",
            None,
        ),
    ] {
        let carrier = carrier.parse().unwrap();
        let tracker = Tracker {
            provider: shipments::shipping_api::types::ProviderId::EasyPost,
            id: tracker_id.into(),
            tracking_code: number.into(),
            carrier,
            status: Status::Unknown,
            public_url: None,
            carrier_tracking: Default::default(),
        };
        let saved = store
            .create(
                CreateShipment {
                    title: title.into(),
                    direction,
                    merchant: merchant.map(str::to_owned),
                    order_reference: None,
                    signature_required: None,
                    tracking: InputTracking {
                        recipient_url: None,
                        number: number.into(),
                        carrier: Some(carrier),
                    },
                    source: None,
                },
                "container-fixture",
                tracker_id,
                Some(&tracker),
            )
            .await
            .unwrap();
        assert_eq!(saved.tracking.carrier, carrier);
        assert_eq!(saved.sync.registration_state, RegistrationState::Registered);
        assert_eq!(saved.sync.tracker_id.as_deref(), Some(tracker_id));
        ids.push(saved.id);
    }
    store.close().await.unwrap();
    // Docker copies only the checkpointed main file, never live WAL state.
    assert!(!std::path::PathBuf::from(format!("{}-wal", path.display())).exists());
    let reopened = Store::open(&path).await.unwrap();
    for id in ids {
        let saved = reopened.get(id).await.unwrap();
        assert_eq!(saved.sync.registration_state, RegistrationState::Registered);
        assert!(saved.sync.tracker_id.is_some());
        assert_eq!(reopened.events(id, 0, 100).await.unwrap().len(), 1);
    }
    reopened.close().await.unwrap();
}
