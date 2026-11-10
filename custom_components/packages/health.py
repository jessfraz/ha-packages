"""Discovery progress, separate from individual parcel tracking updates."""

from dataclasses import asdict, dataclass
from enum import StrEnum

from homeassistant.util import dt as dt_util

# ConnectionReason is the source's fixed vocabulary, never provider free text.
_DISCOVERY_ERROR_CODES = frozenset(
    {
        "invalid_input",
        "expired_flow",
        "account_identity_missing",
        "account_mismatch",
        "session_changed",
        "upstream_rejected",
        "unexpected_response",
        "timeout",
        "connection",
        "rate_limited",
        "upstream_http",
        "persistence",
        "configuration",
        "unsupported_orders_layout",
        "challenge_page",
        "in_progress",
    }
)


class DiscoveryState(StrEnum):
    DISABLED = "disabled"
    PENDING = "pending"
    RUNNING = "running"
    COMPLETE = "complete"
    PARTIAL = "partial"
    RETRYING = "retrying"
    AUTH_REQUIRED = "auth_required"
    CHALLENGE_REQUIRED = "challenge_required"
    FAILED = "failed"
    STALE = "stale"


@dataclass(frozen=True, slots=True)
class DiscoveryHealth:
    state: DiscoveryState
    problem: bool
    checked_at: int
    last_attempt_at: int | None
    last_finished_at: int | None
    last_complete_at: int | None
    next_attempt_at: int
    running_until: int | None
    consecutive_failures: int
    last_outcome: str | None
    last_error_code: str | None
    has_more: bool
    deferred_parcels: int

    @classmethod
    def parse(cls, value: object) -> DiscoveryHealth | None:
        """An absent or invalid health report is unavailable, never healthy."""
        if not isinstance(value, dict):
            return None
        try:
            state = DiscoveryState(value.get("state"))
        except ValueError, TypeError:
            return None
        if any(type(value.get(key)) is not bool for key in ("problem", "has_more")):
            return None
        required = (
            "checked_at",
            "next_attempt_at",
            "consecutive_failures",
            "deferred_parcels",
        )
        optional = (
            "last_attempt_at",
            "last_finished_at",
            "last_complete_at",
            "running_until",
        )
        if any(type(value.get(key)) is not int or value[key] < 0 for key in required):
            return None
        if any(
            value.get(key) is not None
            and (type(value[key]) is not int or value[key] < 0)
            for key in optional
        ):
            return None
        outcome, error = value.get("last_outcome"), value.get("last_error_code")
        if outcome is not None and outcome not in (
            "complete",
            "partial",
            "blocked",
            "failed",
        ):
            return None
        if error is not None and (not isinstance(error, str) or len(error) > 100):
            return None
        if error not in _DISCOVERY_ERROR_CODES:
            error = None
        return cls(
            state,
            value["problem"],
            value["checked_at"],
            value.get("last_attempt_at"),
            value.get("last_finished_at"),
            value.get("last_complete_at"),
            value["next_attempt_at"],
            value.get("running_until"),
            value["consecutive_failures"],
            outcome,
            error,
            value["has_more"],
            value["deferred_parcels"],
        )

    @property
    def current(self) -> bool:
        return -60 <= dt_util.utcnow().timestamp() - self.checked_at <= 180

    def attributes(self) -> dict[str, object]:
        """Only fixed health fields leave the app, without account or parcel data."""
        return asdict(self)
