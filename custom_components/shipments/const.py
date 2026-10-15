"""Constants for the Shipments adapter."""

from .models import Direction

DOMAIN = "shipments"
CONF_BACKEND_URL = "backend_url"
CONF_TOKEN = "token"
CONF_ADDON_SLUG = "addon_slug"
DEFAULT_BACKEND_URL = ""
MAX_REQUEST_BYTES = 64 * 1024
MAX_RESPONSE_BYTES = 1024 * 1024
REQUEST_TIMEOUT_SECONDS = 15
SUMMARY_KEYS = (
    "active",
    "expected",
    "in_transit",
    "out_for_delivery",
    "delivered",
    "exceptions",
    "sync_errors",
)
DIRECTION_SUMMARY_KEYS: dict[Direction, tuple[str, ...]] = {
    Direction.INBOUND: ("active", "out_for_delivery", "exceptions"),
    Direction.OUTBOUND: ("active", "exceptions"),
}
SENSOR_KEYS = SUMMARY_KEYS + tuple(
    f"{direction}_{key}"
    for direction, keys in DIRECTION_SUMMARY_KEYS.items()
    for key in keys
)
