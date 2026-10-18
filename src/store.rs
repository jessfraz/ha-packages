use crate::{
    carrier::Carrier,
    error::Error,
    models::*,
    shipping_api::types::{ProviderId, Tracker},
};
use sha2::{Digest, Sha256};
use sqlx::{
    FromRow, QueryBuilder, Sqlite, SqlitePool, Transaction,
    sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions},
};
use std::{collections::HashMap, path::Path, time::Duration};
use uuid::Uuid;

#[derive(Clone)]
pub struct Store {
    pool: SqlitePool,
}

#[derive(FromRow)]
struct Record {
    tracking_provider: ProviderId,
    id: String,
    revision: i64,
    title: String,
    direction: String,
    merchant: Option<String>,
    order_reference: Option<String>,
    signature_required: Option<bool>,
    carrier_tracking: String,
    tracking_carrier: String,
    tracking_number: String,
    recipient_url: Option<String>,
    carrier_status: Option<String>,
    override_status: Option<String>,
    override_reason: Option<String>,
    override_actor: Option<String>,
    override_at: Option<String>,
    archived: bool,
    created_at: String,
    updated_at: String,
    tracker_id: Option<String>,
    tracking_url: Option<String>,
    last_success_at: Option<String>,
    last_error: Option<String>,
    next_attempt_at: Option<i64>,
    attempts: i64,
    registration_state: String,
    registration_started_at: Option<i64>,
    last_error_code: Option<String>,
    last_provider_status: Option<u16>,
    last_provider_error_code: Option<String>,
}

impl Record {
    fn into_shipment(self, sources: Vec<Source>) -> Result<Shipment, Error> {
        let carrier_status = self
            .carrier_status
            .as_deref()
            .map(serde_json::from_str)
            .transpose()?;
        let status_override = self
            .override_status
            .as_deref()
            .map(|status| -> Result<StatusOverride, Error> {
                Ok(StatusOverride {
                    status: serde_json::from_str(status)?,
                    reason: self.override_reason.unwrap_or_default(),
                    actor_id: self.override_actor.unwrap_or_default(),
                    set_at: self.override_at.unwrap_or_default(),
                })
            })
            .transpose()?;
        let status = status_override
            .as_ref()
            .map(|value| value.status)
            .or(carrier_status)
            .unwrap_or_default();
        Ok(Shipment {
            id: parse_id(&self.id)?,
            revision: self.revision,
            title: self.title,
            direction: match self.direction.as_str() {
                "inbound" => Direction::Inbound,
                "outbound" => Direction::Outbound,
                _ => {
                    return Err(Error::Invalid(
                        "stored direction must be inbound or outbound".into(),
                    ));
                }
            },
            merchant: self.merchant,
            order_reference: self.order_reference,
            signature_required: self.signature_required,
            tracking: Tracking {
                carrier: self.tracking_carrier.parse().map_err(
                    |error: crate::carrier::UnsupportedCarrier| {
                        Error::Database(sqlx::Error::Decode(Box::new(error)))
                    },
                )?,
                number: self.tracking_number,
                recipient_url: self.recipient_url,
            },
            status,
            carrier_status,
            carrier_tracking: serde_json::from_str(&self.carrier_tracking)?,
            status_override,
            archived: self.archived,
            created_at: self.created_at,
            updated_at: self.updated_at,
            sources,
            sync: SyncState {
                provider: self.tracking_provider,
                registration_state: match self.registration_state.as_str() {
                    "pending" => RegistrationState::Pending,
                    "registered" => RegistrationState::Registered,
                    "rejected"
                        if self
                            .last_error_code
                            .as_deref()
                            .is_some_and(crate::shipping_api::error::configuration_error_code) =>
                    {
                        RegistrationState::ConfigurationRequired
                    }
                    "rejected" => RegistrationState::Rejected,
                    _ => {
                        return Err(Error::Invalid(
                            "stored registration state is invalid".into(),
                        ));
                    }
                },
                registration_started_at: self.registration_started_at,
                error_code: self.last_error_code,
                provider_status: self.last_provider_status,
                provider_error_code: self.last_provider_error_code,
                tracker_id: self.tracker_id,
                tracking_url: self.tracking_url,
                last_success_at: self.last_success_at,
                last_error: self.last_error,
                next_attempt_at: self.next_attempt_at,
                attempts: self.attempts,
            },
        })
    }
}

fn parse_id(value: &str) -> Result<Uuid, Error> {
    Uuid::parse_str(value).map_err(|error| Error::Database(sqlx::Error::Decode(Box::new(error))))
}

fn valid_text(value: &str, field: &str, max: usize) -> Result<(), Error> {
    if value.trim().is_empty() || value.len() > max || value.chars().any(char::is_control) {
        return Err(Error::Invalid(format!(
            "{field} must contain 1..{max} printable bytes"
        )));
    }
    Ok(())
}

pub fn normalize_tracking<C>(tracking: &mut Tracking<C>) -> Result<(), Error> {
    if tracking.number.trim().is_empty() {
        return Err(Error::TrackingRequired);
    }
    valid_text(&tracking.number, "tracking number", 200)?;
    tracking.number = tracking
        .number
        .chars()
        .filter(|c| !c.is_whitespace())
        .flat_map(char::to_uppercase)
        .collect();
    Ok(())
}

fn normalize_input_tracking(tracking: &mut InputTracking) -> Result<(), Error> {
    normalize_tracking(tracking)?;
    if let Some(url) = &tracking.recipient_url {
        let canonical = crate::shipping_api::doordash::normalize_recipient_url(url)
            .map_err(|_| Error::InvalidRecipientLink)?;
        if tracking
            .carrier
            .is_some_and(|carrier| carrier != Carrier::DoorDash)
        {
            return Err(Error::InvalidRecipientLink);
        }
        tracking.carrier = Some(Carrier::DoorDash);
        tracking.recipient_url = Some(canonical);
    } else if tracking.carrier == Some(Carrier::DoorDash) {
        return Err(Error::RecipientLinkRequired);
    }
    Ok(())
}

