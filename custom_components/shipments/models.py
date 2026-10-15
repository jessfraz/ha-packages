"""Validate the small backend projections consumed by native entities."""

from dataclasses import dataclass
from datetime import date, datetime, timedelta
from enum import StrEnum
from uuid import UUID

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
    if not isinstance(value, str):
        raise ValueError("Invalid carrier delivery estimate")
    carrier = record.get("carrier")
    if carrier is not None and not isinstance(carrier, str):
        raise ValueError("Invalid shipment carrier")
    estimated: date | datetime | None = None
    if len(value) == 10:
        estimated = dt_util.parse_date(value)
    else:
        timestamp = dt_util.parse_datetime(value)
        if timestamp is not None and timestamp.tzinfo is not None:
            estimated = timestamp
    return identity, (
        DeliveryEstimate(identity, title, carrier, estimated)
        if estimated is not None
        else None
    )
