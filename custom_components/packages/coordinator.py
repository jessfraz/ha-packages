"""Coordinate shipment counts, dashboard items and installation check-ins."""

import asyncio
import json
import logging
from collections.abc import Callable
from datetime import datetime, timedelta
from typing import Literal, NotRequired, TypedDict
from urllib.parse import quote, urlencode
from uuid import UUID

from aiohttp import ClientError
from homeassistant.components import persistent_notification
from homeassistant.config_entries import ConfigEntry, ConfigEntryState
from homeassistant.core import CALLBACK_TYPE, HomeAssistant, callback
from homeassistant.exceptions import ConfigEntryAuthFailed, HomeAssistantError
from homeassistant.helpers import issue_registry as ir
from homeassistant.helpers.event import async_track_time_interval
from homeassistant.helpers.hassio import is_hassio
from homeassistant.helpers.storage import Store
from homeassistant.helpers.update_coordinator import DataUpdateCoordinator, UpdateFailed
from homeassistant.util import dt as dt_util

from ._app_kit.repairs import VersionRepairs
from ._app_kit.versions import ReleaseVersion
from .client import PackagesClient, ResponseTooLarge
from .const import (
    CONF_ADDON_SLUG,
    CONF_BACKEND_URL,
    DIRECTION_SUMMARY_KEYS,
    DOMAIN,
    SUMMARY_KEYS,
)
from .health import DiscoveryHealth, DiscoveryState
from .models import (
    DeliveryDay,
    DeliveryDayReport,
    DeliveryEstimate,
    Direction,
    ShipmentItem,
    ShipmentItems,
    ShipmentStatus,
    ShipmentStatusEvent,
    delivery_estimate,
)
from .pickups import PickupReport
from .repairs import VERSION_REPAIR

_LOGGER = logging.getLogger(__name__)
_BACKUP_TIMEOUT_SECONDS = 10
_ITEM_LIMIT = 20
_AMAZON_STARTUP_GRACE = timedelta(minutes=3)
type _AmazonUnavailableReason = Literal[
    "backend_unavailable",
    "health_report_missing",
    "health_report_invalid",
    "health_report_expired",
    "health_report_future",
]


def amazon_notification_id(entry_id: str) -> str:
    """Identify the legacy account notice for migration and removal."""
    return f"{DOMAIN}_{entry_id}_amazon_auth_required"


def amazon_discovery_issue_id(entry_id: str) -> str:
    """Keep one account-owned Repair stable through refreshes and reloads."""
    return f"amazon_discovery_{entry_id}"


class BackupReport(TypedDict):
    """Only backup coverage and date leave Home Assistant."""

    status: Literal[
        "complete", "app_only", "missing", "outdated", "unavailable", "standalone"
    ]
    last_backup: int | None


class PresenceReport(TypedDict):
    """A loaded integration's minimal authenticated installation evidence."""

    version: str
    backup: BackupReport
    administrator_ids: list[str]
    delivery_day: NotRequired[DeliveryDayReport]


class EventCheckpoint(TypedDict):
    """Only a numeric backend cursor is persisted by the adapter."""

    after: int


