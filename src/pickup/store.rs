use sha2::{Digest, Sha256};
use sqlx::{FromRow, QueryBuilder, Sqlite, SqlitePool};
use uuid::Uuid;

use crate::pickup::{
    Error, ErrorCode, Result,
    models::{
        Batch, Confirmation, Offer, Parcel, ParcelInput, PickupCarrier, Record, ResolvedPlan,
        ScheduleRequest, Site, SiteInput, State, normalize_tracking, text,
    },
};

#[derive(Clone)]
pub(crate) struct Store {
    pool: SqlitePool,
}

#[derive(FromRow)]
struct SiteRow {
    id: String,
    revision: i64,
    is_default: bool,
    details: String,
}
impl SiteRow {
    fn site(self) -> Result<Site> {
        Ok(Site {
            id: uuid(&self.id)?,
            revision: self.revision,
            is_default: self.is_default,
            details: serde_json::from_str(&self.details)?,
        })
    }
}

#[derive(FromRow)]
struct JobRow {
    id: String,
    batch_id: String,
    revision: i64,
    state: State,
    accept_fee: bool,
    plan: String,
    offer: Option<String>,
    confirmation: Option<String>,
    error: Option<String>,
    created_at: i64,
    updated_at: i64,
}
impl JobRow {
    fn record(self) -> Result<Record> {
        // A process can die after dispatch but before recording the reply. No
        // startup worker reclaims this row. Its durable dispatch fence remains.
        let state = if matches!(self.state, State::Dispatching | State::Cancelling)
            && jiff::Timestamp::now()
                .as_second()
                .saturating_sub(self.updated_at)
                > 180
        {
            State::Unknown
        } else {
            self.state
        };
        Ok(Record {
            id: uuid(&self.id)?,
            batch_id: uuid(&self.batch_id)?,
            revision: self.revision,
            state,
            accept_fee: self.accept_fee,
            plan: serde_json::from_str(&self.plan)?,
            offer: self.offer.map(|s| serde_json::from_str(&s)).transpose()?,
            confirmation: self
                .confirmation
                .map(|s| serde_json::from_str(&s))
                .transpose()?,
            error: self.error.map(|s| serde_json::from_str(&s)).transpose()?,
            created_at: self.created_at,
            updated_at: self.updated_at,
        })
    }
}

fn uuid(value: &str) -> Result<Uuid> {
    Uuid::parse_str(value).map_err(|_| Error::new(ErrorCode::StorageUnavailable))
}
fn conflict() -> Error {
    Error::new(ErrorCode::Conflict)
}

