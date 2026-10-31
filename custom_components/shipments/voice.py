"""Read-only shipment answers shared by native Assist and LLM tools."""

from dataclasses import dataclass
from datetime import date, datetime, timedelta
from enum import StrEnum
from typing import TypedDict
from urllib.parse import quote, urlencode
from uuid import UUID
from zoneinfo import ZoneInfo

from aiohttp import ClientError
from homeassistant.auth.permissions.const import POLICY_READ
from homeassistant.components.homeassistant import exposed_entities
from homeassistant.config_entries import ConfigEntry, ConfigEntryState
from homeassistant.core import Context, HomeAssistant
from homeassistant.helpers import entity_registry as er
from homeassistant.util import dt as dt_util

from .client import ResponseTooLarge
from .const import DOMAIN
from .coordinator import ShipmentsCoordinator
from .models import (
    DeliveryDay,
    DeliveryWindow,
    Direction,
    ShipmentStatus,
    carrier_estimated_delivery,
    parse_delivery_window,
    parse_provider_delivery_date,
)

_PAGE_LIMIT = 200
_SPOKEN_LIMIT = 5
_TERMINAL = {
    ShipmentStatus.DELIVERED,
    ShipmentStatus.RETURN_TO_SENDER,
    ShipmentStatus.CANCELLED,
}
_EXCEPTIONS = {
    ShipmentStatus.FAILURE,
    ShipmentStatus.ERROR,
    ShipmentStatus.RETURN_TO_SENDER,
}


class ShipmentQuery(StrEnum):
    ARRIVING = "arriving"
    OUT_FOR_DELIVERY = "out_for_delivery"
    DELIVERED = "delivered"
    STATUS = "status"
    ISSUES = "issues"


class ShipmentDay(StrEnum):
    TODAY = "today"
    TOMORROW = "tomorrow"


@dataclass(frozen=True, slots=True)
class VoiceRequest:
    query: ShipmentQuery
    direction: Direction | None = Direction.INBOUND
    day: ShipmentDay = ShipmentDay.TODAY
    name: str | None = None


class VoiceItem(TypedDict):
    id: str
    title: str
    carrier: str
    direction: str
    status: str
    delivered_at: str | None
    estimated_delivery: str | None
    tracking_issue: bool
    url: str


class VoiceResult(TypedDict):
    speech: str
    query: str
    direction: str | None
    day: str
    count: int | None
    count_exact: bool
    items: list[VoiceItem]
    truncated: bool
    error: str | None


@dataclass(frozen=True, slots=True)
class VoiceAnswer:
    """A safe projection; raw provider records and errors never leave this module."""

    speech: str
    card_text: str
    request: VoiceRequest
    items: tuple[_Shipment, ...] = ()
    count: int | None = None
    count_exact: bool = True
    error: str | None = None

    def as_dict(self) -> VoiceResult:
        return {
            "speech": self.speech,
            "query": self.request.query.value,
            "direction": self.request.direction.value
            if self.request.direction
            else None,
            "day": self.request.day.value,
            "count": self.count,
            "count_exact": self.count_exact,
            "items": [item.attributes() for item in self.items],
            "truncated": not self.count_exact
            or (self.count is not None and self.count > len(self.items)),
            "error": self.error,
        }


