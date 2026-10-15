use sha2::{Digest, Sha256};
use shipments::{models::*, store::Store};

#[tokio::test]
async fn cold_backup_restores_records_history_and_replay_without_wal() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("live.sqlite3");
    let backup = dir.path().join("restored.sqlite3");
    let store = Store::open(&path).await.unwrap();
    let active_digest: [u8; 32] = Sha256::digest("active-disposable-test-credential").into();
    let revoked_digest: [u8; 32] = Sha256::digest("revoked-disposable-test-credential").into();
    let active = store
        .create_agent("Importer", &active_digest)
        .await
        .unwrap();
    let revoked = store
        .create_agent("Retired importer", &revoked_digest)
        .await
        .unwrap();
    assert!(store.revoke_agent(revoked.id).await.unwrap());
    let input = CreateShipment {
        title: "Backup parcel".into(),
        merchant: None,
        order_reference: Some("TEST-123".into()),
        tracking: None,
        source: Some(Source {
            namespace: "test".into(),
            key: "message".into(),
        }),
    };
    let original = store
        .create(input.clone(), "owner", "durable-key")
        .await
        .unwrap();
    let changed = store
        .set_override(
            original.id,
            original.revision,
            Some(SetOverride {
                status: Status::Delivered,
                reason: "Received".into(),
            }),
            "human",
        )
        .await
        .unwrap();
    store.close().await.unwrap();
    std::fs::copy(&path, &backup).unwrap();
    let restored = Store::open(&backup).await.unwrap();
    assert_eq!(
        restored.agent_identity(&active_digest).await.unwrap(),
        Some(active.id)
    );
    assert_eq!(
        restored.agent_identity(&revoked_digest).await.unwrap(),
        None
    );
    let agents = restored.agents().await.unwrap();
    assert_eq!(agents.items.len(), 1);
    assert_eq!(agents.items[0].name, "Importer");
    let record = restored.get(original.id).await.unwrap();
    assert_eq!(record.revision, changed.revision);
    assert_eq!(record.status, Status::Delivered);
    assert_eq!(record.sources, original.sources);
    let events = restored.events(original.id, 0, 100).await.unwrap();
    assert_eq!(events.len(), 2);
    assert_eq!(
        events[0].snapshot.as_ref().unwrap().status,
        Status::Expected
    );
    assert_eq!(
        restored
            .create(input, "owner", "durable-key")
            .await
            .unwrap()
            .revision,
        1
    );
    restored.close().await.unwrap();
}
