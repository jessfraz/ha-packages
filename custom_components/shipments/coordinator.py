"""Coordinate shipment counts and authenticated installation check-ins."""

import asyncio
import json
import logging
from collections.abc import Callable
from datetime import timedelta
from typing import Literal, TypedDict
from urllib.parse import urlencode
from uuid import UUID

from aiohttp import ClientError
from homeassistant.config_entries import ConfigEntry, ConfigEntryState
from homeassistant.core import CALLBACK_TYPE, HomeAssistant, callback
from homeassistant.exceptions import ConfigEntryAuthFailed, HomeAssistantError
from homeassistant.helpers import issue_registry as ir
from homeassistant.helpers.hassio import is_hassio
from homeassistant.helpers.storage import Store
from homeassistant.helpers.update_coordinator import DataUpdateCoordinator, UpdateFailed

from .client import ResponseTooLarge, ShipmentsClient
from .const import CONF_ADDON_SLUG, DIRECTION_SUMMARY_KEYS, DOMAIN, SUMMARY_KEYS
from .models import DeliveryEstimate, ShipmentStatusEvent, delivery_estimate

_LOGGER = logging.getLogger(__name__)
_BACKUP_TIMEOUT_SECONDS = 10


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


class EventCheckpoint(TypedDict):
    """Only a numeric backend cursor is persisted by the adapter."""

    after: int


class ShipmentsCoordinator(DataUpdateCoordinator[dict[str, int]]):
    """Poll combined and direction-specific summaries for all count sensors."""

    def __init__(
        self,
        hass: HomeAssistant,
        entry: ConfigEntry,
        client: ShipmentsClient,
        version: str,
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
        self._version = version
        self.inbound_estimates: tuple[DeliveryEstimate, ...] = ()
        self._event_store = Store[EventCheckpoint](
            hass, 1, f"{DOMAIN}.{entry.entry_id}.events", atomic_writes=True
        )
        self._checkpoint_loaded = False
        self._after: int | None = None
        self._event_listeners: list[Callable[[ShipmentStatusEvent], None]] = []
        self.event_available = False

    async def _async_update_data(self) -> dict[str, int]:
        # A prior successful feed cannot establish availability for this refresh.
        # The validated current page restores it before dispatching transitions.
        self.event_available = False
        try:
            summary = await self._async_summary()
            for direction, keys in DIRECTION_SUMMARY_KEYS.items():
                counts = await self._async_summary(direction)
                summary.update({f"{direction}_{key}": counts[key] for key in keys})
            estimates = await self._async_estimates()
            await self._async_check_version()
            await self._async_report_presence()
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
        self.inbound_estimates = estimates
        return summary

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

    async def _async_check_version(self) -> None:
        response = await self.client.request("GET", "/v1/system")
        if response.status in {401, 403}:
            raise ConfigEntryAuthFailed("The backend rejected its credential")
        if response.status != 200:
            raise UpdateFailed("The backend could not return its version")
        data = response.json()
        version = data.get("version") if isinstance(data, dict) else None
        if not isinstance(version, str) or not version or len(version) > 64:
            raise ValueError("Invalid backend version")
        if version != self._version:
            ir.async_create_issue(
                self.hass,
                DOMAIN,
                "version_mismatch",
                is_fixable=False,
                severity=ir.IssueSeverity.WARNING,
                translation_key="version_mismatch",
                translation_placeholders={
                    "app_version": version,
                    "integration_version": self._version,
                },
            )
        else:
            ir.async_delete_issue(self.hass, DOMAIN, "version_mismatch")

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

    async def _async_summary(self, direction: str | None = None) -> dict[str, int]:
        """Validate every summary before publishing a complete refresh."""
        path = "/v1/summary"
        label = "summary"
        if direction is not None:
            path += f"?direction={direction}"
            label = f"{direction} summary"
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

    async def _async_report_presence(self) -> None:
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
