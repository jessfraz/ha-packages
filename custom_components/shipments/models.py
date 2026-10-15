"""Validate the small backend projections consumed by native entities."""

from dataclasses import dataclass
from datetime import date, datetime, time, timedelta
from enum import StrEnum
from typing import TypedDict
from urllib.parse import quote
from uuid import UUID
from zoneinfo import ZoneInfo

from homeassistant.components.calendar import CalendarEvent
from homeassistant.util import dt as dt_util


class StatusEventType(StrEnum):
    OUT_FOR_DELIVERY = "out_for_delivery"
    DELIVERED = "delivered"
    EXCEPTION = "exception"


class Direction(StrEnum):
    INBOUND = "inbound"
    OUTBOUND = "outbound"


class ShipmentStatus(StrEnum):
    EXPECTED = "expected"
    UNKNOWN = "unknown"
    PRE_TRANSIT = "pre_transit"
    IN_TRANSIT = "in_transit"
    OUT_FOR_DELIVERY = "out_for_delivery"
    AVAILABLE_FOR_PICKUP = "available_for_pickup"
    DELIVERED = "delivered"
    RETURN_TO_SENDER = "return_to_sender"
    FAILURE = "failure"
    CANCELLED = "cancelled"
    ERROR = "error"


@dataclass(frozen=True, slots=True)
class ShipmentStatusEvent:
    """One durable backend status transition, with a stable deduplication ID."""

    id: int
    shipment_id: UUID
    event_type: StatusEventType
    status: ShipmentStatus
    title: str
    carrier: str | None
    direction: Direction
    at: datetime

    @classmethod
    def parse(cls, record: object) -> ShipmentStatusEvent:
        if not isinstance(record, dict):
            raise ValueError("Invalid shipment event")
        event_id = record.get("id")
        shipment_id = record.get("shipment_id")
        event_type = record.get("event_type")
        status = record.get("status")
        title = record.get("title")
        carrier = record.get("carrier")
        direction = record.get("direction")
        at = record.get("at")
        if (
            type(event_id) is not int
            or event_id < 1
            or not isinstance(shipment_id, str)
            or not isinstance(event_type, str)
            or not isinstance(status, str)
            or not isinstance(title, str)
            or (carrier is not None and not isinstance(carrier, str))
            or not isinstance(direction, str)
            or not isinstance(at, str)
        ):
            raise ValueError("Invalid shipment event")
        timestamp = dt_util.parse_datetime(at)
        if timestamp is None or timestamp.tzinfo is None:
            raise ValueError("Invalid shipment event timestamp")
        return cls(
            event_id,
            UUID(shipment_id),
            StatusEventType(event_type),
            ShipmentStatus(status),
            title,
            carrier,
            Direction(direction),
            timestamp,
        )

    def attributes(self) -> dict[str, int | str | None]:
        return {
            "event_id": self.id,
            "shipment_id": str(self.shipment_id),
            "title": self.title,
            "carrier": self.carrier,
            "direction": self.direction,
            "status": self.status,
            "at": self.at.isoformat(),
        }


class DeliveryDayReport(TypedDict):
    """The authenticated local-day projection shared with the app UI."""

    date: str
    time_zone: str
    delivered_after: int
    delivered_before: int


@dataclass(frozen=True, slots=True)
class DeliveryDay:
    """UTC bounds for one civil day in Home Assistant's configured timezone."""

    date: date
    time_zone: str
    delivered_after: int
    delivered_before: int

    @classmethod
    def at(cls, now: datetime, time_zone: str) -> DeliveryDay:
        if now.tzinfo is None:
            raise ValueError("A delivery day requires an aware timestamp")
        zone = ZoneInfo(time_zone)
        day = now.astimezone(zone).date()
        start = datetime.combine(day, time.min, zone)
        end = datetime.combine(day + timedelta(days=1), time.min, zone)
        return cls(day, time_zone, int(start.timestamp()), int(end.timestamp()))

    def query(self) -> dict[str, str]:
        return {
            "delivered_after": str(self.delivered_after),
            "delivered_before": str(self.delivered_before),
        }

    def presence(self) -> DeliveryDayReport:
        return {
            "date": self.date.isoformat(),
            "time_zone": self.time_zone,
            "delivered_after": self.delivered_after,
            "delivered_before": self.delivered_before,
        }


class ShipmentItemAttributes(TypedDict):
    """Only dashboard fields leave the coordinator, never tracking numbers."""

    id: str
    title: str
    carrier: str
    status: str
    delivered_at: str | None
    estimated_delivery: str | None
    url: str


class ShipmentItemsAttributes(TypedDict):
    items: list[ShipmentItemAttributes]
    items_truncated: bool