class PackagesCoordinator(DataUpdateCoordinator[dict[str, int]]):
    """Poll combined and direction-specific summaries for all count sensors."""

    def __init__(
        self,
        hass: HomeAssistant,
        entry: ConfigEntry,
        client: PackagesClient,
    ) -> None:
        super().__init__(
            hass,
            _LOGGER,
            name=DOMAIN,
            config_entry=entry,
            update_interval=timedelta(seconds=60),
            always_update=True,
        )
        self.client = client
        self._entry = entry
        self._version = str(VERSION_REPAIR.loaded_version)
        self._version_repairs: VersionRepairs | None = None
        self.last_backend_version: str | None = None
        self.inbound_estimates: tuple[DeliveryEstimate, ...] = ()
        self.pickup_report: PickupReport | None = None
        self.shipment_items: dict[str, ShipmentItems] = {}
        self._event_store = Store[EventCheckpoint](
            hass, 1, f"{DOMAIN}.{entry.entry_id}.events", atomic_writes=True
        )
        self._checkpoint_loaded = False
        self._after: int | None = None
        self._event_listeners: list[Callable[[ShipmentStatusEvent], None]] = []
        self.event_available = False
        self.amazon_discovery: DiscoveryHealth | None = None
        self._amazon_repair_active = True
        self._amazon_started_at = dt_util.utcnow()
        self._amazon_seen_current = False
        self._amazon_last_report: DiscoveryHealth | None = None
        self._amazon_account_report: object = None
        self._amazon_report_problem: _AmazonUnavailableReason | None = (
            "health_report_missing"
        )

        @callback
        def stop_amazon_repairs() -> None:
            self._amazon_repair_active = False

        entry.async_on_unload(stop_amazon_repairs)

    @callback
    def async_start_version_repairs(self) -> None:
        """Own the version repair after runtime data is assigned."""
        self._version_repairs = VersionRepairs(self.hass, self._entry, VERSION_REPAIR)
        self._version_repairs.async_start()

    @callback
    def async_start_amazon_repairs(self) -> None:
        """Keep source polling and freshness checks independent of enabled entities."""
        self._entry.async_on_unload(self.async_add_listener(self._async_amazon_update))
        self._entry.async_on_unload(
            async_track_time_interval(
                self.hass, self._async_amazon_freshness, timedelta(seconds=30)
            )
        )
        self._async_amazon_update()

    @property
    def _amazon_repairs_allowed(self) -> bool:
        return (
            self._amazon_repair_active
            and self.hass.config_entries.async_get_entry(self._entry.entry_id)
            is self._entry
            and self._entry.state
            in {ConfigEntryState.SETUP_IN_PROGRESS, ConfigEntryState.LOADED}
        )

    @callback
    def _async_amazon_update(self) -> None:
        if not self._amazon_repairs_allowed:
            return
        if not self.last_update_success:
            self._async_amazon_unavailable("backend_unavailable")
            return
        self._async_amazon_repair(self._amazon_account_report, allow_clear=True)
        if self._amazon_report_problem is not None:
            self._async_amazon_unavailable(self._amazon_report_problem)

    @callback
    def _async_amazon_freshness(self, _now: datetime) -> None:
        # This timer never declares recovery. Only a completed fresh poll can.
        report = self._amazon_last_report
        if report is not None and not report.current:
            self._async_amazon_unavailable(
                "health_report_future"
                if report.checked_at > dt_util.utcnow().timestamp()
                else "health_report_expired"
            )
        elif not self.last_update_success:
            self._async_amazon_unavailable("backend_unavailable")
        elif self._amazon_report_problem is not None:
            self._async_amazon_unavailable(self._amazon_report_problem)

    @property
    def _amazon_backend_reauth_required(self) -> bool:
        if not self.last_update_success and isinstance(
            self.last_exception, ConfigEntryAuthFailed
        ):
            return True
        # Core owns the credential Repair while its reauth flow remains open,
        # including after a subsequent connection failure replaces the exception.
        issue = ir.async_get(self.hass).async_get_issue(
            "homeassistant", f"config_entry_reauth_{DOMAIN}_{self._entry.entry_id}"
        )
        return issue is not None and issue.active

    @callback
    def _async_amazon_backend_auth(self) -> None:
        if not self._amazon_repairs_allowed:
            return
        registry = ir.async_get(self.hass)
        issue_id = amazon_discovery_issue_id(self._entry.entry_id)
        existing = registry.async_get_issue(DOMAIN, issue_id)
        # Backend credentials and Amazon cookies are independent. Only the
        # availability fallback is superseded by Core's backend reauth Repair.
        if (
            existing is not None
            and existing.translation_key == "amazon_discovery_unavailable"
        ):
            ir.async_delete_issue(self.hass, DOMAIN, issue_id)
        persistent_notification.async_dismiss(
            self.hass, amazon_notification_id(self._entry.entry_id)
        )

    @callback
    def _async_amazon_unavailable(self, reason: _AmazonUnavailableReason) -> None:
        if not self._amazon_repairs_allowed:
            return
        if self._amazon_backend_reauth_required:
            self._async_amazon_backend_auth()
            return
        registry = ir.async_get(self.hass)
        if self.last_backend_version not in (None, self._version):
            return
        existing = registry.async_get_issue(
            DOMAIN, amazon_discovery_issue_id(self._entry.entry_id)
        )
        # An unknown observation cannot replace a confirmed account/source fault.
        if (
            existing is not None
            and existing.translation_key != "amazon_discovery_unavailable"
        ):
            return
        if (
            existing is None
            and not self._amazon_seen_current
            and dt_util.utcnow() - self._amazon_started_at < _AMAZON_STARTUP_GRACE
        ):
            return
        self._async_set_amazon_issue(
            "amazon_discovery_unavailable", "unavailable", reason, 0
        )

    async def _async_update_data(self) -> dict[str, int]:
        # A prior successful feed cannot establish availability for this refresh.
        # The validated current page restores it before dispatching transitions.
        self.event_available = False
        try:
            # Diagnose a release mismatch before reading newer API projections.
            versions_match = await self._async_check_version()
            summary = await self._async_summary()
            for direction, keys in DIRECTION_SUMMARY_KEYS.items():
                counts = await self._async_summary(direction)
                summary.update({f"{direction}_{key}": counts[key] for key in keys})
            ofd = await self._async_items(
                summary["inbound_out_for_delivery"], complete=True
            )
            item_lists = {
                "inbound_out_for_delivery": ShipmentItems(
                    ofd.items[:_ITEM_LIMIT], len(ofd.items) > _ITEM_LIMIT
                )
            }
            day = None
            if versions_match:
                day = DeliveryDay.at(dt_util.utcnow(), self.hass.config.time_zone)
                delivered = await self._async_summary(Direction.INBOUND, day)
                summary["inbound_delivered_today"] = delivered["delivered"]
                item_lists["inbound_delivered_today"] = await self._async_items(
                    delivered["delivered"], day
                )
            estimates = await self._async_estimates()
            if day is not None:
                # Keep the OFD contract separate, including undated parcels.
                # The fully paginated estimates supply all other due-today items.
                expected = [
                    estimate.item()
                    for estimate in estimates
                    if estimate.status is not ShipmentStatus.OUT_FOR_DELIVERY
                    and estimate.due_on(day)
                ]
                items = {item.id: item for item in (*expected, *ofd.items)}
                count = len(items)
                ordered = sorted(
                    items.values(),
                    key=lambda item: (item.title.casefold(), str(item.id)),
                )
                summary["inbound_due_today"] = count
                item_lists["inbound_due_today"] = ShipmentItems(
                    tuple(ordered[:_ITEM_LIMIT]), count > _ITEM_LIMIT
                )
            await self._async_report_presence(day)
        except ConfigEntryAuthFailed:
            # Also runs during the first refresh, before the monitor is attached.
            self._async_amazon_backend_auth()
            raise
        except (ClientError, TimeoutError, ResponseTooLarge, ValueError) as err:
            raise UpdateFailed("Cannot read the backend state") from err
        try:
            await self._async_events()
        except (
            ClientError,
            TimeoutError,
            ResponseTooLarge,
            ValueError,
            OSError,
            HomeAssistantError,
        ) as err:
            self.event_available = False
            ir.async_create_issue(
                self.hass,
                DOMAIN,
                "event_stream_unavailable",
                is_fixable=False,
                severity=ir.IssueSeverity.WARNING,
                translation_key="event_stream_unavailable",
            )
            if isinstance(err, ConfigEntryAuthFailed):
                self._entry.async_start_reauth(self.hass)
        self.pickup_report = None
        try:
            response = await self.client.request("GET", "/v1/pickups/status")
            if response.status == 200:
                self.pickup_report = PickupReport.parse(response.json())
        except ClientError, TimeoutError, ResponseTooLarge, ValueError:
            pass
        if self.pickup_report is not None:
            summary["pickups_scheduled"] = self.pickup_report.scheduled
            summary["pickups_uncertain"] = self.pickup_report.uncertain
            if self._entry.state is ConfigEntryState.LOADED:
                issue_id = f"pickup_uncertain_{self._entry.entry_id}"
                if self.pickup_report.uncertain:
                    ir.async_create_issue(
                        self.hass,
                        DOMAIN,
                        issue_id,
                        is_fixable=False,
                        severity=ir.IssueSeverity.WARNING,
                        translation_key="pickup_uncertain",
                    )
                else:
                    ir.async_delete_issue(self.hass, DOMAIN, issue_id)
        self.inbound_estimates = estimates
        self.shipment_items = item_lists
        return summary

    async def _async_items(
        self, count: int, day: DeliveryDay | None = None, *, complete: bool = False
    ) -> ShipmentItems:
        """Read one bounded page, using the count's exact filters and day bounds."""
        query = {
            "direction": Direction.INBOUND,
            "archived": "false",
            "limit": str(_ITEM_LIMIT),
            "sort": "delivered" if day is not None else "expected",
        }
        if day is not None:
            query.update(day.query())
        else:
            query["status"] = "out_for_delivery"
        items: list[ShipmentItem] = []
        cursors: set[str] = set()
        while True:
            response = await self.client.request(
                "GET", f"/v1/packages?{urlencode(query)}"
            )
            if response.status in {401, 403}:
                raise ConfigEntryAuthFailed("The backend rejected its credential")
            if response.status != 200:
                raise UpdateFailed("Cannot read inbound shipment items")
            data = response.json()
            if not isinstance(data, dict) or not isinstance(data.get("items"), list):
                raise ValueError("Invalid shipment item page")
            records = data["items"]
            cursor = data.get("next_page")
            remaining = count - len(items)
            if len(records) != min(remaining, _ITEM_LIMIT) or (cursor is not None) != (
                remaining > _ITEM_LIMIT
            ):
                raise ValueError("Shipment count and item page changed during refresh")
            items.extend(ShipmentItem.parse(record, day) for record in records)
            if len({item.id for item in items}) != len(items):
                raise ValueError("Duplicate shipment in item page")
            if cursor is None or not complete:
                return ShipmentItems(tuple(items), cursor is not None)
            if not isinstance(cursor, str) or not cursor or cursor in cursors:
                raise ValueError("Invalid shipment item cursor")
            cursors.add(cursor)
            query["page_token"] = cursor

    @callback
    def async_subscribe_event(
        self, listener: Callable[[ShipmentStatusEvent], None]
    ) -> CALLBACK_TYPE:
        """Subscribe while the native entity exists; unload removes its listener."""
        self._event_listeners.append(listener)

        @callback
        def unsubscribe() -> None:
            self._event_listeners.remove(listener)

        return unsubscribe

    async def async_read_system(self) -> tuple[ReleaseVersion, dict[str, object]]:
        """Read the version without updating other discovery state."""
        response = await self.client.request("GET", "/v1/system")
        if response.status in {401, 403}:
            raise ConfigEntryAuthFailed("The backend rejected its credential")
        if response.status != 200:
            raise UpdateFailed("The backend could not return its version")
        data = response.json()
        if not isinstance(data, dict):
            raise ValueError("Invalid backend version")
        return ReleaseVersion.parse(data.get("version")), data

    async def _async_check_version(self) -> bool:
        self.amazon_discovery = None
        version, data = await self.async_read_system()
        self.last_backend_version = str(version)
        if self._version_repairs is not None:
            self._version_repairs.async_report(version)
        if version != VERSION_REPAIR.loaded_version:
            return False
        raw_report = data.get("amazon_discovery")
        self.amazon_discovery = parsed = DiscoveryHealth.parse(raw_report)
        self._amazon_account_report = data.get("amazon")
        if parsed is None:
            self._amazon_report_problem = (
                "health_report_missing"
                if raw_report is None
                else "health_report_invalid"
            )
        else:
            self._amazon_last_report = parsed
            if parsed.current:
                self._amazon_seen_current = True
                self._amazon_report_problem = None
            else:
                self._amazon_report_problem = (
                    "health_report_future"
                    if parsed.checked_at > dt_util.utcnow().timestamp()
                    else "health_report_expired"
                )
        # Defer recovery until the completed poll so transport failures retain it.
        self._async_amazon_repair(self._amazon_account_report)
        return True

    @callback
    def _async_amazon_repair(
        self, account: object, *, allow_clear: bool = False
    ) -> None:
        """Keep confirmed problems until fresh recovery, including across outages."""
        if not self._amazon_repairs_allowed:
            return
        state: DiscoveryState | None = None
        if (
            isinstance(account, dict)
            and type(account.get("imports_enabled")) is bool
            and isinstance(account.get("state"), str)
            and account.get("state") in {"connected", "auth_required", "disconnected"}
            and type(account.get("challenge_required", False)) is bool
        ):
            if account["state"] == "auth_required":
                state = DiscoveryState.AUTH_REQUIRED
            elif account["state"] == "connected" and account.get("challenge_required"):
                state = DiscoveryState.CHALLENGE_REQUIRED
        report = self.amazon_discovery
        current = report is not None and report.current
        if state is None and current and report is not None and report.problem:
            state = report.state
        issue_id = amazon_discovery_issue_id(self._entry.entry_id)
        if state is None:
            if allow_clear and current and report is not None and not report.problem:
                ir.async_delete_issue(self.hass, DOMAIN, issue_id)
                persistent_notification.async_dismiss(
                    self.hass, amazon_notification_id(self._entry.entry_id)
                )
            return
        error_code = (
            report.last_error_code
            if current and report is not None and report.last_error_code is not None
            else "unspecified"
        )
        translation_key = (
            f"amazon_discovery_{state}"
            if state
            in {DiscoveryState.AUTH_REQUIRED, DiscoveryState.CHALLENGE_REQUIRED}
            else "amazon_discovery"
        )
        self._async_set_amazon_issue(
            translation_key,
            state.value,
            error_code,
            report.deferred_parcels if current and report is not None else 0,
        )

    @callback
    def _async_set_amazon_issue(
        self, translation_key: str, state: str, error_code: str, deferred: int
    ) -> None:
        destination = (
            "/packages/packages/amazon%3Dconnect"
            if self._entry.data.get(CONF_ADDON_SLUG)
            else f"{self._entry.data[CONF_BACKEND_URL]}/#amazon=connect"
        )
        integration_url = f"/config/integrations/integration/{DOMAIN}"
        slug = self._entry.data.get(CONF_ADDON_SLUG)
        logs_url = (
            f"/config/app/{quote(slug, safe='')}/logs"
            if isinstance(slug, str) and slug
            else "/config/logs"
        )
        ir.async_create_issue(
            self.hass,
            DOMAIN,
            amazon_discovery_issue_id(self._entry.entry_id),
            config_entry_id=self._entry.entry_id,
            is_fixable=False,
            is_persistent=True,
            severity=ir.IssueSeverity.WARNING,
            translation_key=translation_key,
            translation_placeholders={
                "state": state,
                "error_code": error_code,
                "deferred_parcels": str(deferred),
                "settings_url": destination,
                "logs_url": logs_url,
                "diagnostics_url": integration_url,
            },
            learn_more_url=integration_url,
        )
        persistent_notification.async_dismiss(
            self.hass, amazon_notification_id(self._entry.entry_id)
        )

    async def _async_estimates(self) -> tuple[DeliveryEstimate, ...]:
        estimates: list[DeliveryEstimate] = []
        identities: set[UUID] = set()
        cursors: set[str] = set()
        cursor: str | None = None
        while True:
            query = {"limit": "200"}
            if cursor is not None:
                query["page_token"] = cursor
            response = await self.client.request(
                "GET", f"/v1/delivery-estimates?{urlencode(query)}"
            )
            if response.status in {401, 403}:
                raise ConfigEntryAuthFailed("The backend rejected its credential")
            if response.status != 200:
                raise UpdateFailed("Cannot read inbound delivery estimates")
            data = response.json()
            if not isinstance(data, dict) or not isinstance(data.get("items"), list):
                raise ValueError("Invalid delivery estimate page")
            for record in data["items"]:
                identity, estimate = delivery_estimate(record)
                if identity in identities:
                    raise ValueError("Duplicate shipment in delivery pages")
                identities.add(identity)
                if estimate is not None:
                    estimates.append(estimate)
            cursor = data.get("next_page")
            if cursor is None:
                return tuple(estimates)
            if not isinstance(cursor, str) or not cursor or cursor in cursors:
                raise ValueError("Invalid delivery estimate cursor")
            cursors.add(cursor)

    async def _async_events(self) -> None:
        """Emit durable changes after entity setup, checkpointing after delivery."""
        if (
            self._entry.state is not ConfigEntryState.LOADED
            or not self._event_listeners
        ):
            return
        if not self._checkpoint_loaded:
            checkpoint = await self._event_store.async_load()
            if checkpoint is not None:
                if not isinstance(checkpoint, dict):
                    raise ValueError("Invalid persisted shipment event checkpoint")
                after = checkpoint.get("after")
                if type(after) is not int or after < 0:
                    raise ValueError("Invalid persisted shipment event cursor")
                self._after = after
            self._checkpoint_loaded = True
        while self._entry.state is ConfigEntryState.LOADED and self._event_listeners:
            query = {"limit": "200"}
            if self._after is not None:
                query["after"] = str(self._after)
            response = await self.client.request(
                "GET", f"/v1/events?{urlencode(query)}"
            )
            if response.status == 409 and self._after is not None:
                # An app-only database restore can put the durable HA cursor
                # ahead of retained history. Warn and bootstrap at its new tail.
                ir.async_create_issue(
                    self.hass,
                    DOMAIN,
                    "event_history_reset",
                    is_fixable=False,
                    severity=ir.IssueSeverity.WARNING,
                    translation_key="event_history_reset",
                )
                self._after = None
                continue
            if response.status in {401, 403}:
                raise ConfigEntryAuthFailed("The backend rejected its credential")
            if response.status != 200:
                raise UpdateFailed("Cannot read shipment status events")
            data = response.json()
            if not isinstance(data, dict) or not isinstance(data.get("items"), list):
                raise ValueError("Invalid shipment event page")
            events = [ShipmentStatusEvent.parse(record) for record in data["items"]]
            next_after = data.get("next_after")
            has_more = data.get("has_more")
            if (
                type(next_after) is not int
                or next_after < 0
                or type(has_more) is not bool
            ):
                raise ValueError("Invalid shipment event cursor")
            previous = self._after
            if previous is None and (events or has_more):
                raise ValueError("Event bootstrap must start at the current tail")
            for event in events:
                if previous is not None and event.id <= previous:
                    raise ValueError("Shipment events must be strictly ordered")
                previous = event.id
            if (previous is not None and next_after < previous) or (
                has_more and not events
            ):
                raise ValueError("Shipment event cursor did not advance")
            if events and next_after != events[-1].id:
                raise ValueError("Shipment event cursor skipped unseen events")
            if (
                self._entry.state is not ConfigEntryState.LOADED
                or not self._event_listeners
            ):
                return
            # Restore availability before dispatching every queued transition;
            # otherwise EventEntity would hide all but the last replayed event.
            self.event_available = True
            for event in events:
                for listener in tuple(self._event_listeners):
                    listener(event)
            if next_after != self._after:
                # Delivery precedes the durable checkpoint. A crash can replay
                # an event, so consumers deduplicate using event_id.
                await self._event_store.async_save({"after": next_after})
                if await self._event_store.async_load() != {"after": next_after}:
                    raise OSError("The shipment event cursor could not be persisted")
                self._after = next_after
            if not has_more:
                self.event_available = True
                ir.async_delete_issue(self.hass, DOMAIN, "event_stream_unavailable")
                return

    async def _async_summary(
        self, direction: Direction | None = None, day: DeliveryDay | None = None
    ) -> dict[str, int]:
        """Validate every summary before publishing a complete refresh."""
        path = "/v1/summary"
        label = "summary"
        query: dict[str, str] = {}
        if direction is not None:
            query["direction"] = direction
            label = f"{direction} summary"
        if day is not None:
            query.update(day.query())
            label += " for the local delivery day"
        if query:
            path += f"?{urlencode(query)}"
        response = await self.client.request("GET", path)
        if response.status in {401, 403}:
            raise ConfigEntryAuthFailed("The backend rejected its credential")
        if response.status != 200:
            raise UpdateFailed(f"The backend could not return its {label}")
        data = response.json()
        if not isinstance(data, dict) or any(
            type(data.get(key)) is not int or data[key] < 0 for key in SUMMARY_KEYS
        ):
            raise UpdateFailed(f"The backend returned an invalid {label}")
        return {key: data[key] for key in SUMMARY_KEYS}

    async def _async_report_presence(self, day: DeliveryDay | None) -> None:
        """Check in only after summary authentication and complete entry setup."""
        if self._entry.state is not ConfigEntryState.LOADED or not self._entry.data.get(
            CONF_ADDON_SLUG
        ):
            return
        backup = await self._async_backup_report()
        # Unloading may begin while Supervisor is reading its backup inventory.
        if self._entry.state is not ConfigEntryState.LOADED:
            return
        users = await self.hass.auth.async_get_users()
        if self._entry.state is not ConfigEntryState.LOADED:
            return
        report: PresenceReport = {
            "version": self._version,
            "backup": backup,
            "administrator_ids": [
                user.id for user in users if user.is_active and user.is_admin
            ],
        }
        if day is not None:
            report["delivery_day"] = day.presence()
        try:
            response = await self.client.request(
                "POST",
                "/v1/integration/presence",
                body=json.dumps(report).encode(),
                actor="ha:integration",
            )
        except ClientError, TimeoutError, ResponseTooLarge:
            _LOGGER.debug("Cannot send the integration check-in")
            return
        if response.status in {401, 403}:
            raise ConfigEntryAuthFailed(
                "The backend rejected its integration credential"
            )
        if response.status != 204:
            _LOGGER.debug("The backend could not accept its integration check-in")

    async def _async_backup_report(self) -> BackupReport:
        slug = self._entry.data.get(CONF_ADDON_SLUG)
        if not slug:
            return {"status": "standalone", "last_backup": None}
        if not is_hassio(self.hass):
            return {"status": "unavailable", "last_backup": None}
        from aiohasupervisor import SupervisorError
        from homeassistant.components.hassio import get_supervisor_client

        try:
            async with asyncio.timeout(_BACKUP_TIMEOUT_SECONDS):
                info = await get_supervisor_client(self.hass).backups.info()
        except SupervisorError, ClientError, TimeoutError, ValueError, LookupError:
            return {"status": "unavailable", "last_backup": None}
        backups = [backup for backup in info.backups if slug in backup.content.addons]
        if not backups:
            return {"status": "missing", "last_backup": None}
        current = [
            backup for backup in backups if backup.date >= self._entry.modified_at
        ]
        if not current:
            return {
                "status": "outdated",
                "last_backup": int(max(backup.date for backup in backups).timestamp()),
            }
        backups = current
        complete = [backup for backup in backups if backup.content.homeassistant]
        latest = max(complete or backups, key=lambda backup: backup.date)
        return {
            "status": "complete" if complete else "app_only",
            "last_backup": int(latest.date.timestamp()),
        }
