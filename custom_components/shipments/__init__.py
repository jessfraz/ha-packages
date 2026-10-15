"""Home Assistant adapter for the local Shipments service."""

from homeassistant.config_entries import ConfigEntry, ConfigEntryState
from homeassistant.const import Platform
from homeassistant.core import HomeAssistant, callback
from homeassistant.helpers.aiohttp_client import async_get_clientsession
from homeassistant.helpers.typing import ConfigType
from homeassistant.loader import async_get_integration

from .client import ShipmentsClient
from .const import CONF_ADDON_SLUG, CONF_BACKEND_URL, CONF_TOKEN, DOMAIN
from .coordinator import ShipmentsCoordinator
from .http import ShipmentsView
from .services import async_register_services

PLATFORMS = [Platform.SENSOR]
type ShipmentsConfigEntry = ConfigEntry[ShipmentsCoordinator]


async def async_setup(hass: HomeAssistant, config: ConfigType) -> bool:
    """Install the authenticated view once, independent of reloads."""
    hass.http.register_view(ShipmentsView(hass))
    async_register_services(hass)
    return True


async def async_setup_entry(hass: HomeAssistant, entry: ShipmentsConfigEntry) -> bool:
    """Connect one backend and forward its count sensors."""
    client = ShipmentsClient(
        async_get_clientsession(hass),
        entry.data[CONF_BACKEND_URL],
        entry.data[CONF_TOKEN],
    )
    integration = await async_get_integration(hass, DOMAIN)
    coordinator = ShipmentsCoordinator(hass, entry, client, str(integration.version))
    await coordinator.async_config_entry_first_refresh()
    entry.runtime_data = coordinator
    await hass.config_entries.async_forward_entry_setups(entry, PLATFORMS)

    @callback
    def report_loaded() -> None:
        if entry.state is ConfigEntryState.LOADED and entry.data.get(CONF_ADDON_SLUG):
            # A second authenticated poll proves setup actually reached LOADED.
            # Entry-owned work is cancelled if unloading starts during the report.
            entry.async_create_background_task(
                hass, coordinator.async_refresh(), "Shipments loaded check-in"
            )

    entry.async_on_unload(entry.async_on_state_change(report_loaded))
    return True


async def async_unload_entry(hass: HomeAssistant, entry: ShipmentsConfigEntry) -> bool:
    """Release entities and polling while retaining HA's shared HTTP session."""
    return await hass.config_entries.async_unload_platforms(entry, PLATFORMS)
