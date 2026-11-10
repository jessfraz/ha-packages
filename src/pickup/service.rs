//! Durable pickup orchestration. HTTP handlers enqueue explicit user intent;
//! one worker performs read-only checks, fences dispatch, then records the reply.
//! An interrupted dispatch is never reclaimed for another physical request.

use std::{
    collections::{BTreeMap, HashMap, HashSet},
    sync::Arc,
};

use schemars::JsonSchema;
use serde::Serialize;
use uuid::Uuid;

use crate::pickup::{
    Capabilities, Error, ErrorCode, Lookup, Pickup, Result, Settings, fedex::FedEx, models::*,
    store::Store, ups::Ups, usps::Usps,
};

#[derive(Clone)]
pub struct Manager {
    store: Store,
    providers: Vec<Arc<dyn Pickup>>,
    configuration_id: Uuid,
}

#[derive(Serialize, serde::Deserialize, JsonSchema)]
pub struct CarrierStatus {
    pub carrier: PickupCarrier,
    pub configured: bool,
    pub capabilities: Capabilities,
    pub configuration_id: Option<Uuid>,
}

impl Manager {
    pub fn unconfigured(store: &crate::store::Store) -> Self {
        Self {
            store: Store::new(store.pool().clone()),
            providers: Vec::new(),
            configuration_id: Uuid::now_v7(),
        }
    }
    pub fn new(store: &crate::store::Store, settings: Settings) -> Result<Self> {
        let mut manager = Self::unconfigured(store);
        if let Some(credentials) = settings.usps {
            manager = manager.with_provider(Arc::new(Usps::new(credentials)?));
        }
        if let Some(credentials) = settings.ups {
            manager = manager.with_provider(Arc::new(Ups::new(credentials)?));
        }
        if let Some(credentials) = settings.fedex {
            manager = manager.with_provider(Arc::new(FedEx::new(credentials)?));
        }
        Ok(manager)
    }
    pub fn with_provider(mut self, provider: Arc<dyn Pickup>) -> Self {
        self.providers.retain(|p| p.carrier() != provider.carrier());
        self.providers.push(provider);
        // A browser review belongs to this loaded configuration. A random
        // generation reveals no account identifiers and changes on restart;
        // durable jobs keep their separate, stable billing fingerprint.
        self.configuration_id = Uuid::now_v7();
        self
    }
    fn provider(&self, carrier: PickupCarrier) -> Result<Arc<dyn Pickup>> {
        self.providers
            .iter()
            .find(|p| p.carrier() == carrier)
            .cloned()
            .ok_or_else(|| Error::new(ErrorCode::NotConfigured))
    }
    async fn observe<T>(&self, provider: &dyn Pickup, result: Result<T>) -> Result<T> {
        if let Err(error) = &result
            && let Some(seconds) = error.retry_after_seconds
        {
            // Preserve physical uncertainty if recording a carrier deadline
            // also fails. It must never become a definite retryable rejection.
            if self
                .store
                .defer(&provider.fingerprint(), seconds)
                .await
                .is_err()
            {
                return Err(Error::new(if error.code == ErrorCode::UnknownOutcome {
                    ErrorCode::UnknownOutcome
                } else {
                    ErrorCode::StorageUnavailable
                }));
            }
        }
        result
    }
    pub fn carriers(&self) -> Vec<CarrierStatus> {
        PickupCarrier::ALL
            .into_iter()
            .map(|carrier| {
                let provider = self.providers.iter().find(|p| p.carrier() == carrier);
                let capabilities = provider.map_or_else(
                    || Capabilities {
                        lookup: match carrier {
                            PickupCarrier::Usps => Lookup::Confirmation,
                            PickupCarrier::Ups => Lookup::Pending,
                            PickupCarrier::FedEx => Lookup::Unsupported,
                        },
                        free_normal_route: carrier == PickupCarrier::Usps,
                        custom_window: carrier != PickupCarrier::Usps,
                        services: PickupService::ALL
                            .into_iter()
                            .filter(|s| s.carrier() == carrier)
                            .collect(),
                    },
                    |p| p.capabilities(),
                );
                CarrierStatus {
                    carrier,
                    configured: provider.is_some(),
                    capabilities,
                    configuration_id: provider.map(|_| self.configuration_id),
                }
            })
            .collect()
    }
    pub async fn status(&self) -> Result<PickupStatus> {
        self.store.status().await
    }
    async fn request_site(&self, request: &ScheduleRequest) -> Result<Site> {
        if let Some(reviewed) = &request.reviewed_site {
            if request.site_id.is_some_and(|id| id != reviewed.id) {
                return Err(invalid());
            }
            let site = self.store.site(Some(reviewed.id)).await?;
            if reviewed.revision < 1 || site.revision != reviewed.revision {
                return Err(Error::new(ErrorCode::LocationChanged));
            }
            return Ok(site);
        }
        self.store.site(request.site_id).await
    }
    pub async fn check_reviewed_plan(&self, request: &ScheduleRequest) -> Result<()> {
        self.check_reviewed_configuration(request)?;
        if request.reviewed_site.is_some() {
            self.request_site(request).await?;
        }
        Ok(())
    }
    fn check_reviewed_configuration(&self, request: &ScheduleRequest) -> Result<()> {
        if request
            .reviewed_configuration
            .is_some_and(|reviewed| reviewed != self.configuration_id)
        {
            return Err(Error::new(ErrorCode::ConfigurationChanged));
        }
        Ok(())
    }
    pub async fn replay(
        &self,
        actor: &str,
        key: &str,
        request: &ScheduleRequest,
    ) -> Result<Option<Batch>> {
        self.store.existing_batch(actor, key, request).await
    }
    pub async fn sites(&self) -> Result<Vec<Site>> {
        self.store.sites().await
    }
    pub async fn save_site(
        &self,
        id: Option<Uuid>,
        revision: Option<i64>,
        input: SiteInput,
        make_default: bool,
    ) -> Result<Site> {
        self.store
            .save_site(id, revision, input, make_default)
            .await
    }
    pub async fn save_parcel(&self, tracking: &str, input: ParcelInput) -> Result<Parcel> {
        self.store.save_parcel(tracking, input).await
    }
    pub async fn parcel(
        &self,
        tracking: &str,
        carrier: Option<PickupCarrier>,
    ) -> Result<Option<Parcel>> {
        self.store.parcel(tracking, carrier).await
    }
    pub async fn get(&self, id: Uuid) -> Result<Record> {
        self.store.get(id).await
    }
    pub async fn resolve(&self, reference: &str, carrier: Option<PickupCarrier>) -> Result<Record> {
        self.store.resolve(reference, carrier).await
    }
    pub async fn batch(&self, id: Uuid) -> Result<Batch> {
        self.store.batch(id).await
    }
    pub async fn list(&self, after: Option<Uuid>, limit: u32) -> Result<Vec<Record>> {
        self.store.list(after, limit).await
    }
    pub async fn watched(
        &self,
        tracking: &[String],
    ) -> Result<Vec<(String, crate::carrier::Carrier, bool)>> {
        self.store.watched(tracking).await
    }

