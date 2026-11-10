"""Strict, recorder-safe pickup counts and calendar windows."""

from dataclasses import dataclass
from datetime import date, datetime, time, timedelta
from uuid import UUID
from zoneinfo import ZoneInfo, ZoneInfoNotFoundError

from homeassistant.components.calendar import CalendarEvent


@dataclass(frozen=True)
class PickupWindow:
    """A scheduled carrier window, never proof of physical collection."""

    id: UUID
    carrier: str
    start: datetime
    end: datetime

    def calendar_event(self) -> CalendarEvent:
        """Keep addresses, contact details and barcodes out of the recorder."""
        label = {"usps": "USPS", "ups": "UPS", "fedex": "FedEx"}[self.carrier]
        return CalendarEvent(
            start=self.start.date() if self.carrier == "usps" else self.start,
            end=self.start.date() + timedelta(days=1)
            if self.carrier == "usps"
            else self.end,
            summary=f"{label} pickup",
            description="During normal mail delivery; no pickup time can be selected."
            if self.carrier == "usps"
            else "Scheduled carrier collection window, not proof of collection.",
            uid=str(self.id),
        )


@dataclass(frozen=True)
class PickupReport:
    """Absence or invalid coverage is not a zero pickup count."""

    scheduled: int
    uncertain: int
    windows: tuple[PickupWindow, ...]
    windows_truncated: bool

    @classmethod
    def parse(cls, data: object) -> PickupReport:
        """Validate the actual backend projection before exposing native entities."""
        if not isinstance(data, dict):
            raise ValueError("Invalid pickup projection")
        scheduled, uncertain = data.get("scheduled"), data.get("uncertain")
        rows, truncated = data.get("windows"), data.get("windows_truncated")
        if (
            type(scheduled) is not int
            or type(uncertain) is not int
            or scheduled < 0
            or uncertain < 0
            or not isinstance(rows, list)
            or len(rows) > 200
            or type(truncated) is not bool
        ):
            raise ValueError("Invalid pickup projection")
        windows: list[PickupWindow] = []
        for row in rows:
            if not isinstance(row, dict) or row.get("carrier") not in {
                "usps",
                "ups",
                "fedex",
            }:
                raise ValueError("Invalid pickup window")
            try:
                identifier = UUID(row["id"])
                day = date.fromisoformat(row["date"])
                ready = time.fromisoformat(row["ready_time"])
                close = time.fromisoformat(row["close_time"])
                zone = ZoneInfo(row["timezone"])
            except ValueError, TypeError, KeyError, ZoneInfoNotFoundError:
                raise ValueError("Invalid pickup window") from None
            if ready.tzinfo is not None or close.tzinfo is not None or ready >= close:
                raise ValueError("Invalid pickup window")
            windows.append(
                PickupWindow(
                    identifier,
                    row["carrier"],
                    datetime.combine(day, ready, zone),
                    datetime.combine(day, close, zone),
                )
            )
        return cls(scheduled, uncertain, tuple(windows), truncated)
