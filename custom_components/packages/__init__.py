"""Home Assistant adapter for the local Packages service."""

from pathlib import Path
from urllib.parse import urlencode

from homeassistant.components import frontend, persistent_notification
from homeassistant.components.http import StaticPathConfig
from homeassistant.config_entries import ConfigEntry, ConfigEntryState
from homeassistant.const import Platform
from homeassistant.core import HomeAssistant, callback
from homeassistant.exceptions import ConfigEntryNotReady
from homeassistant.helpers.aiohttp_client import async_get_clientsession
from homeassistant.helpers.typing import ConfigType

from ._app_kit.version import LOADED_VERSION
from .assist_sentences import async_register_sentences
from .client import PackagesClient
from .const import CONF_ADDON_SLUG, CONF_BACKEND_URL, CONF_TOKEN, DOMAIN
from .coordinator import (
    EventCheckpoint,
    PackagesCoordinator,
    amazon_notification_id,
)
from .http import PackagesView
from .services import async_register_services
from .statistics_migration import async_register_statistics_migration

PLATFORMS = [Platform.SENSOR, Platform.BINARY_SENSOR, Platform.CALENDAR, Platform.EVENT]
type PackagesConfigEntry = ConfigEntry[PackagesCoordinator]


async def async_setup(hass: HomeAssistant, config: ConfigType) -> bool:
    """Install the authenticated view once, independent of reloads."""
    hass.http.register_view(PackagesView(hass))
    await hass.http.async_register_static_paths(
        [
            StaticPathConfig(
                "/packages-static/calendar-links.js",
                str(Path(__file__).parent / "frontend" / "calendar-links.js"),
                False,
            )
        ]
    )
    async_register_services(hass)
    async_register_statistics_migration(hass)
    return True


async def async_setup_entry(hass: HomeAssistant, entry: PackagesConfigEntry) -> bool:
    """Connect one backend and forward its count sensors."""
    client = PackagesClient(
        async_get_clientsession(hass),
        entry.data[CONF_BACKEND_URL],
        entry.data[CONF_TOKEN],
    )
    coordinator = PackagesCoordinator(hass, entry, client)
    try:
        await coordinator.async_config_entry_first_refresh()
    except ConfigEntryNotReady:
        coordinator.async_report_startup_version_mismatch()
        raise
    entry.runtime_data = coordinator
    coordinator.async_start_version_repairs()
    coordinator.async_start_amazon_repairs()
    await hass.config_entries.async_forward_entry_setups(entry, PLATFORMS)
    await async_register_sentences(hass, entry)
    module_url = "/packages-static/calendar-links.js?" + urlencode(
        {"v": LOADED_VERSION}
    )
    frontend.add_extra_js_url(hass, module_url)
    entry.async_on_unload(lambda: frontend.remove_extra_js_url(hass, module_url))
    if addon_slug := entry.data.get(CONF_ADDON_SLUG):
        # Route deep links through Supervisor without adding a second sidebar entry.
        frontend.async_register_built_in_panel(
            hass,
            "app",
            frontend_url_path="packages",
            config={"addon": addon_slug},
            require_admin=False,
            show_in_sidebar=False,
        )
        entry.async_on_unload(lambda: frontend.async_remove_panel(hass, "packages"))

    @callback
    def report_loaded() -> None:
        if entry.state is ConfigEntryState.LOADED:
            # A second authenticated poll proves setup actually reached LOADED.
            # Entry-owned work is cancelled if unloading starts during the report.
            entry.async_create_background_task(
                hass, coordinator.async_refresh(), "Packages loaded check-in"
            )

    entry.async_on_unload(entry.async_on_state_change(report_loaded))
    return True


async def async_unload_entry(hass: HomeAssistant, entry: PackagesConfigEntry) -> bool:
    """Release entities and polling while retaining HA's shared HTTP session."""
    return await hass.config_entries.async_unload_platforms(entry, PLATFORMS)


async def async_remove_entry(hass: HomeAssistant, entry: PackagesConfigEntry) -> None:
    """Remove only this connection's owned cursor and diagnostic issues."""
    from homeassistant.helpers import issue_registry as ir
    from homeassistant.helpers.storage import Store

    await Store[EventCheckpoint](
        hass, 1, f"{DOMAIN}.{entry.entry_id}.events"
    ).async_remove()
    persistent_notification.async_dismiss(hass, amazon_notification_id(entry.entry_id))
    for issue in (
        "version_mismatch",
        f"{entry.entry_id}_version_mismatch",
        "event_stream_unavailable",
        "event_history_reset",
    ):
        ir.async_delete_issue(hass, DOMAIN, issue)