@dataclass(frozen=True, slots=True)
class _Shipment:
    id: UUID
    title: str
    carrier: str
    direction: Direction
    status: ShipmentStatus
    delivered_at: date | datetime | None
    estimated_delivery: date | datetime | None
    estimated_window: DeliveryWindow | None
    tracking_issue: bool
    archived: bool

    @classmethod
    def parse(cls, record: object) -> _Shipment:
        if not isinstance(record, dict):
            raise ValueError("Invalid shipment")
        identity, title = record.get("id"), record.get("title")
        direction, status = record.get("direction"), record.get("status")
        tracking, carrier_tracking = (
            record.get("tracking"),
            record.get("carrier_tracking"),
        )
        sync, archived = record.get("sync"), record.get("archived")
        if (
            not isinstance(identity, str)
            or not isinstance(title, str)
            or not isinstance(direction, str)
            or not isinstance(status, str)
            or not isinstance(tracking, dict)
            or not isinstance(carrier := tracking.get("carrier"), str)
            or not carrier
            or not isinstance(carrier_tracking, dict)
            or not isinstance(sync, dict)
            or type(archived) is not bool
        ):
            raise ValueError("Invalid shipment projection")
        delivered = carrier_tracking.get("delivered_at")
        delivered_at = None
        if delivered is not None:
            if not isinstance(delivered, str):
                raise ValueError("Invalid delivery timestamp")
            delivered_at = dt_util.parse_datetime(delivered)
            if delivered_at is None or delivered_at.tzinfo is None:
                raise ValueError("Invalid delivery timestamp")
        if delivered_at is None:
            reported = parse_provider_delivery_date(
                carrier_tracking.get("delivery_date"), delivered=True
            )
            if isinstance(reported, date):
                delivered_at = reported
        estimated = carrier_estimated_delivery(carrier_tracking)
        window = None
        # Assist previously used only point ETAs. Add civil ranges without
        # reinterpreting the calendar's legacy aware-time window semantics.
        canonical_window = parse_delivery_window(
            carrier_tracking.get("estimated_delivery_window")
        )
        if estimated is None and canonical_window is None:
            reported = parse_provider_delivery_date(
                carrier_tracking.get("delivery_date"), delivered=False
            )
            if isinstance(reported, DeliveryWindow):
                window = reported
        error = sync.get("last_error")
        if error is not None and not isinstance(error, str):
            raise ValueError("Invalid tracking state")
        return cls(
            UUID(identity),
            title,
            carrier,
            Direction(direction),
            ShipmentStatus(status),
            delivered_at,
            estimated,
            window,
            error is not None,
            archived,
        )

    def attributes(self) -> VoiceItem:
        return {
            "id": str(self.id),
            "title": self.title,
            "carrier": self.carrier,
            "direction": self.direction.value,
            "status": self.status.value,
            "delivered_at": self.delivered_at.isoformat()
            if self.delivered_at
            else None,
            "estimated_delivery": (
                self.estimated_delivery.isoformat() if self.estimated_delivery else None
            ),
            "tracking_issue": self.tracking_issue,
            "url": f"/shipments/shipments/{quote(f'shipment={self.id}', safe='')}",
        }


def _error(request: VoiceRequest, code: str, speech: str) -> VoiceAnswer:
    return VoiceAnswer(speech, speech, request, count_exact=False, error=code)


def _connection(
    hass: HomeAssistant, assistant: str | None
) -> tuple[ConfigEntry[ShipmentsCoordinator] | None, str | None, str | None]:
    entries = hass.config_entries.async_entries(DOMAIN)
    if len(entries) != 1 or entries[0].state is not ConfigEntryState.LOADED:
        return None, None, "unavailable"
    entry = entries[0]
    registry = er.async_get(hass)
    gateway = registry.async_get_entity_id(
        "sensor", DOMAIN, f"{entry.entry_id}_inbound_active"
    )
    if not gateway or not exposed_entities.async_should_expose(
        hass, assistant or "conversation", gateway
    ):
        return None, None, "not_exposed"
    coordinator = entry.runtime_data
    state = hass.states.get(gateway)
    if (
        not isinstance(coordinator, ShipmentsCoordinator)
        or not coordinator.last_update_success
        or coordinator.data is None
        or "inbound_delivered_today" not in coordinator.data
        or state is None
        or state.state in {"unavailable", "unknown"}
    ):
        # The coordinator omits the local-day projection on a version mismatch.
        return None, None, "unavailable"
    return entry, gateway, None


def query_enabled(hass: HomeAssistant, assistant: str | None = "conversation") -> bool:
    """Offer tools only for a loaded connection whose gateway is exposed."""
    entry, _, _ = _connection(hass, assistant)
    return entry is not None


async def _coordinator(
    hass: HomeAssistant, context: Context, assistant: str | None
) -> tuple[ShipmentsCoordinator | None, str | None]:
    entry, gateway, error = _connection(hass, assistant)
    if entry is None or gateway is None:
        return None, error
    if context.user_id is not None:
        user = await hass.auth.async_get_user(context.user_id)
        if (
            user is None
            or not user.is_active
            or not user.permissions.check_entity(gateway, POLICY_READ)
        ):
            return None, "unauthorized"
    # A user lookup can overlap an unload or exposure change.
    current, _, error = _connection(hass, assistant)
    return (entry.runtime_data, None) if current is entry else (None, error)


async def _page(
    coordinator: ShipmentsCoordinator, query: dict[str, str]
) -> tuple[list[_Shipment], bool]:
    response = await coordinator.client.request(
        "GET", f"/v1/shipments?{urlencode(query)}"
    )
    if response.status != 200:
        raise ValueError("Cannot read shipments")
    data = response.json()
    if not isinstance(data, dict) or not isinstance(data.get("items"), list):
        raise ValueError("Invalid shipment page")
    records, cursor = data["items"], data.get("next_page")
    if len(records) > _PAGE_LIMIT or (
        cursor is not None and (not isinstance(cursor, str) or not cursor)
    ):
        raise ValueError("Invalid shipment page bounds")
    items = [_Shipment.parse(record) for record in records]
    if len({item.id for item in items}) != len(items):
        raise ValueError("Duplicate shipment in page")
    return items, cursor is not None