impl Store {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }
    pub async fn check_cooldown(&self, account: &[u8; 32]) -> Result<()> {
        let delay = sqlx::query_scalar::<_,i64>("SELECT max(0,not_before-unixepoch()) FROM pickup_cooldowns WHERE account_fingerprint=?").bind(account.as_slice()).fetch_optional(&self.pool).await?;
        if let Some(delay) = delay.filter(|delay| *delay > 0) {
            return Err(Error {
                code: ErrorCode::RateLimited,
                retry_after_seconds: Some(delay.unsigned_abs()),
            });
        }
        Ok(())
    }
    pub async fn defer(&self, account: &[u8; 32], seconds: u64) -> Result<()> {
        let delay = i64::try_from(seconds).unwrap_or(i64::MAX);
        sqlx::query("INSERT INTO pickup_cooldowns(account_fingerprint,not_before) VALUES(?,unixepoch()+min(?,9223372036854775807-unixepoch())) ON CONFLICT(account_fingerprint) DO UPDATE SET not_before=max(not_before,excluded.not_before)").bind(account.as_slice()).bind(delay).execute(&self.pool).await?;
        Ok(())
    }
    pub async fn status(&self) -> Result<crate::pickup::models::PickupStatus> {
        let (scheduled, uncertain): (u32, u32) = sqlx::query_as("SELECT (SELECT count(*) FROM pickup_jobs WHERE state='scheduled' AND pickup_date>=date('now','-1 day')), ((SELECT count(*) FROM pickup_jobs WHERE state='unknown') + (SELECT count(*) FROM pickup_jobs WHERE state IN ('dispatching','cancelling') AND updated_at<unixepoch()-180))").fetch_one(&self.pool).await?;
        let rows = sqlx::query_as::<_,JobRow>("SELECT id,batch_id,revision,state,accept_fee,plan,offer,confirmation,error,created_at,updated_at FROM pickup_jobs WHERE state='scheduled' AND pickup_date>=date('now','-1 day') ORDER BY pickup_date,id LIMIT 201").fetch_all(&self.pool).await?;
        let truncated = rows.len() > 200;
        let windows = rows
            .into_iter()
            .take(200)
            .map(|row| {
                let record = row.record()?;
                Ok(crate::pickup::models::PickupWindow {
                    id: record.id,
                    carrier: record.plan.carrier,
                    date: record
                        .confirmation
                        .ok_or_else(|| Error::new(ErrorCode::StorageUnavailable))?
                        .pickup_date,
                    ready_time: record.plan.ready_time,
                    close_time: record.plan.close_time,
                    timezone: record.plan.site.details.timezone,
                })
            })
            .collect::<Result<_>>()?;
        Ok(crate::pickup::models::PickupStatus {
            scheduled,
            uncertain,
            windows,
            windows_truncated: truncated,
        })
    }
    pub async fn sites(&self) -> Result<Vec<Site>> {
        sqlx::query_as::<_,SiteRow>("SELECT id,revision,is_default,details FROM pickup_sites ORDER BY is_default DESC,id LIMIT 32").fetch_all(&self.pool).await?.into_iter().map(SiteRow::site).collect()
    }
    pub async fn site(&self, id: Option<Uuid>) -> Result<Site> {
        let row =
            match id {
                Some(id) => {
                    sqlx::query_as::<_, SiteRow>(
                        "SELECT id,revision,is_default,details FROM pickup_sites WHERE id=?",
                    )
                    .bind(id.to_string())
                    .fetch_optional(&self.pool)
                    .await?
                }
                None => sqlx::query_as::<_, SiteRow>(
                    "SELECT id,revision,is_default,details FROM pickup_sites WHERE is_default=1",
                )
                .fetch_optional(&self.pool)
                .await?,
            };
        row.ok_or_else(|| Error::new(ErrorCode::LocationRequired))?
            .site()
    }
    pub async fn save_site(
        &self,
        id: Option<Uuid>,
        revision: Option<i64>,
        input: SiteInput,
        make_default: bool,
    ) -> Result<Site> {
        input.validate()?;
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let id = if let Some(id) = id {
            let current =
                sqlx::query_scalar::<_, i64>("SELECT revision FROM pickup_sites WHERE id=?")
                    .bind(id.to_string())
                    .fetch_optional(&mut *tx)
                    .await?
                    .ok_or_else(|| Error::new(ErrorCode::NotFound))?;
            if revision != Some(current) {
                return Err(conflict());
            }
            id
        } else {
            let count: i64 = sqlx::query_scalar("SELECT count(*) FROM pickup_sites")
                .fetch_one(&mut *tx)
                .await?;
            if count >= 32 {
                return Err(conflict());
            }
            Uuid::now_v7()
        };
        if make_default {
            sqlx::query("UPDATE pickup_sites SET is_default=0,revision=revision+1 WHERE is_default=1 AND id<>?").bind(id.to_string()).execute(&mut *tx).await?;
        }
        sqlx::query("INSERT INTO pickup_sites(id,revision,is_default,details) VALUES(?,1,?,?) ON CONFLICT(id) DO UPDATE SET revision=pickup_sites.revision+1,is_default=excluded.is_default,details=excluded.details")
            .bind(id.to_string()).bind(make_default).bind(serde_json::to_string(&input)?).execute(&mut *tx).await?;
        tx.commit().await?;
        self.site(Some(id)).await
    }
    pub async fn save_parcel(&self, tracking: &str, details: ParcelInput) -> Result<Parcel> {
        details.validate()?;
        let tracking = normalize_tracking(tracking)?;
        sqlx::query("INSERT INTO pickup_parcels(carrier,tracking,details) VALUES(?,?,?) ON CONFLICT(carrier,tracking) DO UPDATE SET details=excluded.details").bind(details.carrier.as_str()).bind(&tracking).bind(serde_json::to_string(&details)?).execute(&self.pool).await?;
        Ok(Parcel { tracking, details })
    }
    pub async fn parcel(
        &self,
        tracking: &str,
        carrier: Option<PickupCarrier>,
    ) -> Result<Option<Parcel>> {
        let tracking = normalize_tracking(tracking)?;
        let rows = if let Some(carrier) = carrier {
            sqlx::query_scalar::<_, String>(
                "SELECT details FROM pickup_parcels WHERE carrier=? AND tracking=?",
            )
            .bind(carrier.as_str())
            .bind(&tracking)
            .fetch_all(&self.pool)
            .await?
        } else {
            sqlx::query_scalar::<_, String>(
                "SELECT details FROM pickup_parcels WHERE tracking=? LIMIT 2",
            )
            .bind(&tracking)
            .fetch_all(&self.pool)
            .await?
        };
        if rows.len() > 1 {
            return Err(conflict());
        }
        rows.into_iter()
            .next()
            .map(|details| {
                Ok(Parcel {
                    tracking,
                    details: serde_json::from_str(&details)?,
                })
            })
            .transpose()
    }
    pub async fn existing_batch(
        &self,
        actor: &str,
        key: &str,
        request: &ScheduleRequest,
    ) -> Result<Option<Batch>> {
        let row = sqlx::query_as::<_, (String, Vec<u8>)>(
            "SELECT id,fingerprint FROM pickup_batches WHERE actor=? AND operation_key=?",
        )
        .bind(actor)
        .bind(key)
        .fetch_optional(&self.pool)
        .await?;
        if let Some((id, fingerprint)) = row {
            if fingerprint != request_fingerprint(request)? {
                return Err(conflict());
            }
            return self.batch(uuid(&id)?).await.map(Some);
        }
        Ok(None)
    }
    pub async fn create_batch(
        &self,
        actor: &str,
        key: &str,
        request: &ScheduleRequest,
        plans: Vec<(ResolvedPlan, [u8; 32])>,
    ) -> Result<Batch> {
        text(actor, 200)?;
        text(key, 200)?;
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let fingerprint = request_fingerprint(request)?;
        if let Some((id, prior)) = sqlx::query_as::<_, (String, Vec<u8>)>(
            "SELECT id,fingerprint FROM pickup_batches WHERE actor=? AND operation_key=?",
        )
        .bind(actor)
        .bind(key)
        .fetch_optional(&mut *tx)
        .await?
        {
            if fingerprint != prior {
                return Err(conflict());
            }
            tx.commit().await?;
            return self.batch(uuid(&id)?).await;
        }
        let batch = Uuid::now_v7();
        sqlx::query(
            "INSERT INTO pickup_batches(id,actor,operation_key,fingerprint) VALUES(?,?,?,?)",
        )
        .bind(batch.to_string())
        .bind(actor)
        .bind(key)
        .bind(fingerprint)
        .execute(&mut *tx)
        .await?;
        for (mut plan, account) in plans {
            let site_revision: Option<i64> =
                sqlx::query_scalar("SELECT revision FROM pickup_sites WHERE id=?")
                    .bind(plan.site.id.to_string())
                    .fetch_optional(&mut *tx)
                    .await?;
            if site_revision != Some(plan.site.revision) {
                return Err(Error::new(ErrorCode::LocationChanged));
            }
            let intent = intent_fingerprint(&plan, &account)?;
            if let Some(id) = sqlx::query_scalar::<_, String>(
                "SELECT pickup_id FROM pickup_intent_claims WHERE fingerprint=?",
            )
            .bind(&intent)
            .fetch_optional(&mut *tx)
            .await?
            {
                sqlx::query("INSERT INTO pickup_batch_items(batch_id,pickup_id) VALUES(?,?)")
                    .bind(batch.to_string())
                    .bind(id)
                    .execute(&mut *tx)
                    .await?;
                continue;
            }
            let claimed = if plan.tracking.is_empty() {
                Vec::new()
            } else {
                let mut query = QueryBuilder::<Sqlite>::new(
                    "SELECT tracking,pickup_id FROM pickup_label_claims WHERE carrier=",
                );
                query
                    .push_bind(plan.carrier.as_str())
                    .push(" AND tracking IN (");
                let mut values = query.separated(",");
                for tracking in &plan.tracking {
                    values.push_bind(tracking);
                }
                values.push_unseparated(")");
                query
                    .build_query_as::<(String, String)>()
                    .fetch_all(&mut *tx)
                    .await?
            };
            if !claimed.is_empty() {
                if claimed.len() != plan.tracking.len() {
                    return Err(conflict());
                }
                let ids = claimed
                    .into_iter()
                    .map(|(_, id)| id)
                    .collect::<std::collections::BTreeSet<_>>();
                for id in ids {
                    let (saved, prior_account): (String, Vec<u8>) = sqlx::query_as(
                        "SELECT plan,account_fingerprint FROM pickup_jobs WHERE id=?",
                    )
                    .bind(&id)
                    .fetch_one(&mut *tx)
                    .await?;
                    let saved: ResolvedPlan = serde_json::from_str(&saved)?;
                    if saved.site.details.address != plan.site.details.address
                        || prior_account != account
                        || request
                            .date
                            .as_ref()
                            .is_some_and(|date| saved.date.as_ref() != Some(date))
                        || request
                            .ready_time
                            .as_ref()
                            .is_some_and(|time| &saved.ready_time != time)
                        || request
                            .close_time
                            .as_ref()
                            .is_some_and(|time| &saved.close_time != time)
                    {
                        return Err(conflict());
                    }
                    sqlx::query("INSERT INTO pickup_batch_items(batch_id,pickup_id) VALUES(?,?)")
                        .bind(batch.to_string())
                        .bind(id)
                        .execute(&mut *tx)
                        .await?;
                }
                continue;
            }
            if plan.carrier != PickupCarrier::Usps && !request.accept_fee {
                return Err(Error::new(ErrorCode::FeeConsentRequired));
            }
            let id = Uuid::now_v7();
            plan.reference = id;
            sqlx::query("INSERT INTO pickup_jobs(id,batch_id,group_key,account_fingerprint,plan,accept_fee) VALUES(?,?,?,?,?,?)").bind(id.to_string()).bind(batch.to_string()).bind(plan.parcels.first().ok_or_else(conflict)?.service.group()).bind(account.as_slice()).bind(serde_json::to_string(&plan)?).bind(request.accept_fee).execute(&mut *tx).await?;
            sqlx::query("INSERT INTO pickup_batch_items(batch_id,pickup_id) VALUES(?,?)")
                .bind(batch.to_string())
                .bind(id.to_string())
                .execute(&mut *tx)
                .await?;
            sqlx::query("INSERT INTO pickup_intent_claims(fingerprint,pickup_id) VALUES(?,?)")
                .bind(intent)
                .bind(id.to_string())
                .execute(&mut *tx)
                .await?;
            for tracking in &plan.tracking {
                sqlx::query(
                    "INSERT INTO pickup_label_claims(carrier,tracking,pickup_id) VALUES(?,?,?)",
                )
                .bind(plan.carrier.as_str())
                .bind(tracking)
                .bind(id.to_string())
                .execute(&mut *tx)
                .await?;
                sqlx::query("INSERT INTO pickup_labels(pickup_id,carrier,tracking) VALUES(?,?,?)")
                    .bind(id.to_string())
                    .bind(plan.carrier.as_str())
                    .bind(tracking)
                    .execute(&mut *tx)
                    .await?;
            }
        }
        tx.commit().await?;
        self.batch(batch).await
    }
    pub async fn get(&self, id: Uuid) -> Result<Record> {
        sqlx::query_as::<_,JobRow>("SELECT id,batch_id,revision,state,accept_fee,plan,offer,confirmation,error,created_at,updated_at FROM pickup_jobs WHERE id=?").bind(id.to_string()).fetch_optional(&self.pool).await?.ok_or_else(||Error::new(ErrorCode::NotFound))?.record()
    }
    pub async fn batch(&self, id: Uuid) -> Result<Batch> {
        let rows=sqlx::query_as::<_,JobRow>("SELECT id,batch_id,revision,state,accept_fee,plan,offer,confirmation,error,created_at,updated_at FROM pickup_jobs WHERE id IN (SELECT pickup_id FROM pickup_batch_items WHERE batch_id=?) ORDER BY id LIMIT 100").bind(id.to_string()).fetch_all(&self.pool).await?;
        if rows.is_empty() {
            return Err(Error::new(ErrorCode::NotFound));
        }
        Ok(Batch {
            id,
            items: rows
                .into_iter()
                .map(JobRow::record)
                .collect::<Result<_>>()?,
        })
    }
    pub async fn list(&self, after: Option<Uuid>, limit: u32) -> Result<Vec<Record>> {
        let rows=match after{
            Some(after)=>sqlx::query_as::<_,JobRow>("SELECT id,batch_id,revision,state,accept_fee,plan,offer,confirmation,error,created_at,updated_at FROM pickup_jobs WHERE id<? ORDER BY id DESC LIMIT ?").bind(after.to_string()).bind(limit.clamp(1,201)).fetch_all(&self.pool).await?,
            None=>sqlx::query_as::<_,JobRow>("SELECT id,batch_id,revision,state,accept_fee,plan,offer,confirmation,error,created_at,updated_at FROM pickup_jobs ORDER BY id DESC LIMIT ?").bind(limit.clamp(1,201)).fetch_all(&self.pool).await?,
        };
        rows.into_iter().map(JobRow::record).collect()
    }
    pub async fn resolve(&self, reference: &str, carrier: Option<PickupCarrier>) -> Result<Record> {
        if let Ok(id) = Uuid::parse_str(reference) {
            let record = self.get(id).await?;
            if carrier.is_some_and(|c| c != record.plan.carrier) {
                return Err(conflict());
            }
            return Ok(record);
        }
        let tracking = normalize_tracking(reference)?;
        // Current claims identify the active operation. Historical attempts
        // remain explicit only when no current claim exists.
        let current = match carrier {
            Some(carrier) => sqlx::query_as::<_,JobRow>("SELECT j.id,j.batch_id,j.revision,j.state,j.accept_fee,j.plan,j.offer,j.confirmation,j.error,j.created_at,j.updated_at FROM pickup_label_claims c JOIN pickup_jobs j ON j.id=c.pickup_id WHERE c.tracking=? AND c.carrier=? LIMIT 2").bind(&tracking).bind(carrier.as_str()).fetch_all(&self.pool).await?,
            None => sqlx::query_as::<_,JobRow>("SELECT j.id,j.batch_id,j.revision,j.state,j.accept_fee,j.plan,j.offer,j.confirmation,j.error,j.created_at,j.updated_at FROM pickup_label_claims c JOIN pickup_jobs j ON j.id=c.pickup_id WHERE c.tracking=? LIMIT 2").bind(&tracking).fetch_all(&self.pool).await?,
        };
        if current.len() > 1 {
            return Err(Error::new(ErrorCode::Ambiguous));
        }
        if let Some(row) = current.into_iter().next() {
            return row.record();
        }
        let ids =
            match carrier {
                Some(carrier) => sqlx::query_scalar::<_, String>(
                    "SELECT pickup_id FROM pickup_labels WHERE carrier=? AND tracking=? LIMIT 2",
                )
                .bind(carrier.as_str())
                .bind(&tracking)
                .fetch_all(&self.pool)
                .await?,
                None => {
                    sqlx::query_scalar::<_, String>(
                        "SELECT pickup_id FROM pickup_labels WHERE tracking=? LIMIT 2",
                    )
                    .bind(&tracking)
                    .fetch_all(&self.pool)
                    .await?
                }
            };
        let ids = if ids.is_empty() {
            match carrier {
                Some(carrier) => sqlx::query_scalar::<_, String>(
                    "SELECT id FROM pickup_jobs WHERE confirmation_number=? AND carrier=? LIMIT 2",
                )
                .bind(&tracking)
                .bind(carrier.as_str())
                .fetch_all(&self.pool)
                .await?,
                None => {
                    sqlx::query_scalar::<_, String>(
                        "SELECT id FROM pickup_jobs WHERE confirmation_number=? LIMIT 2",
                    )
                    .bind(&tracking)
                    .fetch_all(&self.pool)
                    .await?
                }
            }
        } else {
            ids
        };
        if ids.len() != 1 {
            return Err(Error::new(if ids.is_empty() {
                ErrorCode::NotFound
            } else {
                ErrorCode::Ambiguous
            }));
        }
        self.get(uuid(&ids[0])?).await
    }
    pub async fn dispatch(
        &self,
        id: Uuid,
        revision: i64,
        plan: &ResolvedPlan,
        offer: &Offer,
        account: &[u8; 32],
    ) -> Result<bool> {
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let site_revision: Option<i64> =
            sqlx::query_scalar("SELECT revision FROM pickup_sites WHERE id=?")
                .bind(plan.site.id.to_string())
                .fetch_optional(&mut *tx)
                .await?;
        if site_revision != Some(plan.site.revision) {
            return Err(conflict());
        }
        if !plan.tracking.is_empty() {
            let mut query = QueryBuilder::<Sqlite>::new(
                "SELECT tracking_number,tracking_carrier,(registration_state='registered' AND tracker_id IS NOT NULL AND archived=0) FROM shipments WHERE tracking_number IN (",
            );
            let mut values = query.separated(",");
            for tracking in &plan.tracking {
                values.push_bind(tracking);
            }
            values.push_unseparated(") LIMIT 100");
            let watched = query
                .build_query_as::<(String, String, bool)>()
                .fetch_all(&mut *tx)
                .await?;
            if watched.len() != plan.tracking.len()
                || watched.iter().any(|(_, carrier, ready)| {
                    !*ready
                        || carrier
                            .parse::<crate::carrier::Carrier>()
                            .ok()
                            .and_then(PickupCarrier::from_tracking)
                            != Some(plan.carrier)
                })
            {
                return Err(Error::new(ErrorCode::WatchNotReady));
            }
        }
        let changed=sqlx::query("UPDATE pickup_jobs SET state='dispatching',revision=revision+1,plan=?,offer=?,updated_at=unixepoch() WHERE id=? AND state='pending' AND revision=? AND account_fingerprint=?").bind(serde_json::to_string(plan)?).bind(serde_json::to_string(offer)?).bind(id.to_string()).bind(revision).bind(account.as_slice()).execute(&mut *tx).await?.rows_affected()==1;
        tx.commit().await?;
        Ok(changed)
    }
    pub async fn finish(
        &self,
        id: Uuid,
        expected: State,
        state: State,
        confirmation: Option<&Confirmation>,
        error: Option<&Error>,
    ) -> Result<()> {
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let changed=sqlx::query("UPDATE pickup_jobs SET state=?,revision=revision+1,confirmation=coalesce(?,confirmation),error=?,updated_at=unixepoch() WHERE id=? AND state=?").bind(state).bind(confirmation.map(serde_json::to_string).transpose()?).bind(error.map(serde_json::to_string).transpose()?).bind(id.to_string()).bind(expected).execute(&mut *tx).await?.rows_affected()==1;
        if !changed {
            return Err(conflict());
        }
        if matches!(state, State::Rejected | State::Cancelled) {
            sqlx::query("DELETE FROM pickup_intent_claims WHERE pickup_id=?")
                .bind(id.to_string())
                .execute(&mut *tx)
                .await?;
            sqlx::query("DELETE FROM pickup_label_claims WHERE pickup_id=?")
                .bind(id.to_string())
                .execute(&mut *tx)
                .await?;
        }
        tx.commit().await?;
        Ok(())
    }
    pub async fn claim_cancel(
        &self,
        id: Uuid,
        revision: i64,
        actor: &str,
        key: &str,
        account: &[u8; 32],
    ) -> Result<bool> {
        text(key, 200)?;
        let changed=sqlx::query("UPDATE pickup_jobs SET state='cancel_pending',revision=revision+1,cancel_actor=?,cancel_key=?,updated_at=unixepoch() WHERE id=? AND state='scheduled' AND revision=? AND account_fingerprint=?").bind(actor).bind(key).bind(id.to_string()).bind(revision).bind(account.as_slice()).execute(&self.pool).await?.rows_affected()==1;
        if changed {
            return Ok(true);
        }
        let previous = sqlx::query_as::<_, (Option<String>, Option<String>)>(
            "SELECT cancel_actor,cancel_key FROM pickup_jobs WHERE id=?",
        )
        .bind(id.to_string())
        .fetch_optional(&self.pool)
        .await?;
        if previous == Some((Some(actor.into()), Some(key.into()))) {
            return Ok(false);
        }
        Err(conflict())
    }
    pub async fn refresh(
        &self,
        id: Uuid,
        revision: i64,
        confirmation: &Confirmation,
    ) -> Result<Record> {
        let changed=sqlx::query("UPDATE pickup_jobs SET confirmation=?,revision=revision+1,error=NULL,updated_at=unixepoch() WHERE id=? AND revision=? AND state='scheduled'").bind(serde_json::to_string(confirmation)?).bind(id.to_string()).bind(revision).execute(&self.pool).await?.rows_affected()==1;
        if !changed {
            return Err(conflict());
        }
        self.get(id).await
    }
    pub async fn reconcile(
        &self,
        record: &Record,
        actor: &str,
        input: &crate::pickup::models::ReconcileRequest,
        account: &[u8; 32],
    ) -> Result<Record> {
        use crate::pickup::models::ReconcileOutcome;
        if record.state != State::Unknown || !input.carrier_portal_checked {
            return Err(conflict());
        }
        let (state, confirmation) = match &input.outcome {
            ReconcileOutcome::Scheduled { confirmation } => (State::Scheduled, Some(confirmation)),
            ReconcileOutcome::NotScheduled if record.confirmation.is_none() => {
                (State::Rejected, None)
            }
            ReconcileOutcome::Cancelled if record.confirmation.is_some() => {
                (State::Cancelled, None)
            }
            _ => return Err(conflict()),
        };
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let changed=sqlx::query("UPDATE pickup_jobs SET state=?,revision=revision+1,confirmation=coalesce(?,confirmation),error=NULL,updated_at=unixepoch() WHERE id=? AND revision=? AND account_fingerprint=? AND (state='unknown' OR (state IN ('dispatching','cancelling') AND updated_at<unixepoch()-180))").bind(state).bind(confirmation.map(serde_json::to_string).transpose()?).bind(record.id.to_string()).bind(record.revision).bind(account.as_slice()).execute(&mut *tx).await?.rows_affected()==1;
        if !changed {
            return Err(conflict());
        }
        sqlx::query("INSERT INTO pickup_reconciliations(pickup_id,revision,actor,assertion) VALUES(?,?,?,?)").bind(record.id.to_string()).bind(record.revision+1).bind(actor).bind(serde_json::to_string(input)?).execute(&mut *tx).await?;
        if matches!(state, State::Rejected | State::Cancelled) {
            sqlx::query("DELETE FROM pickup_intent_claims WHERE pickup_id=?")
                .bind(record.id.to_string())
                .execute(&mut *tx)
                .await?;
            sqlx::query("DELETE FROM pickup_label_claims WHERE pickup_id=?")
                .bind(record.id.to_string())
                .execute(&mut *tx)
                .await?;
        }
        tx.commit().await?;
        self.get(record.id).await
    }
    pub async fn pending(&self) -> Result<Option<Record>> {
        sqlx::query_as::<_,JobRow>("SELECT id,batch_id,revision,state,accept_fee,plan,offer,confirmation,error,created_at,updated_at FROM pickup_jobs INDEXED BY pickup_jobs_pending WHERE state IN ('pending','cancel_pending') ORDER BY id LIMIT 1").fetch_optional(&self.pool).await?.map(JobRow::record).transpose()
    }
    pub async fn account_matches(&self, id: Uuid, account: &[u8; 32]) -> Result<bool> {
        Ok(sqlx::query_scalar::<_, bool>(
            "SELECT EXISTS(SELECT 1 FROM pickup_jobs WHERE id=? AND account_fingerprint=?)",
        )
        .bind(id.to_string())
        .bind(account.as_slice())
        .fetch_one(&self.pool)
        .await?)
    }
    pub async fn begin_cancel(&self, id: Uuid, revision: i64, account: &[u8; 32]) -> Result<bool> {
        Ok(sqlx::query("UPDATE pickup_jobs SET state='cancelling',revision=revision+1,updated_at=unixepoch() WHERE id=? AND revision=? AND state='cancel_pending' AND account_fingerprint=?").bind(id.to_string()).bind(revision).bind(account.as_slice()).execute(&self.pool).await?.rows_affected()==1)
    }
    pub async fn parcels(&self, tracking: &[String]) -> Result<Vec<Parcel>> {
        if tracking.is_empty() {
            return Ok(Vec::new());
        }
        let mut query = QueryBuilder::<Sqlite>::new(
            "SELECT tracking,details FROM pickup_parcels WHERE tracking IN (",
        );
        let mut values = query.separated(",");
        for value in tracking {
            values.push_bind(value);
        }
        values.push_unseparated(") LIMIT 300");
        query
            .build_query_as::<(String, String)>()
            .fetch_all(&self.pool)
            .await?
            .into_iter()
            .map(|(tracking, details)| {
                Ok(Parcel {
                    tracking,
                    details: serde_json::from_str(&details)?,
                })
            })
            .collect()
    }
    pub async fn watched(
        &self,
        tracking: &[String],
    ) -> Result<Vec<(String, crate::carrier::Carrier, bool)>> {
        if tracking.is_empty() {
            return Ok(Vec::new());
        }
        let mut query = QueryBuilder::<Sqlite>::new(
            "SELECT tracking_number,tracking_carrier,(registration_state='registered' AND tracker_id IS NOT NULL AND archived=0) FROM shipments WHERE tracking_number IN (",
        );
        let mut values = query.separated(",");
        for value in tracking {
            values.push_bind(value);
        }
        values.push_unseparated(") LIMIT 100");
        query
            .build_query_as::<(String, String, bool)>()
            .fetch_all(&self.pool)
            .await?
            .into_iter()
            .map(|(tracking, carrier, ready)| {
                Ok((
                    tracking,
                    carrier
                        .parse()
                        .map_err(|_| Error::new(ErrorCode::StorageUnavailable))?,
                    ready,
                ))
            })
            .collect()
    }
}

