"""Shared device identity for every native Shipments platform."""

from homeassistant.config_entries import ConfigEntry
from homeassistant.helpers.entity import DeviceInfo

from .const import CONF_ADDON_SLUG, CONF_BACKEND_URL, DOMAIN


def shipments_device_info(entry: ConfigEntry) -> DeviceInfo:
    """Provide complete metadata before any platform derives its entity IDs."""
    return DeviceInfo(
        identifiers={(DOMAIN, entry.entry_id)},
        name="Shipments",
        manufacturer="Shipments",
        model="Shipment registry",
        configuration_url=(
            "homeassistant://shipments"
            if entry.data.get(CONF_ADDON_SLUG)
            else entry.data[CONF_BACKEND_URL]
        ),
    )
