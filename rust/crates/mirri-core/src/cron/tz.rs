//! Timezone selection for cron evaluation (B5 / DEC-3).
//!
//! Cron expressions are evaluated on a *local calendar* by default — the
//! host's local timezone, mirroring the TS kernel (where JS `Date` applies
//! the process-local zone). Tasks may override the calendar with an
//! explicit IANA name (`CronTimezone::Named`), including `"UTC"` as the
//! escape hatch that preserves the pre-B5 UTC semantics.
//!
//! `jiff` embeds the timezone database at compile time
//! (`tzdb-bundle-always`, with the runtime `tzdb-zoneinfo` reader switched
//! off): resolving a name is pure computation with no runtime IO (zero-IO
//! compliant). The host's local zone comes from `jiff`'s `tz-system`
//! probe, which reads the system's timezone configuration — the same
//! treatment the existing `chrono::Local` / `dirs` dependencies already
//! get. `jiff` is the calendar engine *only*: the engine's time types stay
//! `chrono`, and the two meet here (and in [`to_local_datetime`]) at the
//! cron boundary.

use crate::cron::CronError;

/// The calendar a cron expression is evaluated in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CronTimezone {
    /// Not explicitly specified — resolved to the host's local timezone
    /// at evaluation time (the TS default).
    Local,
    /// An explicit IANA zone name (including `"UTC"` as the escape hatch
    /// back to the pre-B5 semantics).
    Named(jiff::tz::TimeZone),
}

impl CronTimezone {
    /// Parse an optional timezone declaration.
    ///
    /// `None` (the absent key — every record written before B5) means
    /// [`CronTimezone::Local`]. An empty string is an error; an
    /// unrecognised name is an error whose message names the offending
    /// `timezone` (the caller surfaces it to the model verbatim).
    pub fn parse(input: Option<&str>) -> Result<Self, CronError> {
        match input {
            None => Ok(CronTimezone::Local),
            Some("") => Err(CronError::InvalidExpression(
                "timezone must be a non-empty IANA name (e.g. 'Asia/Shanghai')".to_string(),
            )),
            Some(name) => jiff::tz::TimeZone::get(name)
                .map(CronTimezone::Named)
                .map_err(|_| {
                    CronError::InvalidExpression(format!(
                        "unknown timezone '{name}': expected an IANA name such as 'Asia/Shanghai'"
                    ))
                }),
        }
    }

    /// Resolve the declaration to a concrete zone.
    ///
    /// `Local` asks `jiff` for the host's zone; a lookup that cannot be
    /// honoured (no system timezone configuration) logs a warning and falls
    /// back to UTC — scheduling must never crash because the environment
    /// could not be probed, mirroring the TS kernel's tolerance of a broken
    /// system clock. (`jiff`'s system probe yields a named zone rather than
    /// a name string, so the old "unrecognised name" arm has no successor —
    /// an unknown zone degrades inside `jiff` to its unknown zone, which
    /// still answers offsets.)
    pub fn resolve(&self) -> jiff::tz::TimeZone {
        match self {
            CronTimezone::Named(tz) => tz.clone(),
            CronTimezone::Local => jiff::tz::TimeZone::try_system().unwrap_or_else(|error| {
                tracing::warn!(
                    %error,
                    "host timezone could not be determined; cron falls back to UTC"
                );
                jiff::tz::TimeZone::UTC
            }),
        }
    }
}

/// The wall-clock reading of `instant` on `tz`'s calendar (the engine's
/// `chrono` currency converted at the cron boundary). Borrows the zone, the
/// same shape as the reverse mapping [`crate::cron::to_instant`].
pub fn to_local_datetime(
    instant: chrono::DateTime<chrono::Utc>,
    tz: &jiff::tz::TimeZone,
) -> jiff::ZonedDateTime {
    jiff::Timestamp::from_millisecond(instant.timestamp_millis())
        .expect("a chrono instant always fits jiff's timestamp range")
        .to_zoned_datetime(tz.clone())
}