def _civil_day(value: date | datetime, zone: ZoneInfo) -> date:
    return value.astimezone(zone).date() if isinstance(value, datetime) else value


def _due_on(item: _Shipment, target: date, zone: ZoneInfo) -> bool:
    if item.estimated_delivery is not None:
        return _civil_day(item.estimated_delivery, zone) == target
    window = item.estimated_window
    return window is not None and (
        _civil_day(window.start, zone) <= target <= _civil_day(window.end, zone)
    )


def _date_sort(value: date | datetime | None, zone: ZoneInfo) -> tuple[date, float]:
    if isinstance(value, datetime):
        return value.astimezone(zone).date(), value.timestamp()
    # A date has no observed clock time. Sort it deterministically without
    # manufacturing a midnight delivery timestamp.
    return (value, float("-inf")) if value is not None else (date.max, float("inf"))


def _estimate_sort(item: _Shipment, zone: ZoneInfo) -> tuple[date, float, str, str]:
    estimate = item.estimated_delivery
    if estimate is None and item.estimated_window is not None:
        estimate = item.estimated_window.start
    return *_date_sort(estimate, zone), item.title.casefold(), str(item.id)


def _detail(item: _Shipment, request: VoiceRequest, zone: ZoneInfo) -> str:
    detail = f"{item.title} via {item.carrier}, {item.status.value.replace('_', ' ')}"
    if item.delivered_at and (
        request.query is ShipmentQuery.DELIVERED
        or item.status is ShipmentStatus.DELIVERED
    ):
        if isinstance(item.delivered_at, datetime):
            local = item.delivered_at.astimezone(zone)
            stamp = (
                local.strftime("at %-I:%M %p")
                if request.query is ShipmentQuery.DELIVERED
                else local.strftime("on %B %-d at %-I:%M %p")
            )
        else:
            stamp = item.delivered_at.strftime("on %B %-d")
        detail = f"{item.title} via {item.carrier}, delivered {stamp}"
    elif item.estimated_delivery is not None:
        estimate = item.estimated_delivery
        if isinstance(estimate, datetime):
            text = estimate.astimezone(zone).strftime("%B %-d at %-I:%M %p")
        else:
            text = estimate.strftime("%B %-d")
        detail += f", estimated {text}"
    elif item.estimated_window is not None:
        window = item.estimated_window
        detail += (
            f", estimated {_civil_day(window.start, zone).strftime('%B %-d')}"
            f" to {_civil_day(window.end, zone).strftime('%B %-d')}"
        )
    elif request.query in {ShipmentQuery.ARRIVING, ShipmentQuery.STATUS}:
        detail += ", delivery estimate unavailable"
    if item.tracking_issue:
        detail += ", tracking update problem"
    return detail + "."


