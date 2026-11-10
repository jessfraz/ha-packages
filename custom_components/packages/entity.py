"""Shared device identity for every native Packages platform."""

from homeassistant.config_entries import ConfigEntry
from homeassistant.core import callback
from homeassistant.helpers.entity import DeviceInfo, EntityCategory
from homeassistant.helpers.update_coordinator import CoordinatorEntity

from .const import CONF_ADDON_SLUG, CONF_BACKEND_URL, DOMAIN
from .coordinator import PackagesCoordinator
from .health import DiscoveryHealth


class AmazonDiscoveryEntity(CoordinatorEntity[PackagesCoordinator]):
    """Share freshness and safe attributes between the diagnostic projections."""

    _attr_has_entity_name = True
    _attr_entity_category = EntityCategory.DIAGNOSTIC
    _attr_translation_key = "amazon_discovery"

    def __init__(
        self, coordinator: PackagesCoordinator, entry: ConfigEntry, key: str
    ) -> None:
        super().__init__(coordinator)
        self._attr_unique_id = f"{entry.entry_id}_{key}"
        self._attr_device_info = packages_device_info(entry)
        self._update_attributes()

    @property
    def health(self) -> DiscoveryHealth | None:
        report = self.coordinator.amazon_discovery
        return (
            report
            if super().available and report is not None and report.current
            else None
        )

    @property
    def available(self) -> bool:
        return self.health is not None

    @callback
    def _handle_coordinator_update(self) -> None:
        self._update_attributes()
        super()._handle_coordinator_update()

    def _update_attributes(self) -> None:
        report = self.coordinator.amazon_discovery
        self._attr_extra_state_attributes = report.attributes() if report else {}


def packages_device_info(entry: ConfigEntry) -> DeviceInfo:
    """Provide complete metadata before any platform derives its entity IDs."""
    return DeviceInfo(
        identifiers={(DOMAIN, entry.entry_id)},
        name="Packages",
        manufacturer="Packages",
        model="Shipment registry",
        configuration_url=(
            "homeassistant://packages"
            if entry.data.get(CONF_ADDON_SLUG)
            else entry.data[CONF_BACKEND_URL]
        ),
    )
