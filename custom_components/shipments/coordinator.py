"""Coordinate the summary used by Home Assistant entities."""

import logging
from datetime import timedelta

from aiohttp import ClientError
from homeassistant.config_entries import ConfigEntry
from homeassistant.core import HomeAssistant
from homeassistant.exceptions import ConfigEntryAuthFailed
from homeassistant.helpers.update_coordinator import DataUpdateCoordinator, UpdateFailed

from .client import ResponseTooLarge, ShipmentsClient
from .const import DOMAIN, SUMMARY_KEYS

_LOGGER = logging.getLogger(__name__)


class ShipmentsCoordinator(DataUpdateCoordinator[dict[str, int]]):
    """Poll one summary for all count sensors."""

    def __init__(
        self, hass: HomeAssistant, entry: ConfigEntry, client: ShipmentsClient
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
            return {key: data[key] for key in SUMMARY_KEYS}
        except (ClientError, TimeoutError, ResponseTooLarge, ValueError) as err:
            raise UpdateFailed("Cannot read the backend summary") from err
