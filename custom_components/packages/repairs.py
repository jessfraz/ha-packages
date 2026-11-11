"""Native version repair for the Packages companion."""

from homeassistant.components.repairs import RepairsFlow
from homeassistant.config_entries import ConfigEntry
from homeassistant.core import HomeAssistant
from homeassistant.helpers.aiohttp_client import async_get_clientsession

from ._app_kit.repairs import VersionRepairSpec, create_version_fix_flow
from ._app_kit.version import LOADED_VERSION
from ._app_kit.versions import ReleaseVersion
from .client import PackagesClient
from .const import CONF_ADDON_SLUG, CONF_BACKEND_URL, CONF_TOKEN, DOMAIN


async def _async_probe_version(
    hass: HomeAssistant, entry: ConfigEntry
) -> ReleaseVersion:
    client = PackagesClient(
        async_get_clientsession(hass),
        entry.data[CONF_BACKEND_URL],
        entry.data[CONF_TOKEN],
    )
    response = await client.request("GET", "/v1/system")
    if response.status != 200:
        raise ValueError("Cannot verify the Packages version")
    value = response.json()
    if not isinstance(value, dict) or value.get("credential_role") != "integration":
        raise ValueError("Cannot verify the Packages version")
    return ReleaseVersion.parse(value.get("version"))


def _app_slug(entry: ConfigEntry) -> str | None:
    slug = entry.data.get(CONF_ADDON_SLUG)
    return slug if isinstance(slug, str) else None


VERSION_REPAIR = VersionRepairSpec(
    domain=DOMAIN,
    name="Packages",
    repository_id="1420009010",
    loaded_version=ReleaseVersion.parse(LOADED_VERSION),
    probe=_async_probe_version,
    app_slug=_app_slug,
)


async def async_create_fix_flow(
    hass: HomeAssistant, issue_id: str, data: dict[str, object] | None
) -> RepairsFlow:
    """Create the fix only for an issue owned by a live Packages entry."""
    return create_version_fix_flow(hass, issue_id, VERSION_REPAIR)
