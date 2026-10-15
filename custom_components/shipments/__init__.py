"""Home Assistant adapter for the local Shipments service."""

from homeassistant.config_entries import ConfigEntry
from homeassistant.const import Platform
from homeassistant.core import HomeAssistant
from homeassistant.helpers.aiohttp_client import async_get_clientsession
from homeassistant.helpers.typing import ConfigType

from .client import ShipmentsClient
from .const import CONF_BACKEND_URL, CONF_TOKEN
from .coordinator import ShipmentsCoordinator
from .http import ShipmentsView
from .services import async_register_services, async_remove_services

PLATFORMS = [Platform.SENSOR]
type ShipmentsConfigEntry = ConfigEntry[ShipmentsCoordinator]


async def async_setup(hass: HomeAssistant, config: ConfigType) -> bool:
    """Install the authenticated view once, independent of reloads."""
    hass.http.register_view(ShipmentsView(hass))
    return True


async def async_setup_entry(hass: HomeAssistant, entry: ShipmentsConfigEntry) -> bool:
    """Connect one backend and forward its count sensors."""
    client = ShipmentsClient(
        async_get_clientsession(hass),
        entry.data[CONF_BACKEND_URL],
        entry.data[CONF_TOKEN],
    )
    coordinator = ShipmentsCoordinator(hass, entry, client)
    await coordinator.async_config_entry_first_refresh()
    entry.runtime_data = coordinator
    await hass.config_entries.async_forward_entry_setups(entry, PLATFORMS)
    async_register_services(hass)
    return True


async def async_unload_entry(hass: HomeAssistant, entry: ShipmentsConfigEntry) -> bool:
    """Release entities and polling while retaining HA's shared HTTP session."""
    if unloaded := await hass.config_entries.async_unload_platforms(entry, PLATFORMS):
        async_remove_services(hass)
    return unloaded
