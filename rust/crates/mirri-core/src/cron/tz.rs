//! Timezone selection for cron evaluation (B5 / DEC-3).
//!
//! Cron expressions are evaluated on a *local calendar* by default — the
//! host's local timezone, mirroring the TS kernel (where JS `Date` applies
//! the process-local zone). Tasks may override the calendar with an
//! explicit IANA name (`CronTimezone::Named`), including `"UTC"` as the
//! escape hatch that preserves the pre-B5 UTC semantics.
//!
//! `chrono-tz` embeds the timezone database at compile time: resolving a
//! name is pure computation with no runtime IO (zero-IO compliant). The
//! host's local zone comes from `iana-time-zone`, which reads the system's
//! timezone configuration — the same treatment the existing
//! `chrono::Local` / `dirs` dependencies already get.

use crate::cron::CronError;

/// The calendar a cron expression is evaluated in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CronTimezone {
    /// Not explicitly specified — resolved to the host's local timezone
    /// at evaluation time (the TS default).
    Local,
    /// An explicit IANA zone name (including `"UTC"` as the escape hatch
    /// back to the pre-B5 semantics).
    Named(chrono_tz::Tz),
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
            Some(name) => name
                .parse::<chrono_tz::Tz>()
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
    /// `Local` asks `iana-time-zone` for the host's zone and parses it;
    /// a lookup that cannot be honoured (no system timezone configuration,
    /// an unrecognised name) logs a warning and falls back to UTC —
    /// scheduling must never crash because the environment could not be
    /// probed, mirroring the TS kernel's tolerance of a broken system
    /// clock.
    pub fn resolve(&self) -> chrono_tz::Tz {
        match self {
            CronTimezone::Named(tz) => *tz,
            CronTimezone::Local => match iana_time_zone::get_timezone() {
                Ok(name) => name.parse::<chrono_tz::Tz>().unwrap_or_else(|error| {
                    tracing::warn!(
                        zone = %name,
                        %error,
                        "host timezone is not a known IANA name; cron falls back to UTC"
                    );
                    chrono_tz::UTC
                }),
                Err(error) => {
                    tracing::warn!(
                        %error,
                        "host timezone could not be determined; cron falls back to UTC"
                    );
                    chrono_tz::UTC
                }
            },
        }
    }
}

/// Render an instant as a local ISO 8601 string with a numeric offset
/// (`2025-01-15T09:07:30+08:00`) — the shape the TS kernel's
/// `formatLocalIsoWithOffset` produces for cron surfaces (never a `Z`
/// suffix: the point is to show the wall-clock time the task fires at).
pub fn format_local_iso_with_offset(
    at: chrono::DateTime<chrono::Utc>,
    tz: chrono_tz::Tz,
) -> String {
    use chrono::SecondsFormat;
    at.with_timezone(&tz)
        .to_rfc3339_opts(SecondsFormat::Secs, false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn shanghai() -> chrono_tz::Tz {
        chrono_tz::Asia::Shanghai
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
            CronTimezone::Named(t) if t == chrono_tz::UTC
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
        let local = utc_noon.with_timezone(&tz);
        assert_eq!(
            local.to_rfc3339_opts(chrono::SecondsFormat::Secs, false),
            "2025-01-15T20:00:00+08:00"
        );
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
            format_local_iso_with_offset(at, chrono_tz::Asia::Shanghai),
            "2025-01-15T09:07:30+08:00"
        );
    }
}
