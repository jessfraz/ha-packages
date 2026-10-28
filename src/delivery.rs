//! UTC delivery windows supplied by Home Assistant's configured local day.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use time::{Date, OffsetDateTime};

use crate::error::Error;

/// Carrier ETA precision: date-only values never become UTC appointments.
/// EasyPost's untimed midnight-UTC default encodes a civil date. Explicit
/// carrier times (including midnight), offsets and fractional seconds remain
/// instants. Actual delivery timestamps and paired windows never use this rule.
pub(crate) fn estimate(
    value: &str,
    local_date: Option<&str>,
    explicit_time: bool,
) -> Option<String> {
    let format = time::format_description::parse_borrowed::<2>("[year]-[month]-[day]").ok()?;
    if value.len() == 10 {
        return Date::parse(value, &format).ok().map(|day| day.to_string());
    }
    let at = OffsetDateTime::parse(value, &time::format_description::well_known::Rfc3339).ok()?;
    if !explicit_time && at.offset() == time::UtcOffset::UTC && at.time() == time::Time::MIDNIGHT {
        return Some(
            local_date
                .and_then(|day| Date::parse(day, &format).ok())
                .unwrap_or(at.date())
                .to_string(),
        );
    }
    at.to_offset(time::UtcOffset::UTC)
        .format(&time::format_description::well_known::Rfc3339)
        .ok()
}

/// Read old EasyPost snapshots through the same contract as new ingestion.
/// New snapshots preserve their raw ETA, so explicit midnight timestamps can
/// never be mistaken for legacy date defaults. This does not rewrite history.
pub(crate) fn legacy_estimate(
    value: Option<String>,
    raw: Option<&str>,
    easypost: bool,
) -> Option<String> {
    match value {
        Some(value) if raw.is_none() && easypost => estimate(&value, None, false),
        value => value,
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DeliveryWindow {
    after: i64,
    before: i64,
}

impl DeliveryWindow {
    pub fn from_bounds(after: Option<i64>, before: Option<i64>) -> Result<Option<Self>, Error> {
        match (after, before) {
            (None, None) => Ok(None),
            (Some(after), Some(before)) => {
                if OffsetDateTime::from_unix_timestamp(after).is_err()
                    || OffsetDateTime::from_unix_timestamp(before).is_err()
                {
                    return Err(Error::Invalid(
                        "delivery bounds must be valid UTC timestamps".into(),
                    ));
                }
                if after >= before {
                    return Err(Error::Invalid(
                        "delivered_after must be earlier than delivered_before".into(),
                    ));
                }
                Ok(Some(Self { after, before }))
            }
            _ => Err(Error::Invalid(
                "delivered_after and delivered_before must be supplied together".into(),
            )),
        }
    }

    pub fn after(self) -> i64 {
        self.after
    }

    pub fn before(self) -> i64 {
        self.before
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct DeliveryDay {
    pub date: String,
    pub time_zone: String,
    pub delivered_after: i64,
    pub delivered_before: i64,
}

impl DeliveryDay {
    pub fn valid(&self) -> bool {
        let Ok(format) = time::format_description::parse_borrowed::<2>("[year]-[month]-[day]")
        else {
            return false;
        };
        self.date.len() == 10
            && Date::parse(&self.date, &format).is_ok()
            && !self.time_zone.is_empty()
            && self.time_zone.len() <= 128
            && self.time_zone.bytes().all(|byte| {
                byte.is_ascii_alphanumeric() || matches!(byte, b'/' | b'_' | b'+' | b'-')
            })
            && DeliveryWindow::from_bounds(Some(self.delivered_after), Some(self.delivered_before))
                .is_ok_and(|window| window.is_some())
            // Local civil days can change length at DST transitions, including
            // the half-hour shift in Australia/Lord_Howe.
            && (23 * 3600..=25 * 3600)
                .contains(&self.delivered_before.saturating_sub(self.delivered_after))
    }

    pub fn contains(&self, timestamp: i64) -> bool {
        self.delivered_after <= timestamp && timestamp < self.delivered_before
    }
}

#[cfg(test)]
mod tests {
    use crate::delivery::{DeliveryDay, DeliveryWindow};

    #[test]
    fn delivery_windows_require_a_valid_ordered_pair() {
        assert_eq!(DeliveryWindow::from_bounds(None, None).unwrap(), None);
        for (after, before) in [
            (Some(1), None),
            (None, Some(2)),
            (Some(2), Some(2)),
            (Some(3), Some(2)),
            (Some(i64::MIN), Some(i64::MAX)),
        ] {
            assert!(DeliveryWindow::from_bounds(after, before).is_err());
        }
        let window = DeliveryWindow::from_bounds(Some(1), Some(2))
            .unwrap()
            .unwrap();
        assert_eq!(window.after(), 1);
        assert_eq!(window.before(), 2);
    }

    #[test]
    fn local_day_reports_allow_dst_and_reject_malformed_windows() {
        let day = DeliveryDay {
            date: "2026-03-08".into(),
            time_zone: "America/Los_Angeles".into(),
            delivered_after: 1_772_956_800,
            delivered_before: 1_773_039_600,
        };
        assert!(day.valid());
        assert!(day.contains(day.delivered_after));
        assert!(!day.contains(day.delivered_before));
        for length in [23 * 3600, 24 * 3600, 25 * 3600, 47 * 1800] {
            let mut changed = day.clone();
            changed.delivered_before = changed.delivered_after + length;
            assert!(changed.valid());
        }
        for (date, zone, length) in [
            ("2026-02-30", "America/Los_Angeles", 86_400),
            ("2026-03-08", "", 86_400),
            ("2026-03-08", "America/Los_Angeles\n", 86_400),
            ("2026-03-08", "America/Los_Angeles", 1),
        ] {
            let changed = DeliveryDay {
                date: date.into(),
                time_zone: zone.into(),
                delivered_before: day.delivered_after + length,
                ..day.clone()
            };
            assert!(!changed.valid());
        }
    }
}

#[cfg(test)]
mod estimate_tests {
    use super::{estimate, legacy_estimate};

    #[test]
    fn precision_is_preserved_at_ingestion_and_legacy_readback() {
        for day in ["2026-03-08", "2026-11-01", "2026-10-28"] {
            let raw = format!("{day}T00:00:00Z");
            assert_eq!(estimate(&raw, None, false).as_deref(), Some(day));
            assert_eq!(estimate(&raw, None, true).as_deref(), Some(raw.as_str()));
            assert_eq!(
                legacy_estimate(Some(raw.clone()), None, true).as_deref(),
                Some(day)
            );
            assert_eq!(
                legacy_estimate(Some(raw.clone()), Some(&raw), true).as_deref(),
                Some(raw.as_str())
            );
            assert_eq!(
                legacy_estimate(Some(raw.clone()), None, false).as_deref(),
                Some(raw.as_str())
            );
        }
        assert_eq!(
            estimate("2026-10-29T03:00:00Z", None, false).as_deref(),
            Some("2026-10-29T03:00:00Z")
        );
        assert_eq!(
            estimate("2026-10-28T00:00:00-07:00", None, false).as_deref(),
            Some("2026-10-28T07:00:00Z")
        );
        assert_eq!(
            estimate("2026-10-28T00:00:00.001Z", None, false).as_deref(),
            Some("2026-10-28T00:00:00.001Z")
        );
        assert!(estimate("2026-02-30", None, false).is_none());
    }
}
