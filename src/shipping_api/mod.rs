//! Tracking backends. The worker owns persistence; implementations own wire
//! formats, credentials, pacing and safe registration recovery.

pub mod aftership;
pub mod doordash;
pub mod easypost;
pub mod error;
pub mod types;

use std::sync::Arc;

use crate::shipping_api::types::{ApiFuture, ProviderId, Registration, Tracker};

/// Keeps a provider account identity stable through the worker's short commit.
pub trait ObservationGuard: Send {}
impl ObservationGuard for () {}

pub trait ShippingApi: Send + Sync {
    fn observation_guard(&self) -> ApiFuture<'_, Box<dyn ObservationGuard + '_>> {
        Box::pin(async { Ok(Box::new(()) as Box<dyn ObservationGuard>) })
    }

    fn id(&self) -> ProviderId;
    fn blocked(&self) -> bool;
    /// A selected provider must independently verify this carrier and parcel identity.
    fn supports_carrier(&self, carrier: crate::carrier::Carrier) -> bool {
        ProviderId::for_carrier(Some(carrier)) == self.id()
    }
    /// Scope correlation fields while retaining the same shared pacing state.
    fn for_request(&self, request_id: &str, shipment_id: Option<&str>) -> Arc<dyn ShippingApi>;
    fn check_connection(&self) -> ApiFuture<'_, ()>;

    /// Identify the endpoint and credentials for a saved connection diagnostic.
    /// Return only a one-way digest, never credentials. None disables persistence.
    fn connection_fingerprint(&self) -> Option<[u8; 32]> {
        None
    }

    /// Read-only lookup. Only a complete, authoritative absence permits a new
    /// registration. An uncertain or ambiguous result must return an error.
    fn reconcile_registration<'a>(
        &'a self,
        registration: &'a Registration,
    ) -> ApiFuture<'a, Option<Tracker>>;

    /// Reserve pacing before the worker checks its durable ownership fence.
    /// Dropping this reservation cancels it without creating a tracker.
    fn prepare_registration(
        &self,
        registration: Registration,
    ) -> ApiFuture<'_, Box<dyn PreparedRegistration>>;

    fn retrieve<'a>(
        &'a self,
        tracker_id: &'a str,
        tracking_number: &'a str,
    ) -> ApiFuture<'a, Tracker>;
}

pub trait PreparedRegistration: Send {
    /// Consumes the reserved slot without another pacing wait. Recheck any
    /// provider recovery deadline here, immediately before the outbound write.
    fn register(self: Box<Self>) -> ApiFuture<'static, Tracker>;
}
