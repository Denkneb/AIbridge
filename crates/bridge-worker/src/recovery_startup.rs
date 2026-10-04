//! Config-independent probes and the durable spawn-before-lock grace window.
use crate::{WorkerLock, WorkerLockError, WorkerLockOutcome};
use bridge_domain::TaskId;
use bridge_storage::RustStateLayout;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

// Probes briefly acquire flock on separate descriptors. Serialize them to avoid
// observing each other's transient acquisitions as a live worker.
use crate::lock::PROBE_LOCK;
/// A task is running when either its task fence or project fence is held.
/// The live parallel setting is deliberately irrelevant.
/// # Errors
/// State ownership/artifact/OS failures never report an idle task.
pub fn task_worker_running(
    layout: &RustStateLayout,
    task: TaskId,
) -> Result<bool, WorkerLockError> {
    let _probe = PROBE_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    match WorkerLock::try_acquire_task(layout, task)? {
        WorkerLockOutcome::Busy => return Ok(true),
        WorkerLockOutcome::Acquired(guard) => drop(guard),
    }
    match WorkerLock::try_acquire(layout)? {
        WorkerLockOutcome::Busy => Ok(true),
        WorkerLockOutcome::Acquired(guard) => {
            drop(guard);
            Ok(false)
        }
    }
}
/// Mirrors the reference persisted lease check. Expiry is strict `< grace`;
/// missing/malformed timestamps have no lease, future leases remain pending.
/// Valid UTC, naive-UTC and numeric-offset ISO timestamps are supported.
#[must_use]
pub fn spawn_lease_pending(started: Option<&str>, now: SystemTime, grace: Duration) -> bool {
    let Some(started) = started.and_then(parse_timestamp) else {
        return false;
    };
    match now.duration_since(started) {
        Ok(age) => age < grace,
        Err(_) => true,
    }
}
fn number(text: &str) -> Option<i64> {
    if text.is_empty() || !text.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    text.parse().ok()
}
fn parse_timestamp(text: &str) -> Option<SystemTime> {
    let (date, clock) = text.split_once('T')?;
    let date = date.split('-').collect::<Vec<_>>();
    if date.len() != 3 || date[0].len() != 4 || date[1].len() != 2 || date[2].len() != 2 {
        return None;
    }
    let year = number(date[0])?;
    let month = number(date[1])?;
    let day = number(date[2])?;
    if !(1..=9999).contains(&year) || !(1..=12).contains(&month) {
        return None;
    }
    let leap = year % 4 == 0 && (year % 100 != 0 || year % 400 == 0);
    let days = match month {
        2 => {
            if leap {
                29
            } else {
                28
            }
        }
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    };
    if !(1..=days).contains(&day) {
        return None;
    }
    let (clock, offset) = if let Some(clock) = clock.strip_suffix('Z') {
        (clock, 0)
    } else if let Some(pos) = clock.find(['+', '-']) {
        let (clock, tz) = clock.split_at(pos);
        let sign = if tz.starts_with('+') { 1 } else { -1 };
        if tz.len() != 6 || tz.as_bytes()[3] != b':' {
            return None;
        }
        let h = number(tz.get(1..3)?)?;
        let m = number(tz.get(4..6)?)?;
        if h > 23 || m > 59 {
            return None;
        }
        (clock, sign * (h * 3600 + m * 60))
    } else {
        (clock, 0)
    };
    let (clock, nanos) = if let Some((clock, fraction)) = clock.split_once('.') {
        if fraction.is_empty() || fraction.len() > 9 {
            return None;
        }
        let nanos = number(fraction)? * 10_i64.pow(9 - u32::try_from(fraction.len()).ok()?);
        (clock, u32::try_from(nanos).ok()?)
    } else {
        (clock, 0)
    };
    let clock = clock.split(':').collect::<Vec<_>>();
    if clock.len() != 3 || clock.iter().any(|n| n.len() != 2) {
        return None;
    }
    let hour = number(clock[0])?;
    let minute = number(clock[1])?;
    let second = number(clock[2])?;
    if hour > 23 || minute > 59 || second > 59 {
        return None;
    }
    // Gregorian civil date -> epoch days; inverse of the storage formatter.
    let year = year - i64::from(month <= 2);
    let era = year.div_euclid(400);
    let yoe = year - era * 400;
    let mp = month + if month > 2 { -3 } else { 9 };
    let doy = (153 * mp + 2) / 5 + day - 1;
    let epoch_days = era * 146097 + yoe * 365 + yoe / 4 - yoe / 100 + doy - 719468;
    let seconds = epoch_days * 86400 + hour * 3600 + minute * 60 + second - offset;
    if seconds >= 0 {
        UNIX_EPOCH.checked_add(Duration::new(u64::try_from(seconds).ok()?, nanos))
    } else {
        UNIX_EPOCH
            .checked_sub(Duration::from_secs(seconds.unsigned_abs()))?
            .checked_add(Duration::from_nanos(u64::from(nanos)))
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn durable_lease_has_strict_expiry_future_clock_and_malformed_defaults() {
        let now = UNIX_EPOCH + Duration::from_secs(10);
        let grace = Duration::from_secs(10);
        assert!(spawn_lease_pending(
            Some("1970-01-01T00:00:00.001+00:00"),
            now,
            grace
        ));
        assert!(!spawn_lease_pending(
            Some("1970-01-01T00:00:00.000+00:00"),
            now,
            grace
        ));
        assert!(spawn_lease_pending(
            Some("1970-01-01T00:01:00Z"),
            now,
            grace
        ));
        assert!(!spawn_lease_pending(None, now, grace));
        for raw in [
            "",
            "garbage",
            "1970-02-30T00:00:00Z",
            "1900-02-29T00:00:00Z",
            "1970-01-01T24:00:00Z",
            "1970-01-01T00:00:00+99:00",
            "1970-01-01T00:00:00.secretZ",
            "1970-01-01T00:00:00.1234567890Z",
        ] {
            assert!(!spawn_lease_pending(Some(raw), now, grace), "{raw}");
        }
    }
    #[test]
    fn timestamp_goldens_cover_leap_centuries_offsets_fraction_and_naive_utc() {
        for (raw, seconds, nanos) in [
            ("1970-01-01T00:00:00", 0, 0),
            ("1970-01-01T07:00:00+07:00", 0, 0),
            ("1969-12-31T19:00:00-05:00", 0, 0),
            ("2000-02-29T12:34:56.123456789Z", 951827696, 123456789),
            ("2026-10-04T00:00:00.123+00:00", 1791072000, 123000000),
        ] {
            assert_eq!(
                parse_timestamp(raw),
                Some(UNIX_EPOCH + Duration::new(seconds, nanos)),
                "{raw}"
            );
        }
        assert!(parse_timestamp("1900-02-29T00:00:00Z").is_none());
        assert!(parse_timestamp("2000-02-29T00:00:00Z").is_some());
    }
}
