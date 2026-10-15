"""Read-only carrier delivery estimates for active inbound shipments."""

from datetime import datetime

from homeassistant.components.calendar import CalendarEntity, CalendarEvent
from homeassistant.config_entries import ConfigEntry
from homeassistant.core import HomeAssistant
from homeassistant.helpers.entity_platform import AddConfigEntryEntitiesCallback
from homeassistant.helpers.update_coordinator import CoordinatorEntity
from homeassistant.util import dt as dt_util

from .coordinator import ShipmentsCoordinator
from .entity import shipments_device_info


async def async_setup_entry(
    hass: HomeAssistant,
    entry: ConfigEntry[ShipmentsCoordinator],
    async_add_entities: AddConfigEntryEntitiesCallback,
) -> None:
    async_add_entities([InboundDeliveryCalendar(entry.runtime_data, entry)])


class InboundDeliveryCalendar(CoordinatorEntity[ShipmentsCoordinator], CalendarEntity):
    """Calendar windows are estimates, not guaranteed arrival appointments."""

    _attr_has_entity_name = True
    _attr_translation_key = "inbound_deliveries"
    _attr_supported_features = 0

    def __init__(
        self,
        coordinator: ShipmentsCoordinator,
        entry: ConfigEntry[ShipmentsCoordinator],
    ) -> None:
        super().__init__(coordinator)
        self._attr_unique_id = f"{entry.entry_id}_inbound_deliveries"
        self._attr_device_info = shipments_device_info(entry)

    @property
    def event(self) -> CalendarEvent | None:
        now = dt_util.now()
        events = self._events()
        return next((event for event in events if event.end_datetime_local > now), None)

    def _events(self) -> list[CalendarEvent]:
        return sorted(
            (
                estimate.calendar_event()
                for estimate in self.coordinator.inbound_estimates
            ),
            key=lambda event: event.start_datetime_local,
        )

    async def async_get_events(
        self, hass: HomeAssistant, start_date: datetime, end_date: datetime
    ) -> list[CalendarEvent]:
        return [
            event
            for event in self._events()
            if event.start_datetime_local < end_date
            and event.end_datetime_local > start_date
        ]