@dataclass(frozen=True, slots=True)
class ShipmentItem:
    """A bounded dashboard projection of one backend shipment."""

    id: UUID
    title: str
    carrier: str
    status: ShipmentStatus
    delivered_at: datetime | None
    estimated_delivery: date | datetime | None

    @classmethod
    def parse(cls, record: object, day: DeliveryDay | None) -> ShipmentItem:
        if not isinstance(record, dict):
            raise ValueError("Invalid shipment item")
        identity = record.get("id")
        title = record.get("title")
        tracking = record.get("tracking")
        carrier_tracking = record.get("carrier_tracking")
        status = record.get("status")
        if (
            not isinstance(identity, str)
            or not isinstance(title, str)
            or not isinstance(status, str)
            or not isinstance(tracking, dict)
            or not isinstance(tracking.get("carrier"), str)
            or not tracking["carrier"]
            or not isinstance(carrier_tracking, dict)
            or record.get("direction") != Direction.INBOUND
            or record.get("archived") is not False
        ):
            raise ValueError("Invalid inbound shipment item")
        delivered_value = carrier_tracking.get("delivered_at")
        delivered = None
        if delivered_value is not None:
            if not isinstance(delivered_value, str):
                raise ValueError("Invalid carrier delivery timestamp")
            delivered = dt_util.parse_datetime(delivered_value)
            if delivered is None or delivered.tzinfo is None:
                raise ValueError("Invalid carrier delivery timestamp")
        parsed_status = ShipmentStatus(status)
        if day is not None:
            # A later manual status correction does not erase a carrier scan.
            if delivered is None or not (
                day.delivered_after <= delivered.timestamp() < day.delivered_before
            ):
                raise ValueError("Shipment is outside the local delivery day")
        elif parsed_status is not ShipmentStatus.OUT_FOR_DELIVERY:
            raise ValueError("Shipment is not out for delivery")
        return cls(
            UUID(identity),
            title,
            tracking["carrier"],
            parsed_status,
            delivered,
            parse_delivery_estimate(carrier_tracking.get("estimated_delivery_at")),
        )

    def attributes(self) -> ShipmentItemAttributes:
        fragment = quote(f"shipment={self.id}", safe="")
        return {
            "id": str(self.id),
            "title": self.title,
            "carrier": self.carrier,
            "status": self.status.value,
            "delivered_at": (
                self.delivered_at.isoformat() if self.delivered_at is not None else None
            ),
            "estimated_delivery": (
                self.estimated_delivery.isoformat()
                if self.estimated_delivery is not None
                else None
            ),
            "url": f"/shipments/shipments/{fragment}",
        }


@dataclass(frozen=True, slots=True)
class ShipmentItems:
    """Items and their truncation marker are published with the same count."""

    items: tuple[ShipmentItem, ...] = ()
    truncated: bool = False

    def attributes(self) -> ShipmentItemsAttributes:
        return {
            "items": [item.attributes() for item in self.items],
            "items_truncated": self.truncated,
        }


@dataclass(frozen=True, slots=True)
class DeliveryEstimate:
    """A carrier estimate for an active inbound shipment, never a promise."""

    shipment_id: UUID
    title: str
    carrier: str | None
    estimated: date | datetime

    def calendar_event(self) -> CalendarEvent:
        timed = isinstance(self.estimated, datetime)
        return CalendarEvent(
            start=self.estimated,
            end=self.estimated + (timedelta(hours=1) if timed else timedelta(days=1)),
            summary=self.title,
            description=(
                f"Estimated delivery{f' via {self.carrier}' if self.carrier else ''}. "
                "Carrier estimates can change."
            ),
            uid=str(self.shipment_id),
        )


def delivery_estimate(record: object) -> tuple[UUID, DeliveryEstimate | None]:
    """Validate identity and parse a supplied ETA without inventing missing dates."""
    if not isinstance(record, dict):
        raise ValueError("Invalid shipment record")
    shipment_id = record.get("id")
    title = record.get("title")
    if not isinstance(shipment_id, str) or not isinstance(title, str):
        raise ValueError("Invalid shipment record")
    identity = UUID(shipment_id)
    value = record.get("estimated_delivery_at")
    if value is None:
        return identity, None
    carrier = record.get("carrier")
    if carrier is not None and not isinstance(carrier, str):
        raise ValueError("Invalid shipment carrier")
    estimated = parse_delivery_estimate(value)
    return identity, (
        DeliveryEstimate(identity, title, carrier, estimated)
        if estimated is not None
        else None
    )


def parse_delivery_estimate(value: object) -> date | datetime | None:
    """Share the calendar's nullable ISO date/aware-datetime interpretation."""
    if value is None:
        return None
    if not isinstance(value, str):
        raise ValueError("Invalid carrier delivery estimate")
    if len(value) == 10:
        return dt_util.parse_date(value)
    timestamp = dt_util.parse_datetime(value)
    return timestamp if timestamp is not None and timestamp.tzinfo is not None else None
