"""A native event entity backed by persisted Rust status transitions."""

from homeassistant.components.event import EventEntity
from homeassistant.config_entries import ConfigEntry
from homeassistant.core import HomeAssistant, callback
from homeassistant.helpers.entity_platform import AddConfigEntryEntitiesCallback
from homeassistant.helpers.update_coordinator import CoordinatorEntity

from .coordinator import ShipmentsCoordinator
from .entity import shipments_device_info
from .models import ShipmentStatusEvent


async def async_setup_entry(
    hass: HomeAssistant,
    entry: ConfigEntry[ShipmentsCoordinator],
    async_add_entities: AddConfigEntryEntitiesCallback,
) -> None:
    async_add_entities([ShipmentEvents(entry.runtime_data, entry)])


class ShipmentEvents(CoordinatorEntity[ShipmentsCoordinator], EventEntity):
    """Stable event IDs let automations deduplicate at-least-once delivery."""

    _attr_has_entity_name = True
    _attr_name = None
    _attr_event_types = ["out_for_delivery", "delivered", "exception"]

    def __init__(
        self,
        coordinator: ShipmentsCoordinator,
        entry: ConfigEntry[ShipmentsCoordinator],
    ) -> None:
        super().__init__(coordinator)
        self._attr_unique_id = f"{entry.entry_id}_status_events"
        self._attr_device_info = shipments_device_info(entry)

    async def async_added_to_hass(self) -> None:
        await super().async_added_to_hass()
        self.async_on_remove(self.coordinator.async_subscribe_event(self._handle_event))

    @property
    def available(self) -> bool:
        # Core publishes last_update_success after _async_update_data returns;
        # queued events must already be visible while that refresh dispatches.
        return self.coordinator.event_available

    @callback
    def _handle_event(self, event: ShipmentStatusEvent) -> None:
        self._trigger_event(event.event_type, event.attributes())
        self.async_write_ha_state()