fn request_fingerprint(request: &ScheduleRequest) -> Result<Vec<u8>> {
    let mut request = request.clone();
    request.tracking = request
        .tracking
        .iter()
        .map(|s| normalize_tracking(s))
        .collect::<Result<_>>()?;
    request.tracking.sort();
    Ok(Sha256::digest(serde_json::to_vec(&request)?).to_vec())
}

fn intent_fingerprint(plan: &ResolvedPlan, account: &[u8; 32]) -> Result<Vec<u8>> {
    #[derive(serde::Serialize)]
    struct Intent<'a> {
        carrier: PickupCarrier,
        address: &'a crate::pickup::models::Address,
        date: &'a Option<String>,
        ready: &'a str,
        close: &'a str,
        parcels: &'a [crate::pickup::models::ParcelGroup],
        weight: u32,
        overweight: bool,
        tracking: Vec<&'a str>,
    }
    let mut tracking = plan.tracking.iter().map(String::as_str).collect::<Vec<_>>();
    tracking.sort_unstable();
    let input = Intent {
        carrier: plan.carrier,
        address: &plan.site.details.address,
        date: &plan.date,
        ready: &plan.ready_time,
        close: &plan.close_time,
        parcels: &plan.parcels,
        weight: plan.total_weight_hundredths,
        overweight: plan.overweight,
        tracking,
    };
    let mut hash = Sha256::new();
    hash.update(account);
    hash.update(serde_json::to_vec(&input)?);
    Ok(hash.finalize().to_vec())
}
