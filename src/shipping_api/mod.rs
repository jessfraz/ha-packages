//! Tracking backends. The worker owns persistence; implementations own wire
//! formats, credentials, pacing and safe registration recovery.

pub mod easypost;
pub mod error;
pub mod types;

use std::sync::Arc;

use crate::shipping_api::types::{ApiFuture, ProviderId, Registration, Tracker};

pub trait ShippingApi: Send + Sync {
    fn id(&self) -> ProviderId;
    fn blocked(&self) -> bool;
    /// Scope correlation fields while retaining the same shared pacing state.
    fn for_request(&self, request_id: &str, shipment_id: Option<&str>) -> Arc<dyn ShippingApi>;
    fn check_connection(&self) -> ApiFuture<'_, ()>;

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
