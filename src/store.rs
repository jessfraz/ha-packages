use crate::{
    carrier::Carrier,
    error::Error,
    installation::{Check, CheckStatus},
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
struct ProviderCheckRecord {
    status: CheckStatus,
    message: String,
    checked_at: i64,
}

impl ProviderCheckRecord {
    fn into_check(self) -> Result<Check, Error> {
        Ok(Check {
            status: self.status,
            message: self.message,
            checked_at: Some(u64::try_from(self.checked_at).map_err(|_| {
                Error::Invalid("stored provider check timestamp is invalid".into())
            })?),
        })
    }
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
        let mut carrier_tracking: CarrierTracking = serde_json::from_str(&self.carrier_tracking)?;
        let original_estimate = carrier_tracking.estimated_delivery_at.clone();
        carrier_tracking.estimated_delivery_at = crate::delivery::legacy_estimate(
            carrier_tracking.estimated_delivery_at,
            carrier_tracking.estimated_delivery_raw.as_deref(),
            self.tracking_provider == ProviderId::EasyPost,
        );
        if carrier_tracking.estimated_delivery_at != original_estimate {
            carrier_tracking.estimated_delivery_raw = original_estimate;
        }
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
            carrier_tracking,
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

pub fn validate_admission(tracking: &InputTracking) -> Result<(), Error> {
    let alphanumeric = tracking
        .number
        .bytes()
        .filter(u8::is_ascii_alphanumeric)
        .count();
    if !(6..=100).contains(&alphanumeric)
        || tracking.number.len() > 100
        || !tracking
            .number
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
        || !tracking.number.bytes().any(|byte| byte.is_ascii_digit())
    {
        return Err(Error::TrackingInvalid);
    }
    Ok(())
}

pub fn normalize_tracking<C>(tracking: &mut Tracking<C>) -> Result<(), Error> {
    normalize_number(&mut tracking.number)
}

fn normalize_number(number: &mut String) -> Result<(), Error> {
    if number.trim().is_empty() {
        return Err(Error::TrackingRequired);
    }
    valid_text(number, "tracking number", 200)?;
    *number = number
        .chars()
        .filter(|c| !c.is_whitespace())
        .flat_map(char::to_uppercase)
        .collect();
    Ok(())
}

fn normalize_input_tracking(tracking: &mut InputTracking) -> Result<(), Error> {
    normalize_number(&mut tracking.number)?;
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

    pub async fn provider_check(
        &self,
        provider: ProviderId,
        fingerprint: &[u8; 32],
    ) -> Result<Option<Check>, Error> {
        sqlx::query_as::<_, ProviderCheckRecord>(
            r#"SELECT status, message, checked_at
               FROM provider_connection_checks
               WHERE provider = ? AND fingerprint = ?"#,
        )
        .bind(provider)
        .bind(fingerprint.as_slice())
        .fetch_optional(&self.pool)
        .await?
        .map(ProviderCheckRecord::into_check)
        .transpose()
    }

    pub async fn save_provider_check(
        &self,
        provider: ProviderId,
        fingerprint: &[u8; 32],
        check: &Check,
    ) -> Result<Check, Error> {
        sqlx::query_as::<_, ProviderCheckRecord>(
            r#"INSERT INTO provider_connection_checks
                   (provider, fingerprint, status, message, checked_at)
               VALUES (?, ?, ?, ?, unixepoch())
               ON CONFLICT(provider) DO UPDATE SET
                   fingerprint = excluded.fingerprint,
                   status = excluded.status,
                   message = excluded.message,
                   checked_at = excluded.checked_at
               RETURNING status, message, checked_at"#,
        )
        .bind(provider)
        .bind(fingerprint.as_slice())
        .bind(&check.status)
        .bind(&check.message)
        .fetch_one(&self.pool)
        .await?
        .into_check()
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
        let mut tx = self.transaction().await?;
        let shipment = Self::create_in(&mut tx, input, actor, key, verified).await?;
        tx.commit().await?;
        Ok(shipment)
    }

    async fn create_in(
        tx: &mut Transaction<'_, Sqlite>,
        input: CreateShipment,
        actor: &str,
        key: &str,
        verified: Option<&Tracker>,
    ) -> Result<Shipment, Error> {
        let fingerprint = create_fingerprint(&input)?;
        if let Some((saved_hash, response)) = sqlx::query_as::<_, (String, String)>(
            "SELECT fingerprint, response FROM idempotency WHERE actor_id = ? AND key = ?",
        )
        .bind(actor)
        .bind(key)
        .fetch_optional(&mut **tx)
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
        Self::check_tracking_number(tx, id, &tracking.number).await?;
        sqlx::query("INSERT INTO shipments (id,title,direction,merchant,order_reference,signature_required,tracking_carrier,tracking_number,recipient_url,tracking_provider,next_attempt_at) VALUES (?,?,?,?,?,?,?,?,?,?,unixepoch())")
            .bind(id.to_string()).bind(&input.title).bind(input.direction.as_str()).bind(&input.merchant).bind(&input.order_reference)
            .bind(input.signature_required)
            .bind(tracking.carrier.as_db_key()).bind(&tracking.number)
            .bind(&tracking.recipient_url).bind(ProviderId::for_carrier(Some(tracking.carrier)).as_str())
            .execute(&mut **tx).await.map_err(unique_error)?;
        if let Some(tracker) = verified {
            Self::save_verification(tx, id, tracker).await?;
        }
        if let Some(source) = &input.source {
            Self::source_in(tx, id, source, Some((actor, key))).await?;
        }
        Self::event_in(tx, id, EventKind::Created, actor, "Shipment created").await?;
        let shipment = Self::get_in(tx, id).await?;
        sqlx::query("INSERT INTO idempotency (actor_id,key,fingerprint,response) VALUES (?,?,?,?)")
            .bind(actor)
            .bind(key)
            .bind(fingerprint)
            .bind(serde_json::to_string(&shipment)?)
            .execute(&mut **tx)
            .await?;
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
        Self::check_tracking_number(&mut tx, Uuid::now_v7(), &input.tracking.number).await?;
        tx.commit().await?;
        Ok(None)
    }

    pub async fn pause_amazon(&self) -> Result<(), Error> {
        let mut tx = self.transaction().await?;
        sqlx::query("UPDATE shipments SET tracking_generation=tracking_generation+1,next_attempt_at=NULL,lease_token=NULL,lease_until=NULL WHERE tracking_provider='amazon_account'")
            .execute(&mut *tx).await?;
        sqlx::query("UPDATE amazon_discovery SET lease_token=NULL,lease_until=NULL WHERE id=1")
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(())
    }

    pub async fn resume_amazon(&self, scope: &str) -> Result<(), Error> {
        // The scope is generated by the authenticated client, never a public source.
        if scope.len() != 65
            || !scope.ends_with(':')
            || !scope.as_bytes()[..64].iter().all(u8::is_ascii_hexdigit)
        {
            return Err(Error::Invalid("Invalid Amazon account scope".into()));
        }
        sqlx::query(r#"UPDATE shipments SET tracking_generation=tracking_generation+1,lease_token=NULL,lease_until=NULL,next_attempt_at=unixepoch(),attempts=0,last_error=NULL,last_error_code=NULL,last_provider_status=NULL,last_provider_error_code=NULL,registration_state='registered' WHERE tracking_provider='amazon_account' AND substr(tracker_id,1,65)=? AND archived=0 AND coalesce(override_status,carrier_status,'') NOT IN ('"delivered"','"return_to_sender"','"cancelled"')"#)
            .bind(scope).execute(&self.pool).await?;
        Ok(())
    }

    /// Commit a verified Amazon parcel and its identity together. Public source
    /// provenance is never treated as authority to change a tracking provider.
    pub async fn import_amazon(
        &self,
        parcel: &crate::amazon::AmazonParcel,
        discovery_lease: Option<&str>,
    ) -> Result<Shipment, Error> {
        self.store_amazon(parcel, discovery_lease, None).await
    }

    /// Owner-reviewed adoption cannot create a shipment or race a tracking edit.
    pub async fn adopt_amazon(
        &self,
        id: Uuid,
        revision: i64,
        parcel: &crate::amazon::AmazonParcel,
    ) -> Result<Shipment, Error> {
        self.store_amazon(parcel, None, Some((id, revision))).await
    }

    async fn store_amazon(
        &self,
        parcel: &crate::amazon::AmazonParcel,
        discovery_lease: Option<&str>,
        expected: Option<(Uuid, i64)>,
    ) -> Result<Shipment, Error> {
        if parcel.tracker.provider != ProviderId::AmazonAccount
            || parcel.tracker.status == Status::Unknown
            || parcel.tracker.carrier == Carrier::AmazonMcf
        {
            return Err(Error::Invalid("Amazon observation is not verified".into()));
        }
        let source = Source {
            namespace: "amazon-account".into(),
            key: parcel.source_key.clone(),
        };
        let mut input = CreateShipment {
            title: parcel.title.clone(),
            direction: Direction::Inbound,
            merchant: Some("Amazon".into()),
            order_reference: Some(parcel.order_reference.clone()),
            signature_required: None,
            tracking: InputTracking {
                carrier: Some(parcel.tracker.carrier),
                number: parcel.tracker.tracking_code.clone(),
                recipient_url: None,
            },
            source: Some(source.clone()),
        };
        let key: String = Sha256::digest(parcel.source_key.as_bytes())
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        validate_create(&mut input, &key)?;
        validate_admission(&input.tracking)?;
        if input.tracking.number != parcel.tracker.tracking_code {
            return Err(Error::Invalid(
                "Amazon tracking identity is not canonical".into(),
            ));
        }
        let mut tx = self.transaction().await?;
        if let Some(lease) = discovery_lease {
            let owned: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM amazon_discovery WHERE id=1 AND lease_token=? AND lease_until>unixepoch())")
                .bind(lease).fetch_one(&mut *tx).await?;
            if !owned {
                return Err(Error::Conflict("Amazon discovery lease expired".into()));
            }
        }
        let mapped: Option<(String, String)> = sqlx::query_as(
            "SELECT shipment_id,tracking_number FROM amazon_parcels WHERE source_key=?",
        )
        .bind(&source.key)
        .fetch_optional(&mut *tx)
        .await?;
        let existing: Option<String> =
            sqlx::query_scalar("SELECT id FROM shipments WHERE tracking_number=?")
                .bind(&input.tracking.number)
                .fetch_optional(&mut *tx)
                .await?;
        if let Some((id, _)) = expected
            && existing.as_deref() != Some(id.to_string().as_str())
        {
            return Err(Error::Conflict(
                "Verified Amazon tracking does not match the selected shipment".into(),
            ));
        }
        if let Some((id, number)) = &mapped
            && (number != &input.tracking.number || existing.as_ref() != Some(id))
        {
            return Err(Error::Conflict(
                "Amazon parcel identity changed; review before relinking".into(),
            ));
        }
        let shipment = if let Some(id) = existing {
            let id = parse_id(&id)?;
            let current = Self::get_in(&mut tx, id).await?;
            if let Some((_, revision)) = expected {
                Self::check_revision(&current, revision)?;
                if current.archived
                    || current.sync.provider != ProviderId::AfterShip
                    || current.tracking.carrier != Carrier::AmazonMcf
                    || current.order_reference.as_deref() != Some(&parcel.order_reference)
                {
                    return Err(Error::Conflict(
                        "Select an active AfterShip Amazon shipment with its matching order".into(),
                    ));
                }
            }
            let correct_account_carrier = current.tracking.carrier == Carrier::AmazonMcf
                && parcel.tracker.carrier == Carrier::AmazonAccount
                && current.sync.provider == ProviderId::AfterShip
                && current.order_reference.as_deref() == Some(&parcel.order_reference);
            if current.direction != Direction::Inbound
                || (current.tracking.carrier != parcel.tracker.carrier && !correct_account_carrier)
            {
                return Err(Error::Conflict(
                    "Existing shipment does not match the verified Amazon parcel".into(),
                ));
            }
            let add_source = !current.sources.contains(&source);
            Self::source_in(&mut tx, id, &source, None).await?;
            let adopt = !current.archived
                && current.tracking.carrier == Carrier::AmazonMcf
                && current.sync.provider == ProviderId::AfterShip;
            if adopt {
                let mut tracker = parcel.tracker.clone();
                let mut history = current.carrier_tracking.clone();
                history.merge(&tracker.carrier_tracking);
                tracker.carrier_tracking = history;
                sqlx::query("UPDATE shipments SET tracking_carrier=?,tracking_generation=tracking_generation+1,lease_token=NULL,lease_until=NULL WHERE id=?")
                    .bind(tracker.carrier.as_db_key()).bind(id.to_string()).execute(&mut *tx).await?;
                Self::save_verification(&mut tx, id, &tracker).await?;
            }
            if add_source || adopt {
                Self::bump(&mut tx, id).await?;
                Self::event_in(
                    &mut tx,
                    id,
                    EventKind::Updated,
                    "amazon_account",
                    "Verified Amazon parcel linked",
                )
                .await?;
            }
            Self::get_in(&mut tx, id).await?
        } else {
            Self::create_in(
                &mut tx,
                input,
                "amazon_account",
                &key,
                Some(&parcel.tracker),
            )
            .await?
        };
        sqlx::query("INSERT INTO amazon_parcels(source_key,shipment_id,tracking_number) VALUES(?,?,?) ON CONFLICT(source_key) DO NOTHING")
            .bind(&source.key).bind(shipment.id.to_string()).bind(&shipment.tracking.number)
            .execute(&mut *tx).await?;
        tx.commit().await?;
        Ok(shipment)
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
        Self::check_tracking_number(&mut tx, id, &tracking.number).await?;
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
        Self::check_tracking_number(&mut tx, id, &shipment.tracking.number).await?;
        sqlx::query("UPDATE shipments SET title=?,direction=?,merchant=?,order_reference=?,signature_required=?,tracking_carrier=?,tracking_number=?,recipient_url=?,tracking_provider=? WHERE id=?")
            .bind(shipment.title).bind(shipment.direction.as_str()).bind(shipment.merchant).bind(shipment.order_reference)
            .bind(shipment.signature_required)
            .bind(shipment.tracking.carrier.as_db_key()).bind(&shipment.tracking.number)
            .bind(&shipment.tracking.recipient_url).bind(if changed_tracking { ProviderId::for_carrier(Some(shipment.tracking.carrier)) } else { shipment.sync.provider }.as_str())
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

    async fn check_tracking_number(
        tx: &mut Transaction<'_, Sqlite>,
        id: Uuid,
        number: &str,
    ) -> Result<(), Error> {
        // BEGIN IMMEDIATE keeps the number check and following write atomic.
        let existing: Option<String> = sqlx::query_scalar(
            "SELECT id FROM shipments WHERE tracking_number=? AND id<>? LIMIT 1",
        )
        .bind(number)
        .bind(id.to_string())
        .fetch_optional(&mut **tx)
        .await?;
        if let Some(existing) = existing {
            Err(Error::TrackingDuplicate {
                shipment_id: parse_id(&existing)?,
            })
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
            Self::check_tracking_number(&mut tx, id, &shipment.tracking.number).await?;
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
        let mut query = shipment_list_query(filters, after, limit)?;
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

    /// Page inbound delivery estimates and verified delivery history without scan history.
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
            estimated_delivery_at: Option<String>,
            estimated_delivery_raw: Option<String>,
            tracking_provider: ProviderId,
            estimated_delivery_window: Option<String>,
            delivered_at: Option<String>,
            delivery_date: Option<String>,
            calendar_sort_key: i64,
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
                        estimated_delivery_at: crate::delivery::legacy_estimate(
                            row.estimated_delivery_at,
                            row.estimated_delivery_raw.as_deref(),
                            row.tracking_provider == ProviderId::EasyPost,
                        ),
                        estimated_delivery_window: row
                            .estimated_delivery_window
                            .as_deref()
                            .map(serde_json::from_str)
                            .transpose()?,
                        delivered_at: row.delivered_at,
                        delivery_date: row
                            .delivery_date
                            .as_deref()
                            .map(serde_json::from_str)
                            .transpose()?,
                    },
                    position: DeliveryEstimatePosition {
                        id,
                        sort_key: row.calendar_sort_key,
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
        self.summary_in_day(direction, window, None).await
    }

    pub async fn summary_in_day(
        &self,
        direction: Option<Direction>,
        window: Option<crate::delivery::DeliveryWindow>,
        delivered_on: Option<&str>,
    ) -> Result<Summary, Error> {
        let mut query = summary_query(direction, window, delivered_on)?;
        let rows = query
            .build_query_as::<(String, i64, i64)>()
            .fetch_all(&self.pool)
            .await?;
        let mut summary = Summary::default();
        for (status, count, sync_errors) in rows {
            summary.sync_errors += sync_errors;
            let status: Status = serde_json::from_str(&status)?;
            if window.is_some() {
                // A provider's actual delivery (scan or explicit civil date)
                // remains independent of a later manual status correction.
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
            amazon: Default::default(),
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

fn summary_query(
    direction: Option<Direction>,
    window: Option<crate::delivery::DeliveryWindow>,
    delivered_on: Option<&str>,
) -> Result<QueryBuilder<Sqlite>, Error> {
    let delivered_on = validate_delivered_on(delivered_on, window)?;
    let mut query = QueryBuilder::<Sqlite>::new(
        "SELECT coalesce(override_status,carrier_status,'\"expected\"') AS status,count(*),count(last_error) FROM shipments WHERE archived=0",
    );
    if let Some(direction) = direction {
        query.push(" AND direction=").push_bind(direction.as_str());
    }
    if let Some(window) = window {
        push_delivery_window(&mut query, window, delivered_on);
    }
    query.push(" GROUP BY status");
    Ok(query)
}

fn validate_delivered_on(
    delivered_on: Option<&str>,
    window: Option<crate::delivery::DeliveryWindow>,
) -> Result<Option<&str>, Error> {
    if let Some(date) = delivered_on {
        let format = time::format_description::parse_borrowed::<2>("[year]-[month]-[day]")
            .map_err(|_| Error::Invalid("invalid civil date format".into()))?;
        if date.len() != 10 || time::Date::parse(date, &format).is_err() {
            return Err(Error::Invalid(
                "delivered_on must be an ISO civil date".into(),
            ));
        }
        if window.is_none() {
            return Err(Error::Invalid(
                "delivered_on requires delivered_after and delivered_before".into(),
            ));
        }
    }
    Ok(delivered_on)
}

fn push_delivery_window(
    query: &mut QueryBuilder<Sqlite>,
    window: crate::delivery::DeliveryWindow,
    delivered_on: Option<&str>,
) {
    query
        .push(" AND (unixepoch(json_extract(carrier_tracking,'$.delivered_at'))>=")
        .push_bind(window.after())
        .push(" AND unixepoch(json_extract(carrier_tracking,'$.delivered_at'))<")
        .push_bind(window.before());
    if let Some(date) = delivered_on {
        // provider_delivered_on is NULL when an actual timestamp exists.
        // Civil matching uses the caller's day, never a guessed UTC date.
        query.push(" OR provider_delivered_on=").push_bind(date);
    }
    query.push(")");
}

fn shipment_list_query(
    filters: &Filters,
    after: Option<ShipmentPosition>,
    limit: u32,
) -> Result<QueryBuilder<Sqlite>, Error> {
    if filters.source_namespace.is_some() != filters.source_key.is_some() {
        return Err(Error::Invalid(
            "source_namespace and source_key must be supplied together".into(),
        ));
    }
    let delivery_window = crate::delivery::DeliveryWindow::from_bounds(
        filters.delivered_after,
        filters.delivered_before,
    )?;
    let delivered_on = validate_delivered_on(filters.delivered_on.as_deref(), delivery_window)?;
    // Date keys are indexed generated columns. Every page uses the key
    // captured when the preceding row was selected, never a mutable lookup.
    let sort_column = match filters.sort.unwrap_or_default() {
        ShipmentSort::Id => None,
        ShipmentSort::Expected => Some("expected_sort_key"),
        ShipmentSort::Delivered if delivery_window.is_some() && delivered_on.is_some() => Some(
            "-coalesce(unixepoch(json_extract(carrier_tracking,'$.delivered_at')),unixepoch(provider_delivered_on))",
        ),
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
        push_delivery_window(&mut query, window, delivered_on);
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
        normalize_number(&mut tracking.number)?;
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
    Ok(query)
}

fn delivery_estimates_query(
    after: Option<DeliveryEstimatePosition>,
    limit: u32,
) -> QueryBuilder<Sqlite> {
    let mut query = QueryBuilder::new(
        r#"SELECT id, title, tracking_carrier, tracking_provider,
                  coalesce(override_status, carrier_status, '"expected"') AS status,
                  json_extract(carrier_tracking, '$.estimated_delivery_at') AS estimated_delivery_at,
                  json_extract(carrier_tracking, '$.estimated_delivery_raw') AS estimated_delivery_raw,
                  json_extract(carrier_tracking, '$.estimated_delivery_window') AS estimated_delivery_window,
                  json_extract(carrier_tracking, '$.delivered_at') AS delivered_at,
                  json_extract(carrier_tracking, '$.delivery_date') AS delivery_date,
                  calendar_sort_key
           FROM shipments
           WHERE archived = 0 AND direction = 'inbound'
             AND calendar_sort_key < 9223372036854775807
             AND coalesce(override_status, carrier_status, '"expected"')
                 NOT IN ('"return_to_sender"', '"cancelled"')"#,
    );
    if let Some(after) = after {
        query
            .push(" AND (calendar_sort_key,id)>(")
            .push_bind(after.sort_key)
            .push(",")
            .push_bind(after.id.to_string())
            .push(")");
    }
    query
        .push(" ORDER BY calendar_sort_key,id LIMIT ")
        .push_bind(limit.min(201));
    query
}

#[cfg(test)]
mod tests {
    use crate::{
        carrier::Carrier,
        delivery::DeliveryWindow,
        models::{
            CarrierTracking, CreateShipment, DeliveryDateValue, DeliveryEstimatePosition,
            Direction, Filters, InputTracking, ProviderDeliveryDate, ShipmentSort, Status,
        },
        shipping_api::types::{ProviderId, Tracker},
        store::{Store, delivery_estimates_query, shipment_list_query, summary_query},
    };
    use serde::Serialize;
    use sqlx::{Connection, Execute, QueryBuilder, Row, Sqlite, migrate::Migrator};
    use std::path::Path;
    use uuid::Uuid;

    #[derive(Serialize)]
    struct QueryPlan {
        name: &'static str,
        sql: String,
        plan: Vec<String>,
        returned_rows: usize,
        elapsed_micros: u128,
    }

    async fn explain(
        store: &Store,
        name: &'static str,
        mut builder: QueryBuilder<Sqlite>,
    ) -> QueryPlan {
        let sql = builder.sql().as_str().to_owned();
        let mut query = builder.build();
        let arguments = query.take_arguments().unwrap().unwrap();
        let mut actual = QueryBuilder::<Sqlite>::with_arguments("", arguments.clone());
        actual.push(&sql);
        let started = std::time::Instant::now();
        let returned_rows = actual.build().fetch_all(store.pool()).await.unwrap().len();
        let elapsed_micros = started.elapsed().as_micros();
        let mut explain = QueryBuilder::<Sqlite>::with_arguments("EXPLAIN QUERY PLAN ", arguments);
        explain.push(&sql);
        let plan = explain
            .build()
            .fetch_all(store.pool())
            .await
            .unwrap()
            .into_iter()
            .map(|row| row.get("detail"))
            .collect();
        QueryPlan {
            name,
            sql,
            plan,
            returned_rows,
            elapsed_micros,
        }
    }

    #[tokio::test]
    async fn typed_delivery_projections_keep_indexed_bounds_and_migration_history() {
        let directory = tempfile::tempdir().unwrap();
        let migration_directory = directory.path().join("before-typed-dates");
        std::fs::create_dir(&migration_directory).unwrap();
        for entry in
            std::fs::read_dir(Path::new(env!("CARGO_MANIFEST_DIR")).join("migrations")).unwrap()
        {
            let entry = entry.unwrap();
            let name = entry.file_name();
            let version: i64 = name
                .to_str()
                .unwrap()
                .split_once('_')
                .unwrap()
                .0
                .parse()
                .unwrap();
            if version <= 23 {
                std::fs::copy(entry.path(), migration_directory.join(name)).unwrap();
            }
        }
        let database = directory.path().join("plans.sqlite3");
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(
                sqlx::sqlite::SqliteConnectOptions::new()
                    .filename(&database)
                    .create_if_missing(true)
                    .foreign_keys(true),
            )
            .await
            .unwrap();
        Migrator::new(migration_directory.as_path())
            .await
            .unwrap()
            .run(&pool)
            .await
            .unwrap();
        let store = Store { pool };
        let observed = time::OffsetDateTime::parse(
            "2026-10-31T19:00:00Z",
            &time::format_description::well_known::Rfc3339,
        )
        .unwrap();
        let mut protected = Vec::new();
        let mut transaction = store.pool().begin().await.unwrap();
        for index in 0..3072 {
            let date = time::Date::from_calendar_date(
                2026,
                time::Month::October,
                u8::try_from(index / 4 % 28 + 1).unwrap(),
            )
            .unwrap();
            let (status, tracking) = match index % 4 {
                0 => (
                    Status::Delivered,
                    CarrierTracking {
                        delivered_at: Some(
                            date.with_hms(12, 0, 0)
                                .unwrap()
                                .assume_utc()
                                .format(&time::format_description::well_known::Rfc3339)
                                .unwrap(),
                        ),
                        ..Default::default()
                    },
                ),
                1 => (
                    Status::Delivered,
                    CarrierTracking {
                        delivery_date: ProviderDeliveryDate::new(
                            DeliveryDateValue::Delivered { date },
                            observed,
                            "America/Los_Angeles".into(),
                        ),
                        ..Default::default()
                    },
                ),
                2 => (
                    Status::InTransit,
                    CarrierTracking {
                        delivery_date: ProviderDeliveryDate::new(
                            DeliveryDateValue::Estimated { date },
                            observed,
                            "America/Los_Angeles".into(),
                        ),
                        ..Default::default()
                    },
                ),
                _ => (Status::Expected, CarrierTracking::default()),
            };
            let id = Uuid::now_v7();
            sqlx::query("INSERT INTO shipments(id,title,tracking_carrier,tracking_number,carrier_tracking,carrier_status,direction,archived) VALUES (?,?,?,?,?,?,?,?)")
                .bind(id.to_string()).bind(format!("Representative parcel {index}"))
                .bind("USPS").bind(format!("SYNTHETICPLAN{index:06}"))
                .bind(serde_json::to_string(&tracking).unwrap()).bind(serde_json::to_string(&status).unwrap())
                .bind(if index % 7 == 0 { "outbound" } else { "inbound" }).bind(index % 5 == 0)
                .execute(&mut *transaction).await.unwrap();
        }
        transaction.commit().await.unwrap();
        for index in 0..2 {
            let tracker = Tracker {
                provider: ProviderId::AmazonAccount,
                id: format!("synthetic-protected-{index}"),
                tracking_code: format!("PROTECTEDPLAN{index:06}"),
                carrier: Carrier::Usps,
                status: Status::Delivered,
                public_url: None,
                carrier_tracking: CarrierTracking {
                    delivery_date: ProviderDeliveryDate::new(
                        DeliveryDateValue::Delivered {
                            date: time::Date::from_calendar_date(2026, time::Month::October, 31)
                                .unwrap(),
                        },
                        observed,
                        "America/Los_Angeles".into(),
                    ),
                    ..Default::default()
                },
            };
            let row = store
                .create(
                    CreateShipment {
                        title: format!("Protected parcel {index}"),
                        direction: Direction::Inbound,
                        merchant: Some("Synthetic merchant".into()),
                        order_reference: Some(format!("synthetic-order-{index}")),
                        signature_required: Some(true),
                        tracking: InputTracking {
                            number: tracker.tracking_code.clone(),
                            carrier: Some(Carrier::Usps),
                            recipient_url: None,
                        },
                        source: None,
                    },
                    "owner",
                    &format!("protected-{index}"),
                    Some(&tracker),
                )
                .await
                .unwrap();
            protected.push((
                row.id,
                serde_json::to_string(&row).unwrap(),
                serde_json::to_string(&store.events(row.id, 0, 100).await.unwrap()).unwrap(),
            ));
        }
        sqlx::query("ANALYZE").execute(store.pool()).await.unwrap();
        let day_start = time::Date::from_calendar_date(2026, time::Month::October, 25)
            .unwrap()
            .midnight()
            .assume_utc()
            .unix_timestamp();
        let window = DeliveryWindow::from_bounds(Some(day_start), Some(day_start + 86400)).unwrap();
        let timestamp_filters = Filters {
            direction: Some(Direction::Inbound),
            sort: Some(ShipmentSort::Delivered),
            delivered_after: window.map(DeliveryWindow::after),
            delivered_before: window.map(DeliveryWindow::before),
            ..Default::default()
        };
        let mut evidence = Vec::new();
        for (phase, after_migration) in [("before", false), ("after", true)] {
            if after_migration {
                sqlx::migrate!().run(store.pool()).await.unwrap();
                // Production migrates before preparing reads. This same-pool
                // before/after fixture must discard pre-migration SELECT * metadata.
                store
                    .pool()
                    .acquire()
                    .await
                    .unwrap()
                    .clear_cached_statements()
                    .await
                    .unwrap();
                sqlx::query("ANALYZE").execute(store.pool()).await.unwrap();
            }
            evidence.push(
                explain(
                    &store,
                    if after_migration {
                        "calendar_after"
                    } else {
                        "calendar_before"
                    },
                    delivery_estimates_query(None, 101),
                )
                .await,
            );
            evidence.push(
                explain(
                    &store,
                    if after_migration {
                        "calendar_cursor_after"
                    } else {
                        "calendar_cursor_before"
                    },
                    delivery_estimates_query(
                        Some(DeliveryEstimatePosition {
                            id: protected[0].0,
                            sort_key: day_start - 10 * 86400,
                        }),
                        101,
                    ),
                )
                .await,
            );
            for sort in [ShipmentSort::Expected, ShipmentSort::Delivered] {
                let filters = Filters {
                    direction: Some(Direction::Inbound),
                    sort: Some(sort),
                    ..Default::default()
                };
                let plan = explain(
                    &store,
                    match (phase, sort) {
                        ("before", ShipmentSort::Expected) => "expected_before",
                        ("before", _) => "delivered_before",
                        (_, ShipmentSort::Expected) => "expected_after",
                        _ => "delivered_after",
                    },
                    shipment_list_query(&filters, None, 101).unwrap(),
                )
                .await;
                evidence.push(plan);
            }
            evidence.push(
                explain(
                    &store,
                    if after_migration {
                        "timestamp_day_after"
                    } else {
                        "timestamp_day_before"
                    },
                    shipment_list_query(&timestamp_filters, None, 101).unwrap(),
                )
                .await,
            );
            evidence.push(
                explain(
                    &store,
                    if after_migration {
                        "timestamp_summary_after"
                    } else {
                        "timestamp_summary_before"
                    },
                    summary_query(Some(Direction::Inbound), window, None).unwrap(),
                )
                .await,
            );
        }
        let civil_filters = Filters {
            delivered_on: Some("2026-10-25".into()),
            ..timestamp_filters
        };
        evidence.push(
            explain(
                &store,
                "civil_day_after",
                shipment_list_query(&civil_filters, None, 101).unwrap(),
            )
            .await,
        );
        evidence.push(
            explain(
                &store,
                "civil_summary_after",
                summary_query(Some(Direction::Inbound), window, Some("2026-10-25")).unwrap(),
            )
            .await,
        );
        for item in &evidence {
            if item.name.starts_with("calendar")
                || item.name.starts_with("expected")
                || item.name.starts_with("delivered")
            {
                let expected_index = if item.name.starts_with("calendar") {
                    "shipments_direction_calendar_delivery"
                } else if item.name.starts_with("expected") {
                    "shipments_direction_expected_delivery"
                } else {
                    "shipments_direction_delivered_date"
                };
                assert!(
                    item.plan.iter().any(|line| line.contains(expected_index)),
                    "{}: {:?}",
                    item.name,
                    item.plan
                );
                assert!(
                    !item.plan.iter().any(|line| line.contains("TEMP B-TREE")),
                    "{}: {:?}",
                    item.name,
                    item.plan
                );
            } else {
                assert!(
                    item.plan
                        .iter()
                        .any(|line| line.contains("shipments_carrier_delivered_at")),
                    "{}: {:?}",
                    item.name,
                    item.plan
                );
                if item.name.starts_with("civil") {
                    assert!(
                        item.plan
                            .iter()
                            .any(|line| line.contains("shipments_provider_delivered_on")),
                        "{}: {:?}",
                        item.name,
                        item.plan
                    );
                }
            }
        }
        let timestamp_rows = evidence
            .iter()
            .find(|item| item.name == "timestamp_day_after")
            .unwrap()
            .returned_rows;
        let civil_rows = evidence
            .iter()
            .find(|item| item.name == "civil_day_after")
            .unwrap()
            .returned_rows;
        assert!(timestamp_rows > 0 && civil_rows > timestamp_rows && civil_rows < 101);
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT count(*) FROM shipments")
                .fetch_one(store.pool())
                .await
                .unwrap(),
            3074
        );
        for (id, row, events) in protected {
            assert_eq!(
                serde_json::to_string(&store.get(id).await.unwrap()).unwrap(),
                row
            );
            assert_eq!(
                serde_json::to_string(&store.events(id, 0, 100).await.unwrap()).unwrap(),
                events
            );
        }
        if let Ok(path) = std::env::var("SHIPMENTS_TEST_TYPED_DATE_QUERY_EVIDENCE") {
            std::fs::write(path, serde_json::to_string_pretty(&evidence).unwrap()).unwrap();
        }
        store.close().await.unwrap();
    }
}