fn create_fingerprint(input: &CreateShipment) -> Result<String, Error> {
    #[derive(serde::Serialize)]
    struct FingerprintTracking<'a> {
        carrier: Option<String>,
        number: &'a str,
        #[serde(skip_serializing_if = "Option::is_none")]
        recipient_url: &'a Option<String>,
    }

    #[derive(serde::Serialize)]
    struct Fingerprint<'a> {
        title: &'a str,
        #[serde(skip_serializing_if = "Option::is_none")]
        direction: Option<Direction>,
        merchant: &'a Option<String>,
        order_reference: &'a Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        signature_required: Option<bool>,
        tracking: FingerprintTracking<'a>,
        source: &'a Option<Source>,
    }

    // Existing replays hashed normalized lowercase carriers before the API
    // exposed canonical names. New optional fields must not change old hashes.
    let payload = Fingerprint {
        title: &input.title,
        direction: (input.direction == Direction::Outbound).then_some(input.direction),
        merchant: &input.merchant,
        order_reference: &input.order_reference,
        signature_required: input.signature_required,
        tracking: FingerprintTracking {
            carrier: input.tracking.carrier.as_ref().map(Carrier::as_db_key),
            number: &input.tracking.number,
            recipient_url: &input.tracking.recipient_url,
        },
        source: &input.source,
    };
    Ok(Sha256::digest(serde_json::to_vec(&payload)?)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect())
}

fn validate_source(source: &Source) -> Result<(), Error> {
    valid_text(&source.namespace, "source namespace", 200)?;
    valid_text(&source.key, "source key", 500)
}

pub fn validate_create(input: &mut CreateShipment, key: &str) -> Result<(), Error> {
    valid_text(&input.title, "title", 500)?;
    valid_text(key, "Idempotency-Key", 200)?;
    normalize_input_tracking(&mut input.tracking)?;
    for (name, value) in [
        ("merchant", &input.merchant),
        ("order reference", &input.order_reference),
    ] {
        if let Some(value) = value {
            valid_text(value, name, 500)?;
        }
    }
    if let Some(source) = &input.source {
        validate_source(source)?;
    }
    Ok(())
}

pub fn validate_update(input: &mut UpdateShipment) -> Result<(), Error> {
    if matches!(input.direction.value, Some(None)) {
        return Err(Error::Invalid(
            "direction is required and cannot be cleared".into(),
        ));
    }
    if let Some(title) = &input.title {
        valid_text(title, "title", 500)?;
    }
    for (name, patch) in [
        ("merchant", &input.merchant),
        ("order reference", &input.order_reference),
    ] {
        if let Some(Some(value)) = &patch.value {
            valid_text(value, name, 500)?;
        }
    }
    if let Some(value) = &mut input.tracking.value {
        normalize_input_tracking(value.as_mut().ok_or(Error::TrackingRequired)?)?;
    }
    Ok(())
}

fn unique_error(error: sqlx::Error) -> Error {
    if error
        .as_database_error()
        .is_some_and(|error| error.is_unique_violation())
    {
        Error::Conflict(
            "tracking identity or source already belongs to a shipment; find it before retrying"
                .into(),
        )
    } else {
        error.into()
    }
}

impl Store {
    pub async fn open(path: &Path) -> Result<Self, Error> {
        let options = SqliteConnectOptions::new()
            .filename(path)
            .create_if_missing(true)
            .foreign_keys(true)
            .journal_mode(SqliteJournalMode::Wal)
            .busy_timeout(Duration::from_secs(30));
        let pool = SqlitePoolOptions::new()
            .max_connections(5)
            .connect_with(options)
            .await?;
        sqlx::migrate!().run(&pool).await?;
        Ok(Self { pool })
    }

    pub fn pool(&self) -> &SqlitePool {
        &self.pool
    }

