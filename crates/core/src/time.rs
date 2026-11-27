//! Calendar and FILETIME arithmetic shared by every crate.
//!
//! The day-number functions are `const` integer math on purpose: sorting into date sections calls
//! them for every item, and a validating date type would make that slower. `jiff` (a dependency
//! anyway) is the reference in the tests. Everything off the hot path uses `jiff` directly.

use std::time::{Duration, SystemTime};

/// 100 ns ticks from 1601-01-01 (the FILETIME epoch) to 1970-01-01.
pub const UNIX_EPOCH_TICKS: u64 = 116_444_736_000_000_000;

/// Seconds from 1601-01-01 to 1970-01-01.
pub const UNIX_EPOCH_SECS: u64 = UNIX_EPOCH_TICKS / TICKS_PER_SEC;

/// FILETIME ticks in a second.
const TICKS_PER_SEC: u64 = 10_000_000;

/// Civil date to days since 1970-01-01 (proleptic Gregorian).
#[must_use]
pub const fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// Days since 1970-01-01 to the civil date `(year, month, day)`.
#[must_use]
pub const fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + if m <= 2 { 1 } else { 0 };
    (y, m, d)
}

/// A FILETIME (100 ns ticks since 1601, UTC) as a `SystemTime`, the way std converts it, so the
/// result compares equal to `Metadata::modified()` of the same entry. `None` when out of range.
#[must_use]
pub fn system_time_from_filetime(ticks: u64) -> Option<SystemTime> {
    let to_duration = |n: u64| {
        Duration::new(
            n / TICKS_PER_SEC,
            u32::try_from((n % TICKS_PER_SEC) * 100).unwrap_or(0),
        )
    };
    if ticks >= UNIX_EPOCH_TICKS {
        SystemTime::UNIX_EPOCH.checked_add(to_duration(ticks - UNIX_EPOCH_TICKS))
    } else {
        SystemTime::UNIX_EPOCH.checked_sub(to_duration(UNIX_EPOCH_TICKS - ticks))
    }
}

/// A `SystemTime` as FILETIME ticks (100 ns since 1601, UTC). `None` before 1601 or past the
/// range of 64 bits.
#[must_use]
pub fn filetime_from_system_time(time: SystemTime) -> Option<u64> {
    let ticks = match time.duration_since(SystemTime::UNIX_EPOCH) {
        Ok(after) => UNIX_EPOCH_TICKS.checked_add(u64::try_from(after.as_nanos() / 100).ok()?)?,
        Err(before) => {
            UNIX_EPOCH_TICKS.checked_sub(u64::try_from(before.duration().as_nanos() / 100).ok()?)?
        }
    };
    Some(ticks)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The integer math agrees with jiff over several centuries, around leap days included.
    #[test]
    fn day_numbers_agree_with_jiff() {
        let mut date = jiff::civil::date(1600, 1, 1);
        let last = jiff::civil::date(2400, 12, 31);
        let epoch = jiff::civil::date(1970, 1, 1);
        while date <= last {
            let days = i64::from(date.since(epoch).map_or(0, |s| s.get_days()));
            let (y, m, d) = (
                i64::from(date.year()),
                i64::from(date.month()),
                i64::from(date.day()),
            );
            assert_eq!(days_from_civil(y, m, d), days, "{date}");
            assert_eq!(civil_from_days(days), (y, m, d), "{date}");
            date = date.tomorrow().unwrap();
        }
    }

    #[test]
    fn the_epoch_is_day_zero() {
        assert_eq!(days_from_civil(1970, 1, 1), 0);
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(-1), (1969, 12, 31));
    }

    #[test]
    fn file_times_round_trip() {
        let t = SystemTime::UNIX_EPOCH + Duration::new(1_700_000_000, 123_456_700);
        let ticks = filetime_from_system_time(t).unwrap();
        assert_eq!(
            ticks,
            UNIX_EPOCH_TICKS + 1_700_000_000 * TICKS_PER_SEC + 1_234_567
        );
        assert_eq!(system_time_from_filetime(ticks), Some(t));
        assert_eq!(
            system_time_from_filetime(UNIX_EPOCH_TICKS),
            Some(SystemTime::UNIX_EPOCH)
        );
        // Before 1970 still converts, like std.
        let before = system_time_from_filetime(UNIX_EPOCH_TICKS - TICKS_PER_SEC).unwrap();
        assert_eq!(before, SystemTime::UNIX_EPOCH - Duration::from_secs(1));
        assert_eq!(
            filetime_from_system_time(before),
            Some(UNIX_EPOCH_TICKS - TICKS_PER_SEC)
        );
    }
}