    async fn plans(
        &self,
        request: &ScheduleRequest,
        require_watch: bool,
    ) -> Result<Vec<ResolvedPlan>> {
        self.check_reviewed_configuration(request)?;
        if request.tracking.len() > 100 {
            return Err(invalid());
        }
        if let Some(date) = &request.date {
            validate_date(date)?;
        }
        let declared_carrier = request
            .carrier
            .or_else(|| request.service.map(PickupService::carrier));
        let tracking = request
            .tracking
            .iter()
            .map(|s| normalize_tracking(s))
            .collect::<Result<Vec<_>>>()?;
        if tracking.iter().collect::<HashSet<_>>().len() != tracking.len() {
            return Err(invalid());
        }
        let site = self.request_site(request).await?;
        let ready = request
            .ready_time
            .as_ref()
            .unwrap_or(&site.details.ready_time);
        let close = request
            .close_time
            .as_ref()
            .unwrap_or(&site.details.close_time);
        validate_window(ready, close)?;
        let watched = self
            .store
            .watched(&tracking)
            .await?
            .into_iter()
            .map(|(number, carrier, ready)| (number, (carrier, ready)))
            .collect::<HashMap<_, _>>();
        if require_watch
            && tracking
                .iter()
                .any(|number| watched.get(number).is_none_or(|(_, ready)| !*ready))
        {
            return Err(Error::new(ErrorCode::WatchNotReady));
        }
        let parcels = self.store.parcels(&tracking).await?;
        let mut groups: BTreeMap<&'static str, ResolvedPlan> = BTreeMap::new();
        if tracking.is_empty() {
            let carrier = declared_carrier.ok_or_else(invalid)?;
            if carrier == PickupCarrier::Usps
                && (request.ready_time.is_some() || request.close_time.is_some())
            {
                return Err(Error::new(ErrorCode::Unsupported));
            }
            let service = request
                .service
                .ok_or_else(|| Error::new(ErrorCode::ParcelDetailsRequired))?;
            let weight = request
                .weight_lbs
                .as_deref()
                .ok_or_else(|| Error::new(ErrorCode::ParcelDetailsRequired))?;
            let pieces = request.pieces.unwrap_or(1);
            // A total above70lb cannot establish whether an individual parcel
            // exceeds carrier limits. Normal multi-label jobs use saved weights.
            let maximum = if carrier == PickupCarrier::Usps || pieces > 1 {
                7000
            } else {
                15000
            };
            if pieces == 0 || pieces > 100 || parse_weight(weight)? > maximum {
                return Err(invalid());
            }
            let details = ParcelInput {
                carrier,
                service,
                weight_lbs: weight.into(),
                destination_country: request
                    .destination_country
                    .clone()
                    .unwrap_or_else(|| "US".into()),
            };
            let weight = details.validate()?;
            self.provider(carrier)?;
            return Ok(vec![ResolvedPlan {
                reference: Uuid::now_v7(),
                carrier,
                site: site.clone(),
                date: request.date.clone(),
                next_available: request.date.is_none(),
                ready_time: ready.clone(),
                close_time: close.clone(),
                parcels: vec![ParcelGroup {
                    service,
                    count: pieces,
                    destination_country: details.destination_country,
                }],
                total_weight_hundredths: weight,
                overweight: weight > 7000,
                tracking: Vec::new(),
            }]);
        }
        if request
            .pieces
            .is_some_and(|pieces| usize::from(pieces) != tracking.len())
            || (tracking.len() > 1 && request.weight_lbs.is_some())
        {
            return Err(Error::new(ErrorCode::ParcelDetailsRequired));
        }
        for number in tracking {
            let confirmed = watched.get(&number).and_then(|(carrier, ready)| {
                ready
                    .then(|| PickupCarrier::from_tracking(*carrier))
                    .flatten()
            });
            if watched.get(&number).is_some_and(|(carrier, ready)| {
                *ready && PickupCarrier::from_tracking(*carrier).is_none()
            }) {
                return Err(Error::new(ErrorCode::Unsupported));
            }
            if declared_carrier
                .zip(confirmed)
                .is_some_and(|(explicit, actual)| explicit != actual)
            {
                return Err(Error::new(ErrorCode::Conflict));
            }
            let saved: Vec<_> = parcels
                .iter()
                .filter(|p| {
                    p.tracking == number
                        && declared_carrier
                            .or(confirmed)
                            .is_none_or(|carrier| carrier == p.details.carrier)
                })
                .collect();
            if saved.len() > 1 {
                return Err(Error::new(ErrorCode::Ambiguous));
            }
            let saved = saved.first().copied();
            let carrier = declared_carrier
                .or(confirmed)
                .or_else(|| saved.map(|s| s.details.carrier))
                .ok_or_else(|| Error::new(ErrorCode::Ambiguous))?;
            if carrier == PickupCarrier::Usps
                && (request.ready_time.is_some() || request.close_time.is_some())
            {
                return Err(Error::new(ErrorCode::Unsupported));
            }
            let service = request
                .service
                .or_else(|| saved.map(|s| s.details.service))
                .ok_or_else(|| Error::new(ErrorCode::ParcelDetailsRequired))?;
            let weight = request
                .weight_lbs
                .as_deref()
                .or_else(|| saved.map(|s| s.details.weight_lbs.as_str()))
                .ok_or_else(|| Error::new(ErrorCode::ParcelDetailsRequired))?;
            let destination = request
                .destination_country
                .as_deref()
                .or_else(|| saved.map(|s| s.details.destination_country.as_str()))
                .unwrap_or("US");
            let details = ParcelInput {
                carrier,
                service,
                weight_lbs: weight.into(),
                destination_country: destination.into(),
            };
            let weight = details.validate()?;
            let maximum = if carrier == PickupCarrier::Usps {
                7000
            } else {
                15000
            };
            if weight > maximum {
                return Err(Error::new(ErrorCode::Unsupported));
            }
            self.provider(carrier)?;
            if require_watch
                && (request.service.is_some()
                    || request.weight_lbs.is_some()
                    || request.destination_country.is_some())
            {
                self.store.save_parcel(&number, details).await?;
            }
            let plan = groups
                .entry(service.group())
                .or_insert_with(|| ResolvedPlan {
                    reference: Uuid::now_v7(),
                    carrier,
                    site: site.clone(),
                    date: request.date.clone(),
                    next_available: request.date.is_none(),
                    ready_time: ready.clone(),
                    close_time: close.clone(),
                    parcels: Vec::new(),
                    total_weight_hundredths: 0,
                    overweight: false,
                    tracking: Vec::new(),
                });
            plan.overweight |= weight > 7000;
            plan.total_weight_hundredths = plan
                .total_weight_hundredths
                .checked_add(weight)
                .ok_or_else(invalid)?;
            plan.tracking.push(number);
            if let Some(group) = plan
                .parcels
                .iter_mut()
                .find(|p| p.service == service && p.destination_country == destination)
            {
                group.count = group.count.checked_add(1).ok_or_else(invalid)?;
            } else {
                plan.parcels.push(ParcelGroup {
                    service,
                    count: 1,
                    destination_country: destination.into(),
                });
            }
        }
        Ok(groups.into_values().collect())
    }

