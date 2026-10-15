#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("A tracking number is required. Add one before saving; tracking cannot be cleared.")]
    TrackingRequired,
    #[error("{0}")]
    Invalid(String),
    #[error("shipment not found")]
    NotFound,
    #[error("{0}")]
    Conflict(String),
    #[error("shipment changed; fetch its current revision before editing")]
    Precondition,
    #[error(transparent)]
    Database(#[from] sqlx::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error(transparent)]
    Migration(#[from] sqlx::migrate::MigrateError),
}

impl From<Error> for dropshot::HttpError {
    fn from(error: Error) -> Self {
        let status = match error {
            Error::TrackingRequired => dropshot::ClientErrorStatusCode::BAD_REQUEST,
            Error::Invalid(_) => dropshot::ClientErrorStatusCode::BAD_REQUEST,
            Error::NotFound => dropshot::ClientErrorStatusCode::NOT_FOUND,
            Error::Conflict(_) => dropshot::ClientErrorStatusCode::CONFLICT,
            Error::Precondition => dropshot::ClientErrorStatusCode::PRECONDITION_FAILED,
            _ => return Self::for_internal_error(error.to_string()),
        };
        let code = match error {
            Error::TrackingRequired => "tracking_required",
            Error::Invalid(_) => "invalid_input",
            Error::NotFound => "not_found",
            Error::Conflict(_) => "conflict",
            Error::Precondition => "revision_conflict",
            _ => "internal_error",
        };
        Self::for_client_error(Some(code.into()), status, error.to_string())
    }
}
