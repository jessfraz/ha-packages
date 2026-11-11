use std::{future::Future, pin::Pin};

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::pickup::models::{Address, PostalState};

pub type AddressVerificationFuture<'a> =
    Pin<Box<dyn Future<Output = AddressVerification> + Send + 'a>>;

#[derive(Clone, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum AddressVerification {
    Verified { normalized: Option<Address> },
    Suggested { suggestions: Vec<Address> },
    NoMatch,
    Unavailable,
}

impl AddressVerification {
    pub(super) fn verified(original: &Address, candidate: Address) -> Self {
        Self::Verified {
            normalized: (&candidate != original).then_some(candidate),
        }
    }
}

pub(super) fn postal_state(code: &str) -> Option<PostalState> {
    PostalState::ALL
        .iter()
        .copied()
        .find(|state| state.as_str() == code)
}
