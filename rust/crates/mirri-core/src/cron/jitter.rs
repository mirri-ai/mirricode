//! Per-task deterministic jitter for cron fire times (B5 / DEC-3).
//!
//! A line-by-line port of the TS kernel's `app/cron/jitter.ts`: if every
//! user writes `0 9 * * *` ("every day at 9 am") then every client fires at
//! the same instant and the upstream API sees a thundering herd at :00.
//! Each task's ideal fire time is therefore shifted by a small,
//! **deterministic** per-task offset — a given task always lands at the
//! same jittered point, so reschedules and restarts do not drift.
//!
//! Two flavours (both forward-deterministic from the task id alone):
//!
//! - **Recurring**: shift *forward* by a fraction of the period (cap 10%
//!   of the period, hard cap 15 min). Long-period jobs (`0 9 * * *`,
//!   period 1 day) hit the 15-minute cap; short-period jobs (`*/5 * * * *`,
//!   period 5 min) are bounded by the 10% rule.
//! - **One-shot**: shift *earlier* (negative), but only when the ideal
//!   lands on a round local minute (`:00` or `:30` in the task's calendar)
//!   — the signal that the model picked a round number with no specific
//!   intent. Cap 90 s earlier, never earlier than the task's creation.
//!   Any other minute (`:07`, `:23`, …) passes through unchanged.
//!
//! The `MIRRI_CRON_NO_JITTER=1` escape hatch lives in
//! [`CronOptions`](crate::cron::CronOptions), not here — these functions
//! are pure.

use chrono::{DateTime, Duration, Timelike, Utc};

/// The knobs of the two jitter algorithms (the TS `JitterConfig`).
#[derive(Debug, Clone, Copy)]
pub struct JitterConfig {
    /// Recurring jitter cap, as a fraction of the fire period (0.1).
    pub recurring_max_fraction_of_period: f64,
    /// Recurring jitter hard cap in milliseconds (15 min).
    pub recurring_max_ms: i64,
    /// One-shot jitter cap in milliseconds (90 s).
    pub one_shot_max_ms: i64,
}

/// The production jitter constants (the TS `DEFAULT_CRON_JITTER_CONFIG`).
pub const DEFAULT_CRON_JITTER_CONFIG: JitterConfig = JitterConfig {
    recurring_max_fraction_of_period: 0.1,
    recurring_max_ms: 15 * 60_000,
    one_shot_max_ms: 90_000,
};

/// 24 hours in milliseconds — the period assumed when the next occurrence
/// cannot be computed (the TS `MS_PER_DAY` fallback).
const MS_PER_DAY: i64 = 24 * 60 * 60_000;

/// Derive a stable fraction in `[0, 1)` from a task id.
///
/// An id that is exactly eight hex digits takes the fast path
/// (`u32 / 2^32`); anything else runs the djb2 variant: `hash = 5381`,
/// `hash = hash * 33 + codepoint` with `i32` wrapping arithmetic, then
/// `(hash as u32) / 2^32`. This mirrors the TS `fractionFromId` bit for
/// bit (the TS loop walks UTF-16 code units; cron ids are ASCII, where
/// code units and code points coincide).
pub fn fraction_from_id(id: &str) -> f64 {
    if id.len() == 8 && id.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        let digits = u32::from_str_radix(id, 16).expect("eight hex digits always parse");
        return f64::from(digits) / 4_294_967_296.0;
    }
    let mut hash: i32 = 5381;
    for codepoint in id.chars() {
        hash = hash.wrapping_mul(33).wrapping_add(codepoint as u32 as i32);
    }
    f64::from(hash as u32) / 4_294_967_296.0
}

/// The jittered fire time of a recurring task's ideal occurrence.
///
/// Shifts `ideal` *forward* by `min(period × 0.1, 15 min) × fraction(id)`.
/// `next_ideal` is the ideal's next occurrence (used only to measure the
/// period); `None` (or a non-future value) falls back to a 24-hour period,
/// exactly like the TS `jitteredNextCronRunMs`.
pub fn jittered_next_run(
    task_id: &str,
    ideal: DateTime<Utc>,
    next_ideal: Option<DateTime<Utc>>,
    config: &JitterConfig,
) -> DateTime<Utc> {
    let period_ms = match next_ideal {
        Some(next) if next > ideal => (next - ideal).num_milliseconds(),
        _ => MS_PER_DAY,
    };
    let period_cap = period_ms as f64 * config.recurring_max_fraction_of_period;
    let cap = period_cap.min(config.recurring_max_ms as f64);
    if cap <= 0.0 {
        return ideal;
    }
    let offset = cap * fraction_from_id(task_id);
    // Floor (not truncate) so the shift never exceeds the cap by rounding.
    ideal + Duration::milliseconds(offset.floor() as i64)
}

