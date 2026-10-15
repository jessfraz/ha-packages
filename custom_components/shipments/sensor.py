"""Shipment counts exposed as native sensors."""

from homeassistant.components.sensor import SensorEntity, SensorEntityDescription
from homeassistant.config_entries import ConfigEntry
from homeassistant.core import HomeAssistant
from homeassistant.helpers.entity import DeviceInfo
from homeassistant.helpers.entity_platform import AddConfigEntryEntitiesCallback
from homeassistant.helpers.update_coordinator import CoordinatorEntity

from .const import DOMAIN, SUMMARY_KEYS
from .coordinator import ShipmentsCoordinator


async def async_setup_entry(
    hass: HomeAssistant,
    entry: ConfigEntry[ShipmentsCoordinator],
    async_add_entities: AddConfigEntryEntitiesCallback,
) -> None:
    """Add summary entities for the configured backend."""
    async_add_entities(
        ShipmentCountSensor(entry.runtime_data, entry.entry_id, key)
        for key in SUMMARY_KEYS
    )


class ShipmentCountSensor(CoordinatorEntity[ShipmentsCoordinator], SensorEntity):
    """A backend-owned count, with no local shipment interpretation."""

    _attr_has_entity_name = True

    def __init__(
        self, coordinator: ShipmentsCoordinator, entry_id: str, key: str
    ) -> None:
        super().__init__(coordinator)
        self.entity_description = SensorEntityDescription(
            key=key, translation_key=key, icon="mdi:package-variant-closed"
        )
        self._attr_unique_id = f"{entry_id}_{key}"
        self._attr_device_info = DeviceInfo(
            identifiers={(DOMAIN, entry_id)},
            name="Shipments",
            manufacturer="Shipments",
            model="Shipment registry",
        )

    @property
    def native_value(self) -> int:
        """Return the last coordinated value."""
        return self.coordinator.data[self.entity_description.key]
