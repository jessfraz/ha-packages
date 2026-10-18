"""Home Assistant adapter for the local Shipments service."""

from pathlib import Path
from urllib.parse import urlencode

from homeassistant.components import frontend
from homeassistant.components.http import StaticPathConfig
from homeassistant.config_entries import ConfigEntry, ConfigEntryState
from homeassistant.const import Platform
from homeassistant.core import HomeAssistant, callback
from homeassistant.helpers.aiohttp_client import async_get_clientsession
from homeassistant.helpers.typing import ConfigType
from homeassistant.loader import async_get_integration

from .assist_sentences import async_register_sentences
from .client import ShipmentsClient
from .const import CONF_ADDON_SLUG, CONF_BACKEND_URL, CONF_TOKEN, DOMAIN
from .coordinator import EventCheckpoint, ShipmentsCoordinator
from .http import ShipmentsView
from .services import async_register_services

PLATFORMS = [Platform.SENSOR, Platform.BINARY_SENSOR, Platform.CALENDAR, Platform.EVENT]
type ShipmentsConfigEntry = ConfigEntry[ShipmentsCoordinator]


async def async_setup(hass: HomeAssistant, config: ConfigType) -> bool:
    """Install the authenticated view once, independent of reloads."""
    hass.http.register_view(ShipmentsView(hass))
    await hass.http.async_register_static_paths(
        [
            StaticPathConfig(
                "/shipments-static/calendar-links.js",
                str(Path(__file__).parent / "frontend" / "calendar-links.js"),
                False,
            )
        ]
    )
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
    await async_register_sentences(hass, entry)
    module_url = "/shipments-static/calendar-links.js?" + urlencode(
        {"v": str(integration.version)}
    )
    frontend.add_extra_js_url(hass, module_url)
    entry.async_on_unload(lambda: frontend.remove_extra_js_url(hass, module_url))
    if addon_slug := entry.data.get(CONF_ADDON_SLUG):
        frontend.async_register_built_in_panel(
            hass,
            "app",
            sidebar_title="Shipments",
            sidebar_icon="mdi:package-variant",
            frontend_url_path="shipments",
            config={"addon": addon_slug},
            require_admin=False,
        )
        entry.async_on_unload(lambda: frontend.async_remove_panel(hass, "shipments"))

    @callback
    def report_loaded() -> None:
        if entry.state is ConfigEntryState.LOADED:
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


async def async_remove_entry(hass: HomeAssistant, entry: ShipmentsConfigEntry) -> None:
    """Remove only this connection's owned cursor and diagnostic issues."""
    from homeassistant.helpers import issue_registry as ir
    from homeassistant.helpers.storage import Store

    await Store[EventCheckpoint](
        hass, 1, f"{DOMAIN}.{entry.entry_id}.events"
    ).async_remove()
    for issue in (
        "version_mismatch",
        "event_stream_unavailable",
        "event_history_reset",
    ):
        ir.async_delete_issue(hass, DOMAIN, issue)
