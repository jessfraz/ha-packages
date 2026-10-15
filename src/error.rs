#[derive(Debug, thiserror::Error)]
pub enum Error {
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
            Error::Invalid(_) => dropshot::ClientErrorStatusCode::BAD_REQUEST,
            Error::NotFound => dropshot::ClientErrorStatusCode::NOT_FOUND,
            Error::Conflict(_) => dropshot::ClientErrorStatusCode::CONFLICT,
            Error::Precondition => dropshot::ClientErrorStatusCode::PRECONDITION_FAILED,
            _ => return Self::for_internal_error(error.to_string()),
        };
        Self::for_client_error(None, status, error.to_string())
    }
}