async def async_query(
    hass: HomeAssistant,
    request: VoiceRequest,
    context: Context,
    assistant: str | None,
) -> VoiceAnswer:
    """Answer from synchronized database rows, without contacting a carrier."""
    coordinator, access_error = await _coordinator(hass, context, assistant)
    if coordinator is None:
        return _error(
            request,
            access_error or "unavailable",
            "Expose the Shipments inbound active sensor to this assistant first."
            if access_error == "not_exposed"
            else "Shipment information is unavailable to this assistant right now.",
        )
    if request.query is ShipmentQuery.STATUS:
        if request.name is None or not request.name.strip():
            return _error(
                request, "name_required", "Which shipment would you like to check?"
            )
        if len(request.name) > 500:
            return _error(
                request,
                "invalid_name",
                "Shipment titles must be 500 characters or fewer.",
            )
    zone = ZoneInfo(hass.config.time_zone)
    now = dt_util.utcnow()
    today = now.astimezone(zone).date()
    target = today + timedelta(days=request.day is ShipmentDay.TOMORROW)
    query = {"archived": "false", "limit": str(_PAGE_LIMIT), "sort": "expected"}
    if request.direction is not None:
        query["direction"] = request.direction.value
    if request.query is ShipmentQuery.ARRIVING:
        query["active"] = "true"
    elif request.query is ShipmentQuery.OUT_FOR_DELIVERY:
        query["status"] = ShipmentStatus.OUT_FOR_DELIVERY.value
    elif request.query is ShipmentQuery.DELIVERED:
        day = DeliveryDay.at(
            now.astimezone(zone) + timedelta(days=(target - today).days),
            hass.config.time_zone,
        )
        query.update(day.query())
        query["sort"] = "delivered"
    elif request.query is ShipmentQuery.STATUS:
        query["search"] = (request.name or "").strip()
    else:
        query["tracking_issues"] = "true"
    try:
        items, incomplete = await _page(coordinator, query)
        if request.query is ShipmentQuery.ISSUES:
            # Exception status and synchronization failures overlap. Read both
            # bounded sets, then merge by durable shipment identity.
            query.pop("tracking_issues")
            query["exceptions"] = "true"
            exceptions, more = await _page(coordinator, query)
            items = list({item.id: item for item in [*items, *exceptions]}.values())
            incomplete |= more
    except ClientError, TimeoutError, ResponseTooLarge, ValueError:
        return _error(
            request,
            "backend_unavailable",
            "I couldn't read shipment information. Please try again shortly.",
        )
    # Awaiting a request can overlap unloading, exposure changes or user revocation.
    current, access_error = await _coordinator(hass, context, assistant)
    if current is not coordinator:
        return _error(
            request,
            access_error or "unavailable",
            "Shipment information is unavailable to this assistant right now.",
        )
    items = [
        item
        for item in items
        if not item.archived
        and (request.direction is None or item.direction is request.direction)
    ]
    if request.query is ShipmentQuery.ARRIVING:
        items = [
            item
            for item in items
            if item.status not in _TERMINAL
            and (
                _due_on(item, target, zone)
                or (target == today and item.status is ShipmentStatus.OUT_FOR_DELIVERY)
            )
        ]
    elif request.query is ShipmentQuery.OUT_FOR_DELIVERY:
        items = [
            item for item in items if item.status is ShipmentStatus.OUT_FOR_DELIVERY
        ]
    elif request.query is ShipmentQuery.DELIVERED:
        items = [
            item
            for item in items
            if item.delivered_at is not None
            and _civil_day(item.delivered_at, zone) == target
        ]
    elif request.query is ShipmentQuery.ISSUES:
        items = [
            item for item in items if item.status in _EXCEPTIONS or item.tracking_issue
        ]
    else:
        name = (request.name or "").strip().casefold()
        items = [item for item in items if name in item.title.casefold()]
        exact = [item for item in items if item.title.strip().casefold() == name]
        if exact:
            items = exact
        if len(items) > 1 or incomplete:
            return _error(
                request,
                "ambiguous_name",
                "More than one shipment may match. "
                "Please use a more specific shipment title.",
            )
    if request.query is ShipmentQuery.DELIVERED:
        items.sort(
            key=lambda item: _date_sort(item.delivered_at, zone),
            reverse=True,
        )
    else:
        items.sort(key=lambda item: _estimate_sort(item, zone))
    direction = f"{request.direction.value} " if request.direction else ""
    descriptions = {
        ShipmentQuery.ARRIVING: f"expected {request.day.value}",
        ShipmentQuery.OUT_FOR_DELIVERY: "out for delivery",
        ShipmentQuery.DELIVERED: f"delivered {request.day.value}",
        ShipmentQuery.STATUS: "matching that title",
        ShipmentQuery.ISSUES: "with delivery or tracking problems",
    }
    count = len(items)
    if not count:
        speech = (
            f"There are no {direction}shipments {descriptions[request.query]}."
            if not incomplete
            else "I couldn't find a matching shipment in the available records, "
            "but the results are incomplete."
        )
    elif request.query is ShipmentQuery.STATUS:
        speech = _detail(items[0], request, zone)
    else:
        noun = "shipment" if count == 1 else "shipments"
        prefix = "At least " if incomplete else ""
        description = descriptions[request.query]
        if request.query is ShipmentQuery.ISSUES:
            verb = "has" if count == 1 else "have"
            description = "delivery or tracking problems"
        elif request.query is ShipmentQuery.DELIVERED:
            verb = "was" if count == 1 else "were"
        else:
            verb = "is" if count == 1 else "are"
        speech = f"{prefix}{count} {direction}{noun} {verb} {description}. "
        speech += " ".join(
            _detail(item, request, zone) for item in items[:_SPOKEN_LIMIT]
        )
        remaining = count - _SPOKEN_LIMIT
        if remaining > 0:
            speech += f" And {remaining} more."
        if incomplete:
            speech += " More shipments may be available in the app."
    return VoiceAnswer(
        speech,
        speech,
        request,
        tuple(items[:_SPOKEN_LIMIT]),
        count,
        not incomplete,
    )