    pub async fn preview(&self, request: &ScheduleRequest) -> Result<Vec<Preview>> {
        let plans = self.plans(request, false).await?;
        let mut result = Vec::new();
        for plan in plans {
            let provider = self.provider(plan.carrier)?;
            self.store.check_cooldown(&provider.fingerprint()).await?;
            let offer = self
                .observe(provider.as_ref(), provider.preview(&plan).await)
                .await?;
            result.push(Preview {
                plan,
                offer,
                configuration_id: self.configuration_id,
            });
        }
        Ok(result)
    }

    pub async fn availability(&self, request: &ScheduleRequest) -> Result<Offer> {
        let carrier = request.carrier.ok_or_else(invalid)?;
        if carrier == PickupCarrier::Usps
            && (request.ready_time.is_some() || request.close_time.is_some())
        {
            return Err(Error::new(ErrorCode::Unsupported));
        }
        let site = self.request_site(request).await?;
        let ready_time = request
            .ready_time
            .clone()
            .unwrap_or_else(|| site.details.ready_time.clone());
        let close_time = request
            .close_time
            .clone()
            .unwrap_or_else(|| site.details.close_time.clone());
        validate_window(&ready_time, &close_time)?;
        let parcels = if let Some(service) = request.service {
            if service.carrier() != carrier {
                return Err(invalid());
            }
            vec![ParcelGroup {
                service,
                count: 1,
                destination_country: request
                    .destination_country
                    .clone()
                    .unwrap_or_else(|| "US".into()),
            }]
        } else if carrier == PickupCarrier::FedEx {
            return Err(Error::new(ErrorCode::ParcelDetailsRequired));
        } else {
            Vec::new()
        };
        let plan = ResolvedPlan {
            reference: Uuid::now_v7(),
            carrier,
            site,
            date: request.date.clone(),
            next_available: request.date.is_none(),
            ready_time,
            close_time,
            parcels,
            total_weight_hundredths: 0,
            overweight: false,
            tracking: Vec::new(),
        };
        let provider = self.provider(carrier)?;
        self.store.check_cooldown(&provider.fingerprint()).await?;
        self.observe(provider.as_ref(), provider.preview(&plan).await)
            .await
    }