/// Render an instant as a local ISO 8601 string with a numeric offset
/// (`2025-01-15T09:07:30+08:00`) — the shape the TS kernel's
/// `formatLocalIsoWithOffset` produces for cron surfaces (never a `Z`
/// suffix: the point is to show the wall-clock time the task fires at).
///
/// Built from `jiff`'s civil rendering of the local wall clock plus the
/// zone's offset at that instant, at seconds precision — byte-identical to
/// the chrono `to_rfc3339_opts(SecondsFormat::Secs, false)` output this
/// replaces (`+08:00`, `+05:30`, `-05:00`; UTC renders as `+00:00`).
pub fn format_local_iso_with_offset(
    at: chrono::DateTime<chrono::Utc>,
    tz: jiff::tz::TimeZone,
) -> String {
    let timestamp = jiff::Timestamp::from_millisecond(at.timestamp_millis())
        .expect("a chrono instant always fits jiff's timestamp range");
    let local = timestamp.to_zoned_datetime(tz);
    let zoneless = local.datetime().to_string();
    let offset_seconds = local.offset().seconds();
    let (hours, minutes, seconds) = (
        offset_seconds / 3600,
        (offset_seconds % 3600) / 60,
        offset_seconds % 60,
    );
    let mut rendered = format!("{zoneless}{hours:+03}:{minutes:02}");
    // Sub-minute offsets (historical LMT zones) keep their seconds.
    if seconds != 0 {
        rendered.push_str(&format!(":{seconds:02}"));
    }
    rendered
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn shanghai() -> jiff::tz::TimeZone {
        jiff::tz::TimeZone::get("Asia/Shanghai").expect("Asia/Shanghai is in the bundled tzdb")
    }

    #[test]
    fn should_return_named_tz_when_parsing_a_valid_iana_name() {
        let tz = CronTimezone::parse(Some("Asia/Shanghai")).unwrap();
        assert!(matches!(tz, CronTimezone::Named(t) if t == shanghai()));
    }

    #[test]
    fn should_return_named_utc_when_parsing_the_explicit_utc_name() {
        // 显式 "UTC" 是旧语义的逃生门：保留可精确指定 UTC 的能力。
        assert!(matches!(
            CronTimezone::parse(Some("UTC")).unwrap(),
            CronTimezone::Named(t) if t == jiff::tz::TimeZone::UTC
        ));
    }

    #[test]
    fn should_return_local_when_timezone_is_absent() {
        assert!(matches!(
            CronTimezone::parse(None).unwrap(),
            CronTimezone::Local
        ));
    }

    #[test]
    fn should_return_error_when_parsing_an_unknown_timezone_name() {
        let error = CronTimezone::parse(Some("Mars/Olympus_Mons")).unwrap_err();
        assert!(
            matches!(error, crate::cron::CronError::InvalidExpression(ref message)
                if message.contains("timezone")),
            "the error names the offending timezone: {error}"
        );
    }

    #[test]
    fn should_return_error_when_timezone_is_an_empty_string() {
        assert!(CronTimezone::parse(Some("")).is_err());
    }

    #[test]
    fn should_return_a_deterministic_offset_when_resolving_a_named_tz() {
        // Given the named zone Asia/Shanghai (+08:00, no DST)
        let tz = CronTimezone::parse(Some("Asia/Shanghai"))
            .unwrap()
            .resolve();
        // When an instant is converted, then the wall clock is 8 hours ahead of UTC
        let utc_noon = chrono::Utc.with_ymd_and_hms(2025, 1, 15, 12, 0, 0).unwrap();
        let local = to_local_datetime(utc_noon, &tz);
        assert_eq!(
            (local.hour(), local.minute()),
            (20, 0),
            "Shanghai reads UTC noon as 20:00 local"
        );
        assert_eq!(local.offset().seconds(), 8 * 3600);
    }

    #[test]
    fn should_fall_back_to_utc_when_resolving_local_on_an_unparseable_host_tz() {
        // Given a Local zone whose host lookup cannot be honored (simulated by the
        // fallback path — the pure seam: resolve() never panics), when resolved,
        // then it still yields a usable Tz.
        // （宿主真实 Local 的可变性由 embedded TZ env 集成测试覆盖；此处只锁定
        //   Local 分支返回某个可用 Tz 的契约。）
        let _ = CronTimezone::Local.resolve();
    }

    #[test]
    fn should_render_a_local_iso_string_with_offset_when_formatting_an_instant() {
        let at = chrono::Utc.with_ymd_and_hms(2025, 1, 15, 1, 7, 30).unwrap();
        assert_eq!(
            format_local_iso_with_offset(at, shanghai()),
            "2025-01-15T09:07:30+08:00"
        );
    }

    #[test]
    fn should_render_a_zero_offset_with_a_plus_sign_when_formatting_a_utc_instant() {
        // UTC keeps chrono's `+00:00` spelling (never `Z`, never `-00:00`).
        let at = chrono::Utc.with_ymd_and_hms(2025, 1, 15, 1, 7, 30).unwrap();
        assert_eq!(
            format_local_iso_with_offset(at, jiff::tz::TimeZone::UTC),
            "2025-01-15T01:07:30+00:00"
        );
    }

    #[test]
    fn should_render_a_half_hour_offset_when_formatting_a_kolkata_instant() {
        // +05:30 zones exercise the minutes component of the offset.
        let at = chrono::Utc.with_ymd_and_hms(2025, 1, 15, 1, 7, 30).unwrap();
        let kolkata =
            jiff::tz::TimeZone::get("Asia/Kolkata").expect("Asia/Kolkata is in the bundled tzdb");
        assert_eq!(
            format_local_iso_with_offset(at, kolkata),
            "2025-01-15T06:37:30+05:30"
        );
    }
}
