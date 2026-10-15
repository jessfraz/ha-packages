"""Problem indicators for carrier exceptions and synchronization failures."""

from homeassistant.components.binary_sensor import (
    BinarySensorDeviceClass,
    BinarySensorEntity,
    BinarySensorEntityDescription,
)
from homeassistant.config_entries import ConfigEntry
from homeassistant.core import HomeAssistant
from homeassistant.helpers.entity import DeviceInfo
from homeassistant.helpers.entity_platform import AddConfigEntryEntitiesCallback
from homeassistant.helpers.update_coordinator import CoordinatorEntity

from .const import DOMAIN
from .coordinator import ShipmentsCoordinator


async def async_setup_entry(
    hass: HomeAssistant,
    entry: ConfigEntry[ShipmentsCoordinator],
    async_add_entities: AddConfigEntryEntitiesCallback,
) -> None:
    async_add_entities(
        ShipmentProblem(entry.runtime_data, entry.entry_id, key)
        for key in ("exceptions", "sync_errors")
    )


class ShipmentProblem(CoordinatorEntity[ShipmentsCoordinator], BinarySensorEntity):
    """Use backend-owned counts so unknown transport health is unavailable."""

    _attr_has_entity_name = True

    def __init__(
        self, coordinator: ShipmentsCoordinator, entry_id: str, key: str
    ) -> None:
        super().__init__(coordinator)
        self.entity_description = BinarySensorEntityDescription(
            key=key, translation_key=key, device_class=BinarySensorDeviceClass.PROBLEM
        )
        self._attr_unique_id = f"{entry_id}_{key}_problem"
        self._attr_device_info = DeviceInfo(identifiers={(DOMAIN, entry_id)})

    @property
    def is_on(self) -> bool:
        return self.coordinator.data[self.entity_description.key] > 0