    pub async fn schedule(
        &self,
        actor: &str,
        key: &str,
        request: &ScheduleRequest,
    ) -> Result<Batch> {
        if let Some(batch) = self.store.existing_batch(actor, key, request).await? {
            return Ok(batch);
        }
        let plans = self.plans(request, true).await?;
        let plans = plans
            .into_iter()
            .map(|plan| {
                let fingerprint = self.provider(plan.carrier)?.fingerprint();
                Ok((plan, fingerprint))
            })
            .collect::<Result<_>>()?;
        self.store.create_batch(actor, key, request, plans).await
    }

    pub async fn cancel(&self, id: Uuid, revision: i64, actor: &str, key: &str) -> Result<Record> {
        let record = self.store.get(id).await?;
        let provider = self.provider(record.plan.carrier)?;
        self.store
            .claim_cancel(id, revision, actor, key, &provider.fingerprint())
            .await?;
        self.store.get(id).await
    }

    pub async fn refresh(&self, id: Uuid, revision: i64) -> Result<Record> {
        let record = self.store.get(id).await?;
        if record.revision != revision {
            return Err(Error::new(ErrorCode::Conflict));
        }
        let provider = self.provider(record.plan.carrier)?;
        if !self
            .store
            .account_matches(id, &provider.fingerprint())
            .await?
        {
            return Err(Error::new(ErrorCode::Conflict));
        }
        let saved = record
            .confirmation
            .as_ref()
            .ok_or_else(|| Error::new(ErrorCode::Unsupported))?;
        self.store.check_cooldown(&provider.fingerprint()).await?;
        let confirmation = self
            .observe(
                provider.as_ref(),
                provider.lookup(&record.plan, saved).await,
            )
            .await?;
        self.store.refresh(id, revision, &confirmation).await
    }

