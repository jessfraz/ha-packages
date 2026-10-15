"""Coordinate shipment counts and authenticated installation check-ins."""

import asyncio
import json
import logging
from datetime import timedelta
from typing import Literal, TypedDict

from aiohasupervisor import SupervisorError
from aiohttp import ClientError
from homeassistant.components.hassio import get_supervisor_client
from homeassistant.config_entries import ConfigEntry, ConfigEntryState
from homeassistant.core import HomeAssistant
from homeassistant.exceptions import ConfigEntryAuthFailed
from homeassistant.helpers.hassio import is_hassio
from homeassistant.helpers.update_coordinator import DataUpdateCoordinator, UpdateFailed

from .client import ResponseTooLarge, ShipmentsClient
from .const import CONF_ADDON_SLUG, DOMAIN, SUMMARY_KEYS

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


class ShipmentsCoordinator(DataUpdateCoordinator[dict[str, int]]):
    """Poll one summary for all count sensors."""

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
            always_update=False,
        )
        self.client = client
        self._entry = entry
        self._version = version

    async def _async_update_data(self) -> dict[str, int]:
        try:
            response = await self.client.request("GET", "/v1/summary")
            if response.status in {401, 403}:
                raise ConfigEntryAuthFailed("The backend rejected its credential")
            if response.status != 200:
                raise UpdateFailed("The backend could not return its summary")
            data = response.json()
            if not isinstance(data, dict) or any(
                type(data.get(key)) is not int or data[key] < 0 for key in SUMMARY_KEYS
            ):
                raise UpdateFailed("The backend returned an invalid summary")
            summary = {key: data[key] for key in SUMMARY_KEYS}
        except (ClientError, TimeoutError, ResponseTooLarge, ValueError) as err:
            raise UpdateFailed("Cannot read the backend summary") from err
        await self._async_report_presence()
        return summary

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
        report: PresenceReport = {"version": self._version, "backup": backup}
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
