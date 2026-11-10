"""Native version repair for the Packages companion."""

from typing import TYPE_CHECKING

from homeassistant.components.repairs import RepairsFlow
from homeassistant.config_entries import ConfigEntry
from homeassistant.core import HomeAssistant

from ._app_kit.repairs import VersionRepairSpec, create_version_fix_flow
from ._app_kit.version import LOADED_VERSION
from ._app_kit.versions import ReleaseVersion
from .const import CONF_ADDON_SLUG, DOMAIN

if TYPE_CHECKING:
    from .coordinator import PackagesCoordinator


async def _async_probe_version(
    hass: HomeAssistant, entry: ConfigEntry[PackagesCoordinator]
) -> ReleaseVersion:
    version, _ = await entry.runtime_data.async_read_system()
    return version


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
