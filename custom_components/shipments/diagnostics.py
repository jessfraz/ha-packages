"""Native diagnostics containing only cached, explicitly selected metadata."""

from typing import Literal, TypedDict

from homeassistant.config_entries import ConfigEntry, ConfigEntryState
from homeassistant.core import HomeAssistant
from homeassistant.helpers import issue_registry as ir
from homeassistant.loader import async_get_integration

from .const import CONF_ADDON_SLUG, DOMAIN, SENSOR_KEYS
from .coordinator import ShipmentsCoordinator


class IntegrationDiagnostics(TypedDict):
    version: str
    entry_state: str
    connection: Literal["managed_app", "standalone"]


class CoordinatorDiagnostics(TypedDict):
    available: bool
    last_update_success: bool | None
    last_error_type: str | None
    update_interval_seconds: float | None
    event_stream_available: bool | None


class BackendDiagnostics(TypedDict):
    last_observed_version: str | None
    last_observed_versions_match: bool | None


class SummaryDiagnostics(TypedDict):
    current: bool
    counts: dict[str, int] | None


class ShipmentsDiagnostics(TypedDict):
    integration: IntegrationDiagnostics
    coordinator: CoordinatorDiagnostics
    backend: BackendDiagnostics
    summary: SummaryDiagnostics
    repairs: dict[str, bool]


async def async_get_config_entry_diagnostics(
    hass: HomeAssistant, entry: ConfigEntry[ShipmentsCoordinator]
) -> ShipmentsDiagnostics:
    """Download health without credentials, shipment records or network requests."""
    integration = await async_get_integration(hass, DOMAIN)
    version = str(integration.version)
    coordinator = getattr(entry, "runtime_data", None)
    loaded = entry.state is ConfigEntryState.LOADED
    available = bool(loaded and coordinator and coordinator.last_update_success)
    counts = None
    backend_version = None
    if coordinator is not None:
        backend_version = coordinator.last_backend_version
        if coordinator.data is not None:
            # Failed refreshes retain the prior snapshot. Keep its actual values
            # with current=false; absent data must never become invented zeros.
            counts = {
                key: coordinator.data[key]
                for key in SENSOR_KEYS
                if key in coordinator.data
            }
    registry = ir.async_get(hass)
    return {
        "integration": {
            "version": version,
            "entry_state": entry.state.value,
            "connection": "managed_app"
            if entry.data.get(CONF_ADDON_SLUG)
            else "standalone",
        },
        "coordinator": {
            "available": available,
            "last_update_success": (
                coordinator.last_update_success if coordinator is not None else None
            ),
            "last_error_type": (
                type(coordinator.last_exception).__name__
                if coordinator is not None and coordinator.last_exception is not None
                else None
            ),
            "update_interval_seconds": (
                coordinator.update_interval.total_seconds()
                if coordinator is not None and coordinator.update_interval is not None
                else None
            ),
            "event_stream_available": (
                available and coordinator.event_available
                if coordinator is not None
                else None
            ),
        },
        "backend": {
            "last_observed_version": backend_version,
            "last_observed_versions_match": (
                backend_version == version if backend_version is not None else None
            ),
        },
        "summary": {"current": available, "counts": counts},
        "repairs": {
            key: registry.async_get_issue(DOMAIN, key) is not None
            for key in (
                "version_mismatch",
                "event_stream_unavailable",
                "event_history_reset",
            )
        },
    }