    pub async fn create_agent(
        &self,
        name: &str,
        digest: &[u8; 32],
    ) -> Result<AgentCredential, Error> {
        let name = name.trim();
        valid_text(name, "agent name", 100)?;
        let mut tx = self.transaction().await?;
        // The writer lock makes the credential limit atomic across processes.
        let count: i64 = sqlx::query_scalar("SELECT count(*) FROM agent_credentials")
            .fetch_one(&mut *tx)
            .await?;
        if count >= 32 {
            return Err(Error::Conflict(
                "revoke an agent before creating another (limit 32)".into(),
            ));
        }
        let (id, name, created_at) = sqlx::query_as::<_, (String, String, String)>(
            "INSERT INTO agent_credentials (id, name, token_digest) VALUES (?, ?, ?) RETURNING id, name, created_at",
        )
        .bind(Uuid::now_v7().to_string())
        .bind(name)
        .bind(digest.as_slice())
        .fetch_one(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(AgentCredential {
            id: parse_id(&id)?,
            name,
            created_at,
        })
    }

    pub async fn agents(&self) -> Result<AgentCredentials, Error> {
        let rows = sqlx::query_as::<_, (String, String, String)>(
            "SELECT id, name, created_at FROM agent_credentials ORDER BY id DESC LIMIT 32",
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(AgentCredentials {
            items: rows
                .into_iter()
                .map(|(id, name, created_at)| {
                    Ok(AgentCredential {
                        id: parse_id(&id)?,
                        name,
                        created_at,
                    })
                })
                .collect::<Result<_, Error>>()?,
        })
    }

    pub async fn agent_identity(&self, digest: &[u8; 32]) -> Result<Option<Uuid>, Error> {
        sqlx::query_scalar::<_, String>("SELECT id FROM agent_credentials WHERE token_digest = ?")
            .bind(digest.as_slice())
            .fetch_optional(&self.pool)
            .await?
            .map(|id| parse_id(&id))
            .transpose()
    }

    pub async fn revoke_agent(&self, id: Uuid) -> Result<bool, Error> {
        let deleted = sqlx::query("DELETE FROM agent_credentials WHERE id = ?")
            .bind(id.to_string())
            .execute(&self.pool)
            .await?;
        Ok(deleted.rows_affected() != 0)
    }

    /// Drain callers before closing. Flush committed WAL frames explicitly:
    /// a pool's background connection cleanup can outlive close().await.
    pub async fn close(&self) -> Result<(), Error> {
        let checkpoint = sqlx::query_as::<_, (i64, i64, i64)>("PRAGMA wal_checkpoint(TRUNCATE)")
            .fetch_one(&self.pool)
            .await;
        self.pool.close().await;
        let (busy, _, _) = checkpoint?;
        if busy != 0 {
            return Err(Error::Conflict(
                "SQLite checkpoint was busy during shutdown".into(),
            ));
        }
        Ok(())
    }

    // Acquire the writer lock before reading state so concurrent writers cannot
    // race duplicate detection or turn a read transaction into SQLITE_BUSY.
    async fn transaction(&self) -> Result<Transaction<'_, Sqlite>, Error> {
        Ok(self.pool.begin_with("BEGIN IMMEDIATE").await?)
    }

    pub(crate) async fn get_in(
        tx: &mut Transaction<'_, Sqlite>,
        id: Uuid,
    ) -> Result<Shipment, Error> {
        let row = sqlx::query_as::<_, Record>("SELECT * FROM shipments WHERE id = ?")
            .bind(id.to_string())
            .fetch_optional(&mut **tx)
            .await?
            .ok_or(Error::NotFound)?;
        let sources = sqlx::query_as::<_, (String, String)>(
            "SELECT namespace, key FROM sources WHERE shipment_id = ? ORDER BY namespace, key",
        )
        .bind(id.to_string())
        .fetch_all(&mut **tx)
        .await?
        .into_iter()
        .map(|(namespace, key)| Source { namespace, key })
        .collect();
        row.into_shipment(sources)
    }

    pub async fn get(&self, id: Uuid) -> Result<Shipment, Error> {
        let mut tx = self.pool.begin().await?;
        let result = Self::get_in(&mut tx, id).await?;
        tx.commit().await?;
        Ok(result)
    }

    /// Retry committed creates without another provider call. The same check is
    /// repeated under the writer lock when committing concurrent requests.
    pub async fn replay_create(
        &self,
        input: &CreateShipment,
        actor: &str,
        key: &str,
    ) -> Result<Option<Shipment>, Error> {
        let saved = sqlx::query_as::<_, (String, String)>(
            "SELECT fingerprint, response FROM idempotency WHERE actor_id = ? AND key = ?",
        )
        .bind(actor)
        .bind(key)
        .fetch_optional(&self.pool)
        .await?;
        let Some((fingerprint, response)) = saved else {
            return Ok(None);
        };
        if fingerprint != create_fingerprint(input)? {
            return Err(Error::Conflict(
                "Idempotency-Key was already used for different input".into(),
            ));
        }
        Ok(Some(serde_json::from_str(&response)?))
    }

    pub async fn create(
        &self,
        mut input: CreateShipment,
        actor: &str,
        key: &str,
        verified: Option<&Tracker>,
    ) -> Result<Shipment, Error> {
        validate_create(&mut input, key)?;
        let fingerprint = create_fingerprint(&input)?;
        let mut tx = self.transaction().await?;
        if let Some((saved_hash, response)) = sqlx::query_as::<_, (String, String)>(
            "SELECT fingerprint, response FROM idempotency WHERE actor_id = ? AND key = ?",
        )
        .bind(actor)
        .bind(key)
        .fetch_optional(&mut *tx)
        .await?
        {
            if saved_hash != fingerprint {
                return Err(Error::Conflict(
                    "Idempotency-Key was already used for different input".into(),
                ));
            }
            return Ok(serde_json::from_str(&response)?);
        }
        let id = Uuid::now_v7();
        let tracking = Tracking {
            carrier: verified
                .map(|tracker| tracker.carrier)
                .or(input.tracking.carrier)
                .ok_or(Error::CarrierRequired)?,
            number: input.tracking.number.clone(),
            recipient_url: input.tracking.recipient_url.clone(),
        };
        Self::check_tracking(&mut tx, id, &tracking).await?;
        sqlx::query("INSERT INTO shipments (id,title,direction,merchant,order_reference,signature_required,tracking_carrier,tracking_number,recipient_url,tracking_provider,next_attempt_at) VALUES (?,?,?,?,?,?,?,?,?,?,unixepoch())")
            .bind(id.to_string()).bind(&input.title).bind(input.direction.as_str()).bind(&input.merchant).bind(&input.order_reference)
            .bind(input.signature_required)
            .bind(tracking.carrier.as_db_key()).bind(&tracking.number)
            .bind(&tracking.recipient_url).bind(ProviderId::for_carrier(Some(tracking.carrier)).as_str())
            .execute(&mut *tx).await.map_err(unique_error)?;
        if let Some(tracker) = verified {
            Self::save_verification(&mut tx, id, tracker).await?;
        }
        if let Some(source) = &input.source {
            Self::source_in(&mut tx, id, source, Some((actor, key))).await?;
        }
        Self::event_in(&mut tx, id, EventKind::Created, actor, "Shipment created").await?;
        let shipment = Self::get_in(&mut tx, id).await?;
        sqlx::query("INSERT INTO idempotency (actor_id,key,fingerprint,response) VALUES (?,?,?,?)")
            .bind(actor)
            .bind(key)
            .bind(fingerprint)
            .bind(serde_json::to_string(&shipment)?)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(shipment)
    }

    /// Reject known local conflicts before provider registration. The final
    /// create repeats these checks because this transaction ends before I/O.
    pub async fn preflight_create(
        &self,
        input: &CreateShipment,
        actor: &str,
        key: &str,
    ) -> Result<Option<Shipment>, Error> {
        if let Some(replay) = self.replay_create(input, actor, key).await? {
            return Ok(Some(replay));
        }
        let mut tx = self.transaction().await?;
        if let Some(source) = &input.source {
            let exists: bool = sqlx::query_scalar(
                "SELECT EXISTS(SELECT 1 FROM sources WHERE namespace=? AND key=?)",
            )
            .bind(&source.namespace)
            .bind(&source.key)
            .fetch_one(&mut *tx)
            .await?;
            if exists {
                return Err(Error::Conflict(
                    "source already belongs to a shipment; find it before retrying".into(),
                ));
            }
        }
        if let Some(carrier) = input.tracking.carrier {
            let tracking = Tracking {
                carrier,
                number: input.tracking.number.clone(),
                recipient_url: input.tracking.recipient_url.clone(),
            };
            Self::check_tracking(&mut tx, Uuid::now_v7(), &tracking).await?;
        }
        tx.commit().await?;
        Ok(None)
    }

    pub async fn preflight_update(
        &self,
        id: Uuid,
        revision: i64,
        tracking: &InputTracking,
    ) -> Result<(), Error> {
        let mut tx = self.transaction().await?;
        let current = Self::get_in(&mut tx, id).await?;
        Self::check_revision(&current, revision)?;
        if !current.archived
            && let Some(carrier) = tracking.carrier
        {
            let tracking = Tracking {
                carrier,
                number: tracking.number.clone(),
                recipient_url: tracking.recipient_url.clone(),
            };
            Self::check_tracking(&mut tx, id, &tracking).await?;
        }
        tx.commit().await?;
        Ok(())
    }

    pub async fn update(
        &self,
        id: Uuid,
        revision: i64,
        mut input: UpdateShipment,
        actor: &str,
        verified: Option<&Tracker>,
    ) -> Result<Shipment, Error> {
        validate_update(&mut input)?;
        let mut tx = self.transaction().await?;
        let mut shipment = Self::get_in(&mut tx, id).await?;
        Self::check_revision(&shipment, revision)?;
        if let Some(title) = input.title {
            shipment.title = title;
        }
        if let Some(direction) = input.direction.value {
            shipment.direction = direction.ok_or_else(|| {
                Error::Invalid("direction is required and cannot be cleared".into())
            })?;
        }
        if let Some(value) = input.merchant.value {
            shipment.merchant = value;
        }
        if let Some(value) = input.order_reference.value {
            shipment.order_reference = value;
        }
        let mut changed_tracking = false;
        if let Some(value) = input.tracking.value {
            let mut input_tracking = value.ok_or(Error::TrackingRequired)?;
            normalize_input_tracking(&mut input_tracking)?;
            let tracking = Tracking {
                number: input_tracking.number,
                recipient_url: input_tracking.recipient_url,
                carrier: verified
                    .map(|tracker| tracker.carrier)
                    .or(input_tracking.carrier)
                    .ok_or(Error::CarrierRequired)?,
            };
            changed_tracking = tracking != shipment.tracking;
            shipment.tracking = tracking;
        }
        if changed_tracking {
            shipment.signature_required = None;
        }
        if let Some(value) = input.signature_required.value {
            shipment.signature_required = value;
        }
        if !shipment.archived {
            Self::check_tracking(&mut tx, id, &shipment.tracking).await?;
        }
        sqlx::query("UPDATE shipments SET title=?,direction=?,merchant=?,order_reference=?,signature_required=?,tracking_carrier=?,tracking_number=?,recipient_url=?,tracking_provider=? WHERE id=?")
            .bind(shipment.title).bind(shipment.direction.as_str()).bind(shipment.merchant).bind(shipment.order_reference)
            .bind(shipment.signature_required)
            .bind(shipment.tracking.carrier.as_db_key()).bind(&shipment.tracking.number)
            .bind(&shipment.tracking.recipient_url).bind(ProviderId::for_carrier(Some(shipment.tracking.carrier)).as_str())
            .bind(id.to_string()).execute(&mut *tx).await.map_err(unique_error)?;
        if changed_tracking {
            sqlx::query("UPDATE shipments SET tracking_generation=tracking_generation+1,tracker_id=NULL,tracking_url=NULL,carrier_status=NULL,carrier_tracking='{}',registration_state='pending',registration_started_at=NULL,last_error_code=NULL,last_provider_status=NULL,last_provider_error_code=NULL,status_since=NULL,last_success_at=NULL,last_attempt_at=NULL,last_error=NULL,attempts=0,lease_until=NULL,lease_token=NULL,next_attempt_at=CASE WHEN tracking_number IS NOT NULL AND archived=0 AND ? THEN unixepoch() END WHERE id=?")
                .bind(!shipment.status_override.as_ref().is_some_and(|value|value.status.is_terminal())).bind(id.to_string()).execute(&mut *tx).await?;
        }
        if let Some(tracker) = verified {
            Self::save_verification(&mut tx, id, tracker).await?;
        }
        Self::bump(&mut tx, id).await?;
        Self::event_in(
            &mut tx,
            id,
            EventKind::Updated,
            actor,
            if changed_tracking {
                "Shipment edited; tracking identity replaced"
            } else {
                "Shipment edited"
            },
        )
        .await?;
        let result = Self::get_in(&mut tx, id).await?;
        tx.commit().await?;
        Ok(result)
    }

    async fn save_verification(
        tx: &mut Transaction<'_, Sqlite>,
        id: Uuid,
        tracker: &Tracker,
    ) -> Result<(), Error> {
        sqlx::query(r#"UPDATE shipments SET tracking_provider=?,tracker_id=?,tracking_url=?,carrier_status=?,carrier_tracking=?,registration_state='registered',registration_started_at=NULL,last_error_code=NULL,last_provider_status=NULL,last_provider_error_code=NULL,status_since=strftime('%Y-%m-%dT%H:%M:%fZ','now'),last_success_at=strftime('%Y-%m-%dT%H:%M:%fZ','now'),last_attempt_at=unixepoch(),last_error=NULL,attempts=0,next_attempt_at=CASE WHEN archived=1 OR override_status IN ('"delivered"','"return_to_sender"','"cancelled"') THEN NULL ELSE unixepoch()+? END WHERE id=?"#)
            .bind(tracker.provider.as_str())
            .bind(&tracker.id).bind(&tracker.public_url).bind(serde_json::to_string(&tracker.status)?)
            .bind(serde_json::to_string(&tracker.carrier_tracking)?)
            .bind(crate::sync::poll_delay(tracker.status, 0, &id.to_string())).bind(id.to_string()).execute(&mut **tx).await?;
        Ok(())
    }

    fn check_revision(shipment: &Shipment, revision: i64) -> Result<(), Error> {
        if shipment.revision != revision {
            Err(Error::Precondition)
        } else {
            Ok(())
        }
    }

    async fn check_tracking(
        tx: &mut Transaction<'_, Sqlite>,
        id: Uuid,
        tracking: &Tracking,
    ) -> Result<(), Error> {
        // BEGIN IMMEDIATE keeps the identity check and following write atomic.
        let carrier = tracking.carrier.as_db_key();
        let exists:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM shipments WHERE archived=0 AND tracking_number=? AND tracking_carrier=? AND id<>?)")
            .bind(&tracking.number).bind(&carrier).bind(id.to_string()).fetch_one(&mut **tx).await?;
        if exists {
            Err(Error::Conflict(
                "tracking identity already belongs to an active shipment".into(),
            ))
        } else {
            Ok(())
        }
    }

    async fn bump(tx: &mut Transaction<'_, Sqlite>, id: Uuid) -> Result<(), Error> {
        sqlx::query("UPDATE shipments SET revision=revision+1,updated_at=strftime('%Y-%m-%dT%H:%M:%fZ','now') WHERE id=?")
            .bind(id.to_string()).execute(&mut **tx).await?;
        Ok(())
    }

    async fn source_in(
        tx: &mut Transaction<'_, Sqlite>,
        id: Uuid,
        source: &Source,
        admission: Option<(&str, &str)>,
    ) -> Result<(), Error> {
        let reservation: Option<(String,String)> = sqlx::query_as(
            "SELECT actor_id,operation_key FROM admission_sources WHERE namespace=? AND key=? AND lease_until>unixepoch()",
        ).bind(&source.namespace).bind(&source.key).fetch_optional(&mut **tx).await?;
        if let Some((actor, key)) = reservation
            && admission != Some((actor.as_str(), key.as_str()))
        {
            return Err(Error::Conflict("source is reserved by another shipment verification; retry that request after it completes".into()));
        }
        let existing = sqlx::query_scalar::<_, String>(
            "SELECT shipment_id FROM sources WHERE namespace=? AND key=?",
        )
        .bind(&source.namespace)
        .bind(&source.key)
        .fetch_optional(&mut **tx)
        .await?;
        if let Some(existing) = existing {
            return if existing == id.to_string() {
                Ok(())
            } else {
                Err(Error::Conflict(
                    "source already belongs to another shipment".into(),
                ))
            };
        }
        sqlx::query("INSERT INTO sources (namespace,key,shipment_id) VALUES (?,?,?)")
            .bind(&source.namespace)
            .bind(&source.key)
            .bind(id.to_string())
            .execute(&mut **tx)
            .await
            .map_err(unique_error)?;
        Ok(())
    }

    pub async fn add_source(
        &self,
        id: Uuid,
        revision: i64,
        source: Source,
        actor: &str,
    ) -> Result<Shipment, Error> {
        validate_source(&source)?;
        let mut tx = self.transaction().await?;
        let current = Self::get_in(&mut tx, id).await?;
        Self::check_revision(&current, revision)?;
        if !current.sources.contains(&source) {
            Self::source_in(&mut tx, id, &source, None).await?;
            Self::bump(&mut tx, id).await?;
            Self::event_in(
                &mut tx,
                id,
                EventKind::SourceAdded,
                actor,
                "Source attached",
            )
            .await?;
        }
        let result = Self::get_in(&mut tx, id).await?;
        tx.commit().await?;
        Ok(result)
    }

    pub async fn set_override(
        &self,
        id: Uuid,
        revision: i64,
        value: Option<SetOverride>,
        actor: &str,
    ) -> Result<Shipment, Error> {
        if let Some(value) = &value {
            valid_text(&value.reason, "reason", 1000)?;
        }
        let mut tx = self.transaction().await?;
        let current = Self::get_in(&mut tx, id).await?;
        Self::check_revision(&current, revision)?;
        let terminal = value
            .as_ref()
            .map(|value| value.status)
            .or(current.carrier_status)
            .is_some_and(Status::is_terminal);
        sqlx::query("UPDATE shipments SET override_status=?,override_reason=?,override_actor=?,override_at=CASE WHEN ? THEN strftime('%Y-%m-%dT%H:%M:%fZ','now') END,
            last_error=CASE WHEN ? THEN NULL ELSE last_error END,
            last_error_code=CASE WHEN ? THEN NULL ELSE last_error_code END,
            last_provider_status=CASE WHEN ? THEN NULL ELSE last_provider_status END,
            last_provider_error_code=CASE WHEN ? THEN NULL ELSE last_provider_error_code END,
            attempts=CASE WHEN ? THEN 0 ELSE attempts END,
            registration_state=CASE WHEN ? THEN CASE WHEN tracker_id IS NULL THEN 'pending' ELSE 'registered' END ELSE registration_state END,
            next_attempt_at=CASE WHEN ? THEN NULL WHEN tracking_number IS NOT NULL AND archived=0 AND registration_state!='rejected' THEN unixepoch() ELSE NULL END WHERE id=?")
            .bind(value.as_ref().map(|v|serde_json::to_string(&v.status)).transpose()?).bind(value.as_ref().map(|v|&v.reason))
            .bind(value.as_ref().map(|_|actor)).bind(value.is_some())
            .bind(terminal).bind(terminal).bind(terminal).bind(terminal).bind(terminal).bind(terminal).bind(terminal)
            .bind(id.to_string()).execute(&mut *tx).await?;
        Self::bump(&mut tx, id).await?;
        Self::event_in(
            &mut tx,
            id,
            if value.is_some() {
                EventKind::OverrideSet
            } else {
                EventKind::OverrideCleared
            },
            actor,
            value
                .as_ref()
                .map(|v| v.reason.as_str())
                .unwrap_or("Manual correction cleared"),
        )
        .await?;
        let result = Self::get_in(&mut tx, id).await?;
        tx.commit().await?;
        Ok(result)
    }

    pub async fn archive(
        &self,
        id: Uuid,
        revision: i64,
        archived: bool,
        actor: &str,
    ) -> Result<Shipment, Error> {
        let mut tx = self.transaction().await?;
        let shipment = Self::get_in(&mut tx, id).await?;
        Self::check_revision(&shipment, revision)?;
        if !archived {
            Self::check_tracking(&mut tx, id, &shipment.tracking).await?;
        }
        sqlx::query("UPDATE shipments SET archived=?,lease_until=NULL,lease_token=NULL,next_attempt_at=CASE WHEN ?=0 AND tracking_number IS NOT NULL AND ? AND registration_state!='rejected' THEN unixepoch() END WHERE id=?")
            .bind(archived).bind(archived).bind(!shipment.status.is_terminal()).bind(id.to_string()).execute(&mut *tx).await.map_err(unique_error)?;
        Self::bump(&mut tx, id).await?;
        Self::event_in(
            &mut tx,
            id,
            if archived {
                EventKind::Archived
            } else {
                EventKind::Restored
            },
            actor,
            if archived {
                "Shipment archived"
            } else {
                "Shipment restored"
            },
        )
        .await?;
        let result = Self::get_in(&mut tx, id).await?;
        tx.commit().await?;
        Ok(result)
    }

    pub async fn refresh(&self, id: Uuid, revision: i64, actor: &str) -> Result<Shipment, Error> {
        let mut tx = self.transaction().await?;
        let shipment = Self::get_in(&mut tx, id).await?;
        Self::check_revision(&shipment, revision)?;
        if shipment.archived {
            return Err(Error::Conflict(
                "refresh requires an unarchived shipment with tracking".into(),
            ));
        }
        if shipment.sync.registration_state == RegistrationState::Rejected {
            return Err(Error::Conflict(
                "Tracking registration needs attention. Review the shipment's provider error and carrier support before retrying or changing its tracking number.".into(),
            ));
        }
        // Refreshes coalesce without disturbing an in-flight check or a retry.
        // An earlier scheduled check stays earlier; otherwise wait at least one
        // minute after the last provider attempt, even for terminal shipments.
        sqlx::query(
            "UPDATE shipments SET registration_state=CASE WHEN ? THEN 'pending' ELSE registration_state END,next_attempt_at=CASE
                 WHEN lease_until>unixepoch()
                   OR (last_error IS NOT NULL AND next_attempt_at>unixepoch())
                     THEN next_attempt_at
                 WHEN next_attempt_at>unixepoch()
                     THEN min(next_attempt_at,max(unixepoch(),coalesce(last_attempt_at+60,unixepoch())))
                 ELSE max(unixepoch(),coalesce(last_attempt_at+60,unixepoch()))
             END WHERE id=?",
        )
        .bind(shipment.sync.registration_state == RegistrationState::ConfigurationRequired)
        .bind(id.to_string())
        .execute(&mut *tx)
        .await?;
        Self::event_in(
            &mut tx,
            id,
            EventKind::RefreshRequested,
            actor,
            "Tracking refresh queued",
        )
        .await?;
        let result = Self::get_in(&mut tx, id).await?;
        tx.commit().await?;
        Ok(result)
    }

    pub async fn list(
        &self,
        filters: &Filters,
        after: Option<ShipmentPosition>,
        limit: u32,
    ) -> Result<Vec<ShipmentListEntry>, Error> {
        if filters.source_namespace.is_some() != filters.source_key.is_some() {
            return Err(Error::Invalid(
                "source_namespace and source_key must be supplied together".into(),
            ));
        }
        let delivery_window = crate::delivery::DeliveryWindow::from_bounds(
            filters.delivered_after,
            filters.delivered_before,
        )?;
        // Date keys are indexed generated columns. Every page uses the key
        // captured when the preceding row was selected, never a mutable lookup.
        let sort_column = match filters.sort.unwrap_or_default() {
            ShipmentSort::Id => None,
            ShipmentSort::Expected => Some("expected_sort_key"),
            ShipmentSort::Delivered if delivery_window.is_some() => {
                Some("-unixepoch(json_extract(carrier_tracking,'$.delivered_at'))")
            }
            ShipmentSort::Delivered => Some("delivered_sort_key"),
        };
        let mut query = QueryBuilder::<Sqlite>::new("SELECT shipments.*,");
        query.push(sort_column.unwrap_or("NULL"));
        query.push(" AS list_sort_key FROM shipments WHERE archived=");
        query.push_bind(filters.archived.unwrap_or(false));
        if let Some(direction) = filters.direction {
            query.push(" AND direction=").push_bind(direction.as_str());
        }
        if let Some(issues) = filters.tracking_issues {
            query.push(if issues {
                " AND last_error IS NOT NULL"
            } else {
                " AND last_error IS NULL"
            });
        }
        if let Some(window) = delivery_window {
            query
                .push(" AND unixepoch(json_extract(carrier_tracking,'$.delivered_at'))>=")
                .push_bind(window.after())
                .push(" AND unixepoch(json_extract(carrier_tracking,'$.delivered_at'))<")
                .push_bind(window.before());
        }
        if let Some(position) = after {
            if let Some(column) = sort_column {
                query
                    .push(" AND (")
                    .push(column)
                    .push(",id)>(")
                    .push_bind(position.sort_key.unwrap_or(i64::MAX))
                    .push(",")
                    .push_bind(position.id.to_string())
                    .push(")");
            } else {
                query.push(" AND id>").push_bind(position.id.to_string());
            }
        }
        if let Some(value) = &filters.tracking_number {
            let mut tracking = InputTracking {
                number: value.clone(),
                carrier: None,
                recipient_url: None,
            };
            normalize_tracking(&mut tracking)?;
            query
                .push(" AND tracking_number=")
                .push_bind(tracking.number);
        }
        if let Some(value) = &filters.carrier {
            query
                .push(" AND tracking_carrier=")
                .push_bind(value.as_db_key());
        }
        if let Some(value) = &filters.order_reference {
            query.push(" AND order_reference=").push_bind(value);
        }
        if let (Some(namespace), Some(key)) = (&filters.source_namespace, &filters.source_key) {
            query
                .push(" AND id=(SELECT shipment_id FROM sources WHERE namespace=")
                .push_bind(namespace)
                .push(" AND key=")
                .push_bind(key)
                .push(")");
        }
        if let Some(value) = &filters.search {
            valid_text(value, "search", 500)?;
            query
                .push(" AND (instr(lower(title),lower(")
                .push_bind(value)
                .push("))>0 OR instr(lower(coalesce(merchant,'')),lower(")
                .push_bind(value)
                .push("))>0 OR instr(lower(coalesce(order_reference,'')),lower(")
                .push_bind(value)
                .push("))>0)");
        }
        if let Some(active) = filters.active {
            query.push(" AND coalesce(override_status,carrier_status,'\"expected\"')");
            query.push(if active { " NOT IN (" } else { " IN (" });
            {
                let mut terminal = query.separated(",");
                for status in Status::TERMINAL {
                    terminal.push_bind(serde_json::to_string(&status)?);
                }
            }
            query.push(")");
        }
        if let Some(value) = filters.status {
            query
                .push(" AND coalesce(override_status,carrier_status,'\"expected\"')=")
                .push_bind(serde_json::to_string(&value)?);
        }
        if let Some(exceptions) = filters.exceptions {
            query.push(" AND coalesce(override_status,carrier_status,'\"expected\"')");
            query.push(if exceptions { " IN (" } else { " NOT IN (" });
            {
                let mut statuses = query.separated(",");
                for status in [Status::Failure, Status::Error, Status::ReturnToSender] {
                    statuses.push_bind(serde_json::to_string(&status)?);
                }
            }
            query.push(")");
        }
        query.push(" ORDER BY ");
        if let Some(column) = sort_column {
            query.push(column).push(",");
        }
        query.push("id LIMIT ").push_bind(limit.min(201));
        #[derive(FromRow)]
        struct ListRecord {
            #[sqlx(flatten)]
            record: Record,
            list_sort_key: Option<i64>,
        }
        let mut tx = self.pool.begin().await?;
        let rows = query
            .build_query_as::<ListRecord>()
            .fetch_all(&mut *tx)
            .await?;
        let mut sources_by_id: HashMap<String, Vec<Source>> = HashMap::new();
        if !rows.is_empty() {
            let mut source_query = QueryBuilder::<Sqlite>::new(
                "SELECT shipment_id,namespace,key FROM sources WHERE shipment_id IN (",
            );
            {
                let mut separated = source_query.separated(",");
                for row in &rows {
                    separated.push_bind(&row.record.id);
                }
            }
            source_query.push(") ORDER BY namespace,key");
            for (id, namespace, key) in source_query
                .build_query_as::<(String, String, String)>()
                .fetch_all(&mut *tx)
                .await?
            {
                sources_by_id
                    .entry(id)
                    .or_default()
                    .push(Source { namespace, key });
            }
        }
        let result = rows
            .into_iter()
            .map(|row| {
                let sources = sources_by_id.remove(&row.record.id).unwrap_or_default();
                let shipment = row.record.into_shipment(sources)?;
                Ok(ShipmentListEntry {
                    position: ShipmentPosition {
                        id: shipment.id,
                        sort_key: row.list_sort_key,
                    },
                    shipment,
                })
            })
            .collect();
        tx.commit().await?;
        result
    }

    pub async fn events(&self, id: Uuid, after: i64, limit: u32) -> Result<Vec<Event>, Error> {
        if !sqlx::query_scalar::<_, bool>("SELECT EXISTS(SELECT 1 FROM shipments WHERE id=?)")
            .bind(id.to_string())
            .fetch_one(&self.pool)
            .await?
        {
            return Err(Error::NotFound);
        }
        sqlx::query_as::<_,(i64,String,String,String,String,String,Option<String>)>("SELECT id,shipment_id,kind,actor_id,at,detail,snapshot FROM events WHERE shipment_id=? AND id>? ORDER BY id LIMIT ?")
            .bind(id.to_string()).bind(after).bind(limit.min(201)).fetch_all(&self.pool).await?.into_iter().map(|(id,shipment_id,kind,actor_id,at,detail,snapshot)|
                Ok(Event {id,shipment_id:parse_id(&shipment_id)?,kind:serde_json::from_str(&kind)?,actor_id,at,detail,snapshot:snapshot.as_deref().map(serde_json::from_str).transpose()?})).collect()
    }

    /// Page active inbound estimates without loading carrier scan history.
    pub async fn delivery_estimates(
        &self,
        after: Option<DeliveryEstimatePosition>,
        limit: u32,
    ) -> Result<Vec<DeliveryEstimateEntry>, Error> {
        #[derive(FromRow)]
        struct EstimateRow {
            id: String,
            title: String,
            tracking_carrier: Option<String>,
            status: String,
            estimated_delivery_at: String,
            expected_sort_key: i64,
        }
        let mut query = delivery_estimates_query(after, limit);
        query
            .build_query_as::<EstimateRow>()
            .fetch_all(&self.pool)
            .await?
            .into_iter()
            .map(|row| {
                let id = parse_id(&row.id)?;
                Ok(DeliveryEstimateEntry {
                    estimate: DeliveryEstimate {
                        id,
                        title: row.title,
                        direction: Direction::Inbound,
                        carrier: row
                            .tracking_carrier
                            .map(|value| {
                                value
                                    .parse::<Carrier>()
                                    .map_err(|_| Error::Invalid("invalid stored carrier".into()))
                            })
                            .transpose()?,
                        status: serde_json::from_str(&row.status)?,
                        estimated_delivery_at: row.estimated_delivery_at,
                    },
                    position: DeliveryEstimatePosition {
                        id,
                        sort_key: row.expected_sort_key,
                    },
                })
            })
            .collect()
    }

    /// With no cursor, establish a tail checkpoint without replaying history.
    pub async fn state_events(
        &self,
        after: Option<i64>,
        limit: u32,
    ) -> Result<StateEventPage, Error> {
        if !(1..=200).contains(&limit) || after.is_some_and(|value| value < 0) {
            return Err(Error::Invalid("invalid event cursor or limit".into()));
        }
        let mut transaction = self.pool.begin().await?;
        let head: i64 = sqlx::query_scalar("SELECT coalesce(max(id),0) FROM state_events")
            .fetch_one(&mut *transaction)
            .await?;
        let Some(after) = after else {
            transaction.commit().await?;
            return Ok(StateEventPage {
                items: Vec::new(),
                next_after: head,
                has_more: false,
            });
        };
        if after > head {
            return Err(Error::Conflict("event checkpoint is ahead of the restored history; establish a new tail checkpoint".into()));
        }
        #[derive(FromRow)]
        struct StateRow {
            id: i64,
            shipment_id: String,
            event_type: String,
            status: String,
            title: String,
            carrier: Option<String>,
            direction: String,
            at: String,
        }
        let mut rows: Vec<StateRow> = sqlx::query_as(
            "SELECT id, shipment_id, event_type, status, title, carrier, direction, at
             FROM state_events WHERE id > ? ORDER BY id LIMIT ?",
        )
        .bind(after)
        .bind(i64::from(limit) + 1)
        .fetch_all(&mut *transaction)
        .await?;
        let has_more = rows.len() > limit as usize;
        rows.truncate(limit as usize);
        let next_after = rows.last().map_or(after, |row| row.id);
        let items = rows
            .into_iter()
            .map(|row| {
                Ok(StateEvent {
                    id: row.id,
                    shipment_id: parse_id(&row.shipment_id)?,
                    event_type: match row.event_type.as_str() {
                        "out_for_delivery" => StateEventType::OutForDelivery,
                        "delivered" => StateEventType::Delivered,
                        "exception" => StateEventType::Exception,
                        _ => return Err(Error::Invalid("invalid stored event type".into())),
                    },
                    status: serde::Deserialize::deserialize(serde::de::value::StrDeserializer::<
                        serde::de::value::Error,
                    >::new(&row.status))
                    .map_err(|_| Error::Invalid("invalid stored status".into()))?,
                    title: row.title,
                    carrier: row
                        .carrier
                        .map(|value| {
                            value
                                .parse::<Carrier>()
                                .map_err(|_| Error::Invalid("invalid stored carrier".into()))
                        })
                        .transpose()?,
                    direction: match row.direction.as_str() {
                        "inbound" => Direction::Inbound,
                        "outbound" => Direction::Outbound,
                        _ => return Err(Error::Invalid("invalid stored direction".into())),
                    },
                    at: row.at,
                })
            })
            .collect::<Result<Vec<_>, Error>>()?;
        transaction.commit().await?;
        Ok(StateEventPage {
            items,
            next_after,
            has_more,
        })
    }

    pub async fn summary(&self, direction: Option<Direction>) -> Result<Summary, Error> {
        self.summary_in_window(direction, None).await
    }

    pub async fn summary_in_window(
        &self,
        direction: Option<Direction>,
        window: Option<crate::delivery::DeliveryWindow>,
    ) -> Result<Summary, Error> {
        let mut query = QueryBuilder::<Sqlite>::new(
            "SELECT coalesce(override_status,carrier_status,'\"expected\"') AS status,count(*),count(last_error) FROM shipments WHERE archived=0",
        );
        if let Some(direction) = direction {
            query.push(" AND direction=").push_bind(direction.as_str());
        }
        if let Some(window) = window.as_ref() {
            query
                .push(" AND unixepoch(json_extract(carrier_tracking,'$.delivered_at'))>=")
                .push_bind(window.after())
                .push(" AND unixepoch(json_extract(carrier_tracking,'$.delivered_at'))<")
                .push_bind(window.before());
        }
        query.push(" GROUP BY status");
        let rows = query
            .build_query_as::<(String, i64, i64)>()
            .fetch_all(&self.pool)
            .await?;
        let mut summary = Summary::default();
        for (status, count, sync_errors) in rows {
            summary.sync_errors += sync_errors;
            let status: Status = serde_json::from_str(&status)?;
            if window.is_some() {
                // The window is based on an actual delivery scan, independent
                // of a subsequent manual status correction.
                summary.delivered += count;
            }
            if !status.is_terminal() {
                summary.active += count;
            }
            match status {
                Status::Expected | Status::Unknown | Status::PreTransit => {
                    summary.expected += count
                }
                Status::InTransit | Status::AvailableForPickup => summary.in_transit += count,
                Status::OutForDelivery => summary.out_for_delivery += count,
                Status::Delivered if window.is_none() => summary.delivered += count,
                Status::Failure | Status::Error | Status::ReturnToSender => {
                    summary.exceptions += count
                }
                _ => {}
            }
        }
        Ok(summary)
    }

    pub async fn system(&self, provider_enabled: bool) -> Result<System, Error> {
        let (pending_syncs,sync_errors)=sqlx::query_as("SELECT count(CASE WHEN next_attempt_at IS NOT NULL THEN 1 END),count(last_error) FROM shipments WHERE archived=0").fetch_one(&self.pool).await?;
        Ok(System {
            version: env!("CARGO_PKG_VERSION").into(),
            provider_enabled,
            enabled_providers: Vec::new(),
            pending_syncs,
            sync_errors,
            credential_role: String::new(),
            delivery_day: None,
        })
    }

    async fn event_in(
        tx: &mut Transaction<'_, Sqlite>,
        id: Uuid,
        kind: EventKind,
        actor: &str,
        detail: &str,
    ) -> Result<(), Error> {
        let snapshot = Self::get_in(tx, id).await?;
        sqlx::query(
            "INSERT INTO events (shipment_id,kind,actor_id,detail,snapshot) VALUES (?,?,?,?,?)",
        )
        .bind(id.to_string())
        .bind(serde_json::to_string(&kind)?)
        .bind(actor)
        .bind(detail)
        .bind(serde_json::to_string(&snapshot)?)
        .execute(&mut **tx)
        .await?;
        Ok(())
    }
}

fn delivery_estimates_query(
    after: Option<DeliveryEstimatePosition>,
    limit: u32,
) -> QueryBuilder<Sqlite> {
    let mut query = QueryBuilder::new(
        r#"SELECT id, title, tracking_carrier,
                  coalesce(override_status, carrier_status, '"expected"') AS status,
                  json_extract(carrier_tracking, '$.estimated_delivery_at') AS estimated_delivery_at,
                  expected_sort_key
           FROM shipments
           WHERE archived = 0 AND direction = 'inbound'
             AND expected_sort_key < 9223372036854775807
             AND coalesce(override_status, carrier_status, '"expected"')
                 NOT IN ('"delivered"', '"return_to_sender"', '"cancelled"')"#,
    );
    if let Some(after) = after {
        query
            .push(" AND (expected_sort_key,id)>(")
            .push_bind(after.sort_key)
            .push(",")
            .push_bind(after.id.to_string())
            .push(")");
    }
    query
        .push(" ORDER BY expected_sort_key,id LIMIT ")
        .push_bind(limit.min(201));
    query
}
