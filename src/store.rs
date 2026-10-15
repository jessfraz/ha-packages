use crate::{error::Error, models::*};
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
    id: String,
    revision: i64,
    title: String,
    merchant: Option<String>,
    order_reference: Option<String>,
    tracking_carrier: Option<String>,
    tracking_number: Option<String>,
    carrier_status: Option<String>,
    override_status: Option<String>,
    override_reason: Option<String>,
    override_actor: Option<String>,
    override_at: Option<String>,
    archived: bool,
    created_at: String,
    updated_at: String,
    tracker_id: Option<String>,
    last_success_at: Option<String>,
    last_error: Option<String>,
    next_attempt_at: Option<i64>,
    attempts: i64,
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
            merchant: self.merchant,
            order_reference: self.order_reference,
            tracking: self.tracking_number.map(|number| Tracking {
                carrier: self.tracking_carrier,
                number,
            }),
            status,
            carrier_status,
            status_override,
            archived: self.archived,
            created_at: self.created_at,
            updated_at: self.updated_at,
            sources,
            sync: SyncState {
                tracker_id: self.tracker_id,
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

pub fn normalize_tracking(tracking: &mut Tracking) -> Result<(), Error> {
    valid_text(&tracking.number, "tracking number", 200)?;
    tracking.number = tracking
        .number
        .chars()
        .filter(|c| !c.is_whitespace())
        .flat_map(char::to_uppercase)
        .collect();
    if let Some(carrier) = &mut tracking.carrier {
        valid_text(carrier, "carrier", 100)?;
        *carrier = carrier.trim().to_ascii_lowercase();
    }
    Ok(())
}

fn validate_source(source: &Source) -> Result<(), Error> {
    valid_text(&source.namespace, "source namespace", 200)?;
    valid_text(&source.key, "source key", 500)
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

    pub async fn create(
        &self,
        mut input: CreateShipment,
        actor: &str,
        key: &str,
    ) -> Result<Shipment, Error> {
        valid_text(&input.title, "title", 500)?;
        valid_text(key, "Idempotency-Key", 200)?;
        for (name, value) in [
            ("merchant", &input.merchant),
            ("order reference", &input.order_reference),
        ] {
            if let Some(value) = value {
                valid_text(value, name, 500)?;
            }
        }
        if let Some(tracking) = &mut input.tracking {
            normalize_tracking(tracking)?;
        }
        if let Some(source) = &input.source {
            validate_source(source)?;
        }
        let fingerprint = format!("{:x}", Sha256::digest(serde_json::to_vec(&input)?));
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
        if let Some(tracking) = &input.tracking {
            Self::check_tracking(&mut tx, id, tracking).await?;
        }
        sqlx::query("INSERT INTO shipments (id,title,merchant,order_reference,tracking_carrier,tracking_number,next_attempt_at) VALUES (?,?,?,?,?,?,CASE WHEN ? THEN unixepoch() ELSE NULL END)")
            .bind(id.to_string()).bind(&input.title).bind(&input.merchant).bind(&input.order_reference)
            .bind(input.tracking.as_ref().and_then(|t| t.carrier.as_deref())).bind(input.tracking.as_ref().map(|t| &t.number))
            .bind(input.tracking.is_some()).execute(&mut *tx).await.map_err(unique_error)?;
        if let Some(source) = &input.source {
            Self::source_in(&mut tx, id, source).await?;
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

    pub async fn update(
        &self,
        id: Uuid,
        revision: i64,
        input: UpdateShipment,
        actor: &str,
    ) -> Result<Shipment, Error> {
        let mut tx = self.transaction().await?;
        let mut shipment = Self::get_in(&mut tx, id).await?;
        Self::check_revision(&shipment, revision)?;
        if let Some(title) = input.title {
            valid_text(&title, "title", 500)?;
            shipment.title = title;
        }
        if let Some(value) = input.merchant.value {
            if let Some(text) = &value {
                valid_text(text, "merchant", 500)?;
            }
            shipment.merchant = value;
        }
        if let Some(value) = input.order_reference.value {
            if let Some(text) = &value {
                valid_text(text, "order reference", 500)?;
            }
            shipment.order_reference = value;
        }
        let mut changed_tracking = false;
        if let Some(mut value) = input.tracking.value {
            if let Some(tracking) = &mut value {
                normalize_tracking(tracking)?;
            }
            changed_tracking = value != shipment.tracking;
            shipment.tracking = value;
        }
        if !shipment.archived
            && let Some(tracking) = &shipment.tracking
        {
            Self::check_tracking(&mut tx, id, tracking).await?;
        }
        sqlx::query("UPDATE shipments SET title=?,merchant=?,order_reference=?,tracking_carrier=?,tracking_number=? WHERE id=?")
            .bind(shipment.title).bind(shipment.merchant).bind(shipment.order_reference)
            .bind(shipment.tracking.as_ref().and_then(|t|t.carrier.as_deref())).bind(shipment.tracking.as_ref().map(|t|&t.number))
            .bind(id.to_string()).execute(&mut *tx).await.map_err(unique_error)?;
        if changed_tracking {
            sqlx::query("UPDATE shipments SET tracking_generation=tracking_generation+1,tracker_id=NULL,carrier_status=NULL,last_success_at=NULL,last_error=NULL,attempts=0,lease_until=NULL,lease_token=NULL,next_attempt_at=CASE WHEN tracking_number IS NOT NULL AND archived=0 AND ? THEN unixepoch() END WHERE id=?")
                .bind(!shipment.status_override.as_ref().is_some_and(|value|value.status.is_terminal())).bind(id.to_string()).execute(&mut *tx).await?;
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
        // Without a carrier, a number overlaps every known carrier identity.
        // BEGIN IMMEDIATE makes this check and the subsequent write atomic.
        let exists:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM shipments WHERE archived=0 AND tracking_number=? AND id<>? AND (tracking_carrier IS NULL OR ? IS NULL OR tracking_carrier=?))")
            .bind(&tracking.number).bind(id.to_string()).bind(&tracking.carrier).bind(&tracking.carrier).fetch_one(&mut **tx).await?;
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
    ) -> Result<(), Error> {
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
            Self::source_in(&mut tx, id, &source).await?;
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
        sqlx::query("UPDATE shipments SET override_status=?,override_reason=?,override_actor=?,override_at=CASE WHEN ? THEN strftime('%Y-%m-%dT%H:%M:%fZ','now') END, next_attempt_at=CASE WHEN ? THEN NULL WHEN tracking_number IS NOT NULL AND archived=0 THEN unixepoch() ELSE NULL END WHERE id=?")
            .bind(value.as_ref().map(|v|serde_json::to_string(&v.status)).transpose()?).bind(value.as_ref().map(|v|&v.reason))
            .bind(value.as_ref().map(|_|actor)).bind(value.is_some()).bind(terminal)
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
        if !archived && let Some(tracking) = &shipment.tracking {
            Self::check_tracking(&mut tx, id, tracking).await?;
        }
        sqlx::query("UPDATE shipments SET archived=?,lease_until=NULL,lease_token=NULL,next_attempt_at=CASE WHEN ?=0 AND tracking_number IS NOT NULL AND ? THEN unixepoch() END WHERE id=?")
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
        if shipment.archived || shipment.tracking.is_none() {
            return Err(Error::Conflict(
                "refresh requires an unarchived shipment with tracking".into(),
            ));
        }
        // A pending retry remains rate-limited; repeated client refreshes coalesce.
        sqlx::query("UPDATE shipments SET next_attempt_at=CASE WHEN last_error IS NOT NULL AND next_attempt_at>unixepoch() THEN next_attempt_at ELSE min(coalesce(next_attempt_at,unixepoch()),unixepoch()) END WHERE id=?")
            .bind(id.to_string()).execute(&mut *tx).await?;
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
        after: Option<Uuid>,
        limit: u32,
    ) -> Result<Vec<Shipment>, Error> {
        if filters.source_namespace.is_some() != filters.source_key.is_some() {
            return Err(Error::Invalid(
                "source_namespace and source_key must be supplied together".into(),
            ));
        }
        let mut query = QueryBuilder::<Sqlite>::new("SELECT * FROM shipments WHERE archived=");
        query.push_bind(filters.archived.unwrap_or(false));
        if let Some(id) = after {
            query.push(" AND id>").push_bind(id.to_string());
        }
        if let Some(value) = &filters.tracking_number {
            let mut tracking = Tracking {
                number: value.clone(),
                carrier: None,
            };
            normalize_tracking(&mut tracking)?;
            query
                .push(" AND tracking_number=")
                .push_bind(tracking.number);
        }
        if let Some(value) = &filters.carrier {
            query
                .push(" AND tracking_carrier=")
                .push_bind(value.trim().to_ascii_lowercase());
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
        query.push(" ORDER BY id LIMIT ").push_bind(limit.min(201));
        let mut tx = self.pool.begin().await?;
        let rows = query.build_query_as::<Record>().fetch_all(&mut *tx).await?;
        let mut sources_by_id: HashMap<String, Vec<Source>> = HashMap::new();
        if !rows.is_empty() {
            let mut source_query = QueryBuilder::<Sqlite>::new(
                "SELECT shipment_id,namespace,key FROM sources WHERE shipment_id IN (",
            );
            {
                let mut separated = source_query.separated(",");
                for row in &rows {
                    separated.push_bind(&row.id);
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
                let sources = sources_by_id.remove(&row.id).unwrap_or_default();
                row.into_shipment(sources)
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

    pub async fn summary(&self) -> Result<Summary, Error> {
        let rows=sqlx::query_as::<_,(String,i64)>("SELECT coalesce(override_status,carrier_status,'\"expected\"') AS status,count(*) FROM shipments WHERE archived=0 GROUP BY status").fetch_all(&self.pool).await?;
        let mut summary = Summary::default();
        for (status, count) in rows {
            let status: Status = serde_json::from_str(&status)?;
            if !status.is_terminal() {
                summary.active += count;
            }
            match status {
                Status::Expected | Status::Unknown | Status::PreTransit => {
                    summary.expected += count
                }
                Status::InTransit | Status::AvailableForPickup => summary.in_transit += count,
                Status::OutForDelivery => summary.out_for_delivery += count,
                Status::Delivered => summary.delivered += count,
                Status::Failure | Status::Error | Status::ReturnToSender => {
                    summary.exceptions += count
                }
                _ => {}
            }
        }
        summary.sync_errors = sqlx::query_scalar(
            "SELECT count(*) FROM shipments WHERE archived=0 AND last_error IS NOT NULL",
        )
        .fetch_one(&self.pool)
        .await?;
        Ok(summary)
    }

    pub async fn system(&self, provider_enabled: bool) -> Result<System, Error> {
        let (pending_syncs,sync_errors)=sqlx::query_as("SELECT count(CASE WHEN next_attempt_at IS NOT NULL THEN 1 END),count(last_error) FROM shipments WHERE archived=0").fetch_one(&self.pool).await?;
        Ok(System {
            version: env!("CARGO_PKG_VERSION").into(),
            provider_enabled,
            pending_syncs,
            sync_errors,
            credential_role: String::new(),
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