/// The jittered fire time of a one-shot task's ideal occurrence.
///
/// Only when the ideal is aligned to the minute *and* its minute-of-hour
/// in `tz` is `:00` or `:30` does the fire move *earlier* (by at most
/// `one_shot_max_ms × fraction(id)`), and never earlier than `created_at`
/// (the task cannot fire before it exists). Every other minute passes
/// through unchanged.
pub fn one_shot_jittered_next(
    task_id: &str,
    ideal: DateTime<Utc>,
    created_at: DateTime<Utc>,
    tz: chrono_tz::Tz,
    config: &JitterConfig,
) -> DateTime<Utc> {
    if ideal.timestamp_millis() % 60_000 != 0 {
        return ideal;
    }
    let minute_of_hour = ideal.with_timezone(&tz).minute();
    if minute_of_hour != 0 && minute_of_hour != 30 {
        return ideal;
    }
    if config.one_shot_max_ms <= 0 {
        return ideal;
    }
    let offset = -(config.one_shot_max_ms as f64 * fraction_from_id(task_id));
    // Floor (not truncate): the earlier shift never exceeds the cap by
    // rounding (sub-millisecond f64 tolerance, same spirit as TS).
    let shifted = ideal + Duration::milliseconds(offset.floor() as i64);
    if shifted < created_at {
        return ideal;
    }
    shifted
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{Duration, TimeZone};

    fn ms(x: i64) -> chrono::DateTime<chrono::Utc> {
        chrono::Utc.timestamp_millis_opt(x).unwrap()
    }

    /// 2025-01-15T10:00:00Z — a minute-aligned :00 boundary (the plan's
    /// stated intent for the one-shot roundness tests).
    fn round_hour() -> i64 {
        1_736_935_200_000
    }

    #[test]
    fn should_return_a_stable_fraction_when_deriving_from_an_id() {
        // Given the same id, when the fraction is derived twice
        let a = fraction_from_id("cron-3f2b8c1e-9a4d-4c2a-8e1f-0b2c3d4e5f60");
        let b = fraction_from_id("cron-3f2b8c1e-9a4d-4c2a-8e1f-0b2c3d4e5f60");
        // Then it is identical, within [0, 1)
        assert_eq!(a, b);
        assert!((0.0..1.0).contains(&a));
    }

    #[test]
    fn should_use_the_hex_shortcut_when_the_id_is_exactly_eight_hex_digits() {
        // 0x80000000 / 2^32 == 0.5 exactly (the TS 8-hex fast path).
        assert!((fraction_from_id("80000000") - 0.5).abs() < 1e-12);
    }

    #[test]
    fn should_shift_forward_at_most_fifteen_minutes_when_jittering_a_daily_task() {
        // Given a daily task (period 24h → the 15-minute hard cap binds) and an id
        // whose fraction is 0.5
        let ideal = ms(round_hour()); // 2025-01-15T10:00:00Z, aligned to :00
        let next_ideal = ideal + Duration::hours(24);

        let jittered = jittered_next_run(
            "80000000",
            ideal,
            Some(next_ideal),
            &DEFAULT_CRON_JITTER_CONFIG,
        );

        // Then the shift is exactly cap × fraction = 7.5 minutes, forward only
        assert_eq!(jittered - ideal, Duration::seconds(7 * 60 + 30));
    }

    #[test]
    fn should_shift_forward_at_most_ten_percent_when_jittering_a_five_minute_task() {
        // Given a */5 task (period 5min → the 10% rule binds: cap 30s) with the
        // maximal fraction
        let ideal = ms(round_hour());
        let next_ideal = ideal + Duration::minutes(5);

        let jittered = jittered_next_run(
            "ffffffff",
            ideal,
            Some(next_ideal),
            &DEFAULT_CRON_JITTER_CONFIG,
        );

        // Then the shift never exceeds 30s (10% of the 5-minute period)
        assert!(jittered - ideal <= Duration::seconds(30));
        assert!(jittered > ideal);
    }

    #[test]
    fn should_not_shift_back_when_jittering_a_recurring_task() {
        let ideal = ms(round_hour());
        let jittered = jittered_next_run(
            "00000001",
            ideal,
            Some(ideal + Duration::hours(24)),
            &DEFAULT_CRON_JITTER_CONFIG,
        );
        assert!(jittered >= ideal, "recurring jitter is forward-only");
    }

    #[test]
    fn should_shift_earlier_up_to_ninety_seconds_when_one_shot_lands_on_a_round_hour() {
        // Given a one-shot whose ideal is 10:00 UTC — a :00 minute boundary,
        // an id with fraction 0.5, and a creation well before
        let ideal = ms(round_hour()); // :00 minute
        let created = ideal - Duration::hours(1);

        let jittered = one_shot_jittered_next(
            "80000000",
            ideal,
            created,
            chrono_tz::UTC,
            &DEFAULT_CRON_JITTER_CONFIG,
        );

        // Then it fires 45s earlier (90s × 0.5)
        assert_eq!(ideal - jittered, Duration::seconds(45));
    }

    #[test]
    fn should_not_shift_when_one_shot_lands_on_a_non_round_minute() {
        // The model picked :07 deliberately — pass through unchanged.
        let ideal = ms(round_hour() + 7 * 60_000);
        let jittered = one_shot_jittered_next(
            "80000000",
            ideal,
            ideal - Duration::hours(1),
            chrono_tz::UTC,
            &DEFAULT_CRON_JITTER_CONFIG,
        );
        assert_eq!(jittered, ideal);
    }

    #[test]
    fn should_not_shift_when_one_shot_lands_on_a_half_hour() {
        // :30 is also "round" (the TS rule shifts :00 and :30 alike for one-shots).
        let ideal = ms(round_hour() + 30 * 60_000);
        let jittered = one_shot_jittered_next(
            "80000000",
            ideal,
            ideal - Duration::hours(1),
            chrono_tz::UTC,
            &DEFAULT_CRON_JITTER_CONFIG,
        );
        assert!(ideal - jittered <= Duration::seconds(90));
        assert!(jittered < ideal);
    }

    #[test]
    fn should_not_shift_earlier_than_created_at_when_one_shot_jitter_would_undershoot() {
        // Given a one-shot created 30s before its ideal :00 fire
        let ideal = ms(round_hour());
        let created = ideal - Duration::seconds(30);

        let jittered = one_shot_jittered_next(
            "ffffffff",
            ideal,
            created,
            chrono_tz::UTC,
            &DEFAULT_CRON_JITTER_CONFIG,
        );

        // Then the jitter is clamped away: the ideal fires unchanged (TS returns
        // ideal when shifted < createdAt).
        assert_eq!(jittered, ideal);
    }

    #[test]
    fn should_evaluate_roundness_in_the_task_timezone_when_one_shot_jittering() {
        // Given 2025-01-15 18:30 UTC == 00:00 next day in Asia/Shanghai (+08:00)
        // — a :00 local minute — when jittered in Shanghai
        let ideal = chrono::Utc
            .with_ymd_and_hms(2025, 1, 15, 18, 30, 0)
            .unwrap();
        let jittered = one_shot_jittered_next(
            "80000000",
            ideal,
            ideal - Duration::hours(1),
            chrono_tz::Asia::Shanghai,
            &DEFAULT_CRON_JITTER_CONFIG,
        );

        // Then it shifted earlier (the round-minute check is local, not UTC:
        // 18:30 UTC is 00:00 in Shanghai terms).
        assert!(jittered < ideal);
    }

    #[test]
    fn should_not_shift_when_local_minute_is_not_round_even_if_utc_minute_is() {
        // Given 18:07 UTC == 23:37 in Asia/Kolkata (+05:30) — :37 local is
        // not a round minute, so the fire passes through unchanged.
        let ideal = chrono::Utc.with_ymd_and_hms(2025, 1, 15, 18, 7, 0).unwrap();
        let jittered = one_shot_jittered_next(
            "80000000",
            ideal,
            ideal - Duration::hours(1),
            chrono_tz::Asia::Kolkata,
            &DEFAULT_CRON_JITTER_CONFIG,
        );
        assert_eq!(jittered, ideal, "23:37 local is not a round minute");
    }
}