    pub async fn reconcile(
        &self,
        id: Uuid,
        revision: i64,
        actor: &str,
        input: ReconcileRequest,
    ) -> Result<Record> {
        let record = self.store.get(id).await?;
        if record.revision != revision || !input.carrier_portal_checked {
            return Err(Error::new(ErrorCode::Conflict));
        }
        let provider = self.provider(record.plan.carrier)?;
        if let ReconcileOutcome::Scheduled { confirmation } = &input.outcome {
            text(&confirmation.number, 100)?;
            validate_date(&confirmation.pickup_date)?;
            if !confirmation
                .number
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-')
                || confirmation.location_code.as_ref().is_some_and(|s| {
                    s.is_empty()
                        || s.len() > 100
                        || !s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
                })
            {
                return Err(invalid());
            }
            if provider.capabilities().lookup != Lookup::Unsupported {
                self.store.check_cooldown(&provider.fingerprint()).await?;
                self.observe(
                    provider.as_ref(),
                    provider.lookup(&record.plan, confirmation).await,
                )
                .await?;
            } else if record
                .plan
                .parcels
                .iter()
                .any(|p| p.service.group() == "fedex_express")
                && confirmation.location_code.is_none()
            {
                return Err(invalid());
            }
        }
        self.store
            .reconcile(&record, actor, &input, &provider.fingerprint())
            .await
    }

