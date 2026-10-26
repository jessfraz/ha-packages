use shipments::{carrier::Carrier, models::InputTracking};

#[test]
fn admission_resolves_defaults_and_preserves_explicit_products() {
    for (input, expected) in [
        ("fedex", Carrier::FedExDefault),
        (" FedEx ", Carrier::FedExDefault),
        ("FedExDefault", Carrier::FedExDefault),
        ("FedExAccount", Carrier::FedEx),
        ("UPS", Carrier::Ups),
        ("UPSDAP", Carrier::UpsDap),
        ("USPS", Carrier::Usps),
        ("DHLExpress", Carrier::DhlExpress),
        ("DHLExpressDefault", Carrier::DhlExpressDefault),
        ("FedexSmartPost", Carrier::FedexSmartPost),
        ("SpeedX", Carrier::SpeedX),
        ("DoorDash", Carrier::DoorDash),
    ] {
        let tracking: InputTracking = serde_json::from_value(serde_json::json!({
            "number": "SYNTHETIC-PARCEL", "carrier": input
        }))
        .unwrap();
        assert_eq!(tracking.carrier, Some(expected), "{input}");
        let roundtrip: InputTracking =
            serde_json::from_value(serde_json::to_value(&tracking).unwrap()).unwrap();
        assert_eq!(tracking, roundtrip);
    }
}

#[test]
fn unknown_carriers_fail_and_detection_remains_explicit() {
    assert!(
        serde_json::from_value::<InputTracking>(
            serde_json::json!({"number":"SYNTHETIC", "carrier":"NotACarrier"})
        )
        .is_err()
    );
    for value in [
        serde_json::json!({"number":"SYNTHETIC"}),
        serde_json::json!({"number":"SYNTHETIC","carrier":null}),
    ] {
        assert_eq!(
            serde_json::from_value::<InputTracking>(value)
                .unwrap()
                .carrier,
            None
        );
    }
    // Provider responses and persisted history must not be reinterpreted.
    assert_eq!(
        serde_json::from_str::<Carrier>("\"FedEx\"").unwrap(),
        Carrier::FedEx
    );
}
