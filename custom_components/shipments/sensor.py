"""Shipment counts exposed as native sensors."""

from homeassistant.components.sensor import (
    SensorEntity,
    SensorEntityDescription,
    SensorStateClass,
)
from homeassistant.config_entries import ConfigEntry
from homeassistant.core import HomeAssistant
from homeassistant.helpers.entity_platform import AddConfigEntryEntitiesCallback
from homeassistant.helpers.update_coordinator import CoordinatorEntity

from .const import SENSOR_KEYS
from .coordinator import ShipmentsCoordinator
from .entity import shipments_device_info
from .models import ShipmentItems, ShipmentItemsAttributes


async def async_setup_entry(
    hass: HomeAssistant,
    entry: ConfigEntry[ShipmentsCoordinator],
    async_add_entities: AddConfigEntryEntitiesCallback,
) -> None:
    """Add summary entities for the configured backend."""
    async_add_entities(
        ShipmentCountSensor(entry.runtime_data, entry, key) for key in SENSOR_KEYS
    )


class ShipmentCountSensor(CoordinatorEntity[ShipmentsCoordinator], SensorEntity):
    """A backend-owned count with optional bounded dashboard items."""

    _attr_has_entity_name = True
    _unrecorded_attributes = frozenset({"items", "items_truncated"})

    def __init__(
        self,
        coordinator: ShipmentsCoordinator,
        entry: ConfigEntry[ShipmentsCoordinator],
        key: str,
    ) -> None:
        super().__init__(coordinator)
        self.entity_description = SensorEntityDescription(
            key=key, translation_key=key, state_class=SensorStateClass.MEASUREMENT
        )
        self._attr_unique_id = f"{entry.entry_id}_{key}"
        self._attr_device_info = shipments_device_info(entry)

    @property
    def suggested_object_id(self) -> str:
        """Keep count IDs stable when their translated display labels change."""
        return self.entity_description.key

    @property
    def available(self) -> bool:
        """A projection is available only when this backend supplied its value."""
        data = self.coordinator.data
        return (
            super().available
            and data is not None
            and self.entity_description.key in data
        )

    @property
    def native_value(self) -> int | None:
        """Return only a count supported by the coordinated backend."""
        data = self.coordinator.data
        return data.get(self.entity_description.key) if data is not None else None

    @property
    def extra_state_attributes(self) -> ShipmentItemsAttributes | None:
        """Expose only the two bounded dashboard lists, never stale item data."""
        key = self.entity_description.key
        if key not in {"inbound_delivered_today", "inbound_out_for_delivery"}:
            return None
        projection = (
            self.coordinator.shipment_items.get(key) if self.available else None
        )
        return (projection or ShipmentItems()).attributes()