    pub async fn tick(&self, mut stopped: tokio::sync::watch::Receiver<bool>) -> Result<bool> {
        if *stopped.borrow() {
            return Ok(false);
        }
        let Some(record) = self.store.pending().await? else {
            return Ok(false);
        };
        let provider = match self.provider(record.plan.carrier) {
            Ok(provider) => provider,
            Err(error) => {
                self.store
                    .finish(
                        record.id,
                        record.state,
                        if record.state == State::CancelPending {
                            State::Scheduled
                        } else {
                            State::Rejected
                        },
                        None,
                        Some(&error),
                    )
                    .await?;
                return Ok(true);
            }
        };
        if !self
            .store
            .account_matches(record.id, &provider.fingerprint())
            .await?
        {
            self.store
                .finish(
                    record.id,
                    record.state,
                    if record.state == State::CancelPending {
                        State::Scheduled
                    } else {
                        State::Rejected
                    },
                    None,
                    Some(&Error::new(ErrorCode::Conflict)),
                )
                .await?;
            return Ok(true);
        }
        if let Err(error) = self.store.check_cooldown(&provider.fingerprint()).await {
            if error.code == ErrorCode::RateLimited {
                self.store
                    .finish(
                        record.id,
                        record.state,
                        if record.state == State::CancelPending {
                            State::Scheduled
                        } else {
                            State::Rejected
                        },
                        None,
                        Some(&error),
                    )
                    .await?;
                return Ok(true);
            }
            return Err(error);
        }
        if record.state == State::CancelPending {
            let saved = record
                .confirmation
                .as_ref()
                .ok_or_else(|| Error::new(ErrorCode::StorageUnavailable))?;
            if !self
                .store
                .begin_cancel(record.id, record.revision, &provider.fingerprint())
                .await?
            {
                return Ok(true);
            }
            // Once fenced, allow this bounded physical call to drain during
            // normal shutdown. Cancelling the future would lose its receipt.
            match self
                .observe(
                    provider.as_ref(),
                    provider.cancel(&record.plan, saved).await,
                )
                .await
            {
                Ok(()) => {
                    self.store
                        .finish(record.id, State::Cancelling, State::Cancelled, None, None)
                        .await?
                }
                Err(error) => {
                    self.store
                        .finish(
                            record.id,
                            State::Cancelling,
                            if error.code == ErrorCode::UnknownOutcome {
                                State::Unknown
                            } else {
                                State::Scheduled
                            },
                            None,
                            Some(&error),
                        )
                        .await?
                }
            }
            return Ok(true);
        }
        let batch = self.store.batch(record.batch_id).await?;
        let tracking = batch
            .items
            .iter()
            .flat_map(|r| r.plan.tracking.iter().cloned())
            .collect::<Vec<_>>();
        let watched = self.store.watched(&tracking).await?;
        if tracking.iter().any(|number| {
            watched
                .iter()
                .find(|(value, _, _)| value == number)
                .is_none_or(|(_, _, ready)| !*ready)
        }) {
            self.store
                .finish(
                    record.id,
                    State::Pending,
                    State::Rejected,
                    None,
                    Some(&Error::new(ErrorCode::WatchNotReady)),
                )
                .await?;
            return Ok(true);
        }
        let offer = tokio::select! {_ = stopped.changed()=>return Ok(false), offer=provider.preview(&record.plan)=>offer};
        let offer = match self.observe(provider.as_ref(), offer).await {
            Ok(offer) => offer,
            Err(error) => {
                self.store
                    .finish(
                        record.id,
                        State::Pending,
                        State::Rejected,
                        None,
                        Some(&error),
                    )
                    .await?;
                return Ok(true);
            }
        };
        if *stopped.borrow() {
            return Ok(false);
        }
        let mut plan = record.plan;
        plan.date = Some(offer.date.clone());
        plan.ready_time = offer.ready_time.clone();
        plan.close_time = offer.close_time.clone();
        match self
            .store
            .dispatch(
                record.id,
                record.revision,
                &plan,
                &offer,
                &provider.fingerprint(),
            )
            .await
        {
            Ok(true) => {}
            Ok(false) => return Ok(true),
            Err(error) if error.code == ErrorCode::StorageUnavailable => return Err(error),
            Err(error) => {
                self.store
                    .finish(
                        record.id,
                        State::Pending,
                        State::Rejected,
                        None,
                        Some(&error),
                    )
                    .await?;
                return Ok(true);
            }
        }
        match self
            .observe(provider.as_ref(), provider.schedule(&plan).await)
            .await
        {
            Ok(confirmation) => {
                self.store
                    .finish(
                        record.id,
                        State::Dispatching,
                        State::Scheduled,
                        Some(&confirmation),
                        None,
                    )
                    .await?
            }
            Err(error) => {
                self.store
                    .finish(
                        record.id,
                        State::Dispatching,
                        if error.code == ErrorCode::UnknownOutcome {
                            State::Unknown
                        } else {
                            State::Rejected
                        },
                        None,
                        Some(&error),
                    )
                    .await?
            }
        }
        Ok(true)
    }
}
