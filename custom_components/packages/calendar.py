"""Read-only carrier delivery estimates for active inbound packages."""

from datetime import datetime

from homeassistant.components.calendar import CalendarEntity, CalendarEvent
from homeassistant.config_entries import ConfigEntry
from homeassistant.core import HomeAssistant
from homeassistant.exceptions import HomeAssistantError
from homeassistant.helpers.entity_platform import AddConfigEntryEntitiesCallback
from homeassistant.helpers.update_coordinator import CoordinatorEntity
from homeassistant.util import dt as dt_util

from .client import normalize_backend_url
from .const import CONF_ADDON_SLUG, CONF_BACKEND_URL
from .coordinator import PackagesCoordinator
from .entity import packages_device_info


async def async_setup_entry(
    hass: HomeAssistant,
    entry: ConfigEntry[PackagesCoordinator],
    async_add_entities: AddConfigEntryEntitiesCallback,
) -> None:
    async_add_entities(
        [
            InboundDeliveryCalendar(entry.runtime_data, entry),
            PickupCalendar(entry.runtime_data, entry),
        ]
    )


class InboundDeliveryCalendar(CoordinatorEntity[PackagesCoordinator], CalendarEntity):
    """Calendar windows are estimates, not guaranteed arrival appointments."""

    _attr_has_entity_name = True
    _attr_translation_key = "inbound_deliveries"
    _attr_supported_features = 0

    def __init__(
        self,
        coordinator: PackagesCoordinator,
        entry: ConfigEntry[PackagesCoordinator],
    ) -> None:
        super().__init__(coordinator)
        self._attr_unique_id = f"{entry.entry_id}_inbound_deliveries"
        self._attr_device_info = packages_device_info(entry)
        self._attr_extra_state_attributes = {
            "packages_calendar": True,
            "packages_app_url": (
                "/packages/packages/"
                if entry.data.get(CONF_ADDON_SLUG)
                else f"{normalize_backend_url(entry.data[CONF_BACKEND_URL])}/"
            ),
        }

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
        # Use the requested interval, not now: a full-day query includes an
        # authentic carrier window that ended earlier that day, with its exact
        # bounds. The entity's current/future event property stays independent.
        return [
            event
            for event in self._events()
            if event.start_datetime_local < end_date
            and event.end_datetime_local > start_date
        ]


class PickupCalendar(InboundDeliveryCalendar):
    """Read-only windows expose no addresses or parcel identifiers."""

    _attr_translation_key = "pickups"

    def __init__(
        self, coordinator: PackagesCoordinator, entry: ConfigEntry[PackagesCoordinator]
    ) -> None:
        super().__init__(coordinator, entry)
        self._attr_unique_id = f"{entry.entry_id}_pickups"
        self._attr_extra_state_attributes = {}

    @property
    def available(self) -> bool:
        report = self.coordinator.pickup_report
        return super().available and report is not None and not report.windows_truncated

    async def async_get_events(
        self, hass: HomeAssistant, start_date: datetime, end_date: datetime
    ) -> list[CalendarEvent]:
        report = self.coordinator.pickup_report
        if report is None or report.windows_truncated:
            raise HomeAssistantError("Pickup calendar coverage is unavailable")
        return await super().async_get_events(hass, start_date, end_date)

    def _events(self) -> list[CalendarEvent]:
        report = self.coordinator.pickup_report
        return (
            sorted(
                (window.calendar_event() for window in report.windows),
                key=lambda event: event.start_datetime_local,
            )
            if report is not None and not report.windows_truncated
            else []
        )
