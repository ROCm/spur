// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Slurm's time grammar for `--begin` and `--deadline`.
//!
//! Slurm resolves a specification carrying no timezone against the submitting
//! host's local zone, so callers hand in the current local time and every naked
//! form is read in its offset. Taking `now` as a parameter also keeps the
//! grammar testable without depending on the runner's clock or zone.
//!
//! Accepted forms, per sbatch(1):
//!
//! - `now`, and `now+<count>[unit]` / `now-<count>[unit]`, where unit is
//!   seconds (the default when omitted), minutes, hours, days, or weeks
//! - `HH:MM[:SS]`, optionally suffixed `AM` or `PM`
//! - the named times `midnight`, `elevenses`, `noon`, `fika`, and `teatime`
//! - `today` and `tomorrow`, meaning that date at 00:00
//! - `MMDDYY`, `MM/DD/YY`, `MM/DD/YYYY`, and `YYYY-MM-DD`
//! - `YYYY-MM-DD[THH:MM[:SS]]`
//! - anything carrying an explicit offset (`…Z`, `…+05:30`), taken verbatim
//!
//! A time-of-day that has already passed moves to the next day, as sbatch(1)
//! specifies. Forms that name a date do not move, so `today` cannot silently
//! become tomorrow, and a date in the past stays in the past — Slurm accepts
//! those and starts the job immediately.

use chrono::{DateTime, Duration, NaiveDate, NaiveDateTime, NaiveTime, TimeZone, Utc};

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TimeSpecError {
    #[error("empty time specification")]
    Empty,
    #[error(
        "invalid time offset '{0}': expected a count optionally followed by \
         seconds, minutes, hours, days, or weeks"
    )]
    Offset(String),
    #[error(
        "invalid time specification '{0}': expected one of now[+|-]<count>[unit], \
         HH:MM[:SS][AM|PM], midnight, noon, fika, teatime, today, tomorrow, \
         MMDDYY, MM/DD/YY, or YYYY-MM-DD[THH:MM[:SS]]"
    )]
    Invalid(String),
    #[error("time '{0}' does not exist in the local timezone")]
    Skipped(String),
}

/// A local wall-clock instant, plus whether it should move to the next day when
/// it has already passed. Only the time-of-day forms move.
struct LocalSpec {
    naive: NaiveDateTime,
    roll_if_past: bool,
}

enum Meridiem {
    Am,
    Pm,
}

/// Parse a Slurm time specification into an absolute instant.
///
/// `now` supplies both the reference instant and the zone that naked forms are
/// read in; pass `chrono::Local::now()` to match Slurm.
pub fn parse_time_spec<Tz: TimeZone>(
    input: &str,
    now: &DateTime<Tz>,
) -> Result<DateTime<Utc>, TimeSpecError> {
    let spec = input.trim();
    if spec.is_empty() {
        return Err(TimeSpecError::Empty);
    }

    if spec.eq_ignore_ascii_case("now") {
        return Ok(now.with_timezone(&Utc));
    }

    if let Some(offset) = strip_now(spec) {
        return offset_from_now(offset, now);
    }

    // An explicit offset is unambiguous, so it wins over any local reading and
    // keeps every input that already parsed working unchanged.
    if let Ok(absolute) = DateTime::parse_from_rfc3339(spec) {
        return Ok(absolute.with_timezone(&Utc));
    }

    resolve(parse_local(spec, now)?, now, spec)
}

/// The signed offset body of a `now+…` / `now-…` spec, or `None` when `spec`
/// is not a `now` offset at all.
fn strip_now(spec: &str) -> Option<&str> {
    let (head, rest) = spec.split_at_checked(3)?;
    if !head.eq_ignore_ascii_case("now") {
        return None;
    }
    let rest = rest.trim_start();
    (rest.starts_with('+') || rest.starts_with('-')).then_some(rest)
}

fn offset_from_now<Tz: TimeZone>(
    offset: &str,
    now: &DateTime<Tz>,
) -> Result<DateTime<Utc>, TimeSpecError> {
    let (sign, body) = offset.split_at(1);
    let sign: i64 = if sign == "-" { -1 } else { 1 };
    let body = body.trim();

    let split = body
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(body.len());
    let (count, unit) = body.split_at(split);
    let count: i64 = count
        .parse()
        .map_err(|_| TimeSpecError::Offset(body.to_string()))?;

    // sbatch(1): "seconds (default), minutes, hours, days, or weeks".
    let unit_seconds: i64 = match unit.trim().to_ascii_lowercase().as_str() {
        "" | "second" | "seconds" => 1,
        "minute" | "minutes" => 60,
        "hour" | "hours" => 3_600,
        "day" | "days" => 86_400,
        "week" | "weeks" => 604_800,
        _ => return Err(TimeSpecError::Offset(body.to_string())),
    };

    let overflow = || TimeSpecError::Offset(body.to_string());
    let seconds = count
        .checked_mul(unit_seconds)
        .and_then(|s| s.checked_mul(sign))
        .ok_or_else(overflow)?;
    now.clone()
        .checked_add_signed(Duration::try_seconds(seconds).ok_or_else(overflow)?)
        .map(|instant| instant.with_timezone(&Utc))
        .ok_or_else(overflow)
}

fn parse_local<Tz: TimeZone>(spec: &str, now: &DateTime<Tz>) -> Result<LocalSpec, TimeSpecError> {
    let today = now.date_naive();

    if let Some(time) = named_time(spec) {
        return Ok(LocalSpec {
            naive: today.and_time(time),
            roll_if_past: true,
        });
    }

    if spec.eq_ignore_ascii_case("today") {
        return Ok(LocalSpec {
            naive: today.and_time(NaiveTime::MIN),
            roll_if_past: false,
        });
    }

    if spec.eq_ignore_ascii_case("tomorrow") {
        let date = today
            .succ_opt()
            .ok_or_else(|| TimeSpecError::Invalid(spec.to_string()))?;
        return Ok(LocalSpec {
            naive: date.and_time(NaiveTime::MIN),
            roll_if_past: false,
        });
    }

    if let Some(naive) = parse_datetime(spec) {
        return Ok(LocalSpec {
            naive,
            roll_if_past: false,
        });
    }

    if let Some(date) = parse_date(spec) {
        return Ok(LocalSpec {
            naive: date.and_time(NaiveTime::MIN),
            roll_if_past: false,
        });
    }

    if let Some(time) = parse_time_of_day(spec) {
        return Ok(LocalSpec {
            naive: today.and_time(time),
            roll_if_past: true,
        });
    }

    Err(TimeSpecError::Invalid(spec.to_string()))
}

fn resolve<Tz: TimeZone>(
    spec: LocalSpec,
    now: &DateTime<Tz>,
    input: &str,
) -> Result<DateTime<Utc>, TimeSpecError> {
    let zone = now.timezone();
    let mut naive = spec.naive;

    if spec.roll_if_past && zoned(&zone, naive, input)? < *now {
        // Advance the calendar day and re-resolve rather than adding 24 hours,
        // so a daylight-saving shift cannot drag the wall-clock time with it.
        naive += Duration::days(1);
    }

    Ok(zoned(&zone, naive, input)?.with_timezone(&Utc))
}

fn zoned<Tz: TimeZone>(
    zone: &Tz,
    naive: NaiveDateTime,
    input: &str,
) -> Result<DateTime<Tz>, TimeSpecError> {
    // Ambiguous wall-clock times (a daylight-saving fall-back) resolve to the
    // earlier instant; times inside a spring-forward gap do not exist at all.
    zone.from_local_datetime(&naive)
        .earliest()
        .ok_or_else(|| TimeSpecError::Skipped(input.to_string()))
}

/// sbatch(1) documents fika as 3 PM and teatime as 4 PM.
fn named_time(spec: &str) -> Option<NaiveTime> {
    let (hour, minute) = match spec.to_ascii_lowercase().as_str() {
        "midnight" => (0, 0),
        "elevenses" => (11, 0),
        "noon" => (12, 0),
        "fika" => (15, 0),
        "teatime" => (16, 0),
        _ => return None,
    };
    NaiveTime::from_hms_opt(hour, minute, 0)
}

/// `YYYY-MM-DD[THH:MM[:SS]]`, the one combined form sbatch(1) documents.
fn parse_datetime(spec: &str) -> Option<NaiveDateTime> {
    ["%Y-%m-%dT%H:%M:%S", "%Y-%m-%dT%H:%M"]
        .iter()
        .find_map(|fmt| NaiveDateTime::parse_from_str(spec, fmt).ok())
}

fn parse_date(spec: &str) -> Option<NaiveDate> {
    if let Ok(date) = NaiveDate::parse_from_str(spec, "%Y-%m-%d") {
        return Some(date);
    }
    parse_us_date(spec)
}

/// `MM/DD/YY`, `MM/DD/YYYY`, and `MMDDYY`.
///
/// The fields are split by hand rather than handed to chrono, whose `%Y` accepts
/// a short year (reading `08/15/26` as year 26) and whose two-digit `%y` rule
/// does not apply without separators (reading `081526` as year 26 as well). A
/// two-digit year expands to `20YY`, since a scheduler defers into the future
/// and the 1900s are never the intended reading.
fn parse_us_date(spec: &str) -> Option<NaiveDate> {
    let (month, day, year) = if let Some((month, rest)) = spec.split_once('/') {
        let (day, year) = rest.split_once('/')?;
        (month, day, year)
    } else if spec.len() == 6 && spec.bytes().all(|b| b.is_ascii_digit()) {
        (&spec[0..2], &spec[2..4], &spec[4..6])
    } else {
        return None;
    };

    let field = |s: &str, max: usize| -> Option<u32> {
        (!s.is_empty() && s.len() <= max && s.bytes().all(|b| b.is_ascii_digit()))
            .then(|| s.parse().ok())
            .flatten()
    };

    let year = match year.len() {
        2 => 2_000 + field(year, 2)? as i32,
        4 => field(year, 4)? as i32,
        _ => return None,
    };
    NaiveDate::from_ymd_opt(year, field(month, 2)?, field(day, 2)?)
}

/// `HH:MM[:SS]` with an optional `AM`/`PM` suffix. A meridiem also allows the
/// bare-hour form (`4PM`); without one a colon is required, which keeps a plain
/// run of digits available for the `MMDDYY` date form.
fn parse_time_of_day(spec: &str) -> Option<NaiveTime> {
    let lower = spec.to_ascii_lowercase();
    let (body, meridiem) = match () {
        _ if lower.ends_with("am") => (lower[..lower.len() - 2].trim_end(), Some(Meridiem::Am)),
        _ if lower.ends_with("pm") => (lower[..lower.len() - 2].trim_end(), Some(Meridiem::Pm)),
        _ => (lower.as_str(), None),
    };

    let mut fields = body.split(':');
    let hour: u32 = fields.next()?.trim().parse().ok()?;
    let minute: u32 = match fields.next() {
        Some(field) => field.trim().parse().ok()?,
        // A lone number is only a time when a meridiem says so.
        None if meridiem.is_some() => 0,
        None => return None,
    };
    let second: u32 = match fields.next() {
        Some(field) => field.trim().parse().ok()?,
        None => 0,
    };
    if fields.next().is_some() {
        return None;
    }

    let hour = match meridiem {
        None => hour,
        Some(_) if !(1..=12).contains(&hour) => return None,
        Some(Meridiem::Am) => hour % 12,
        Some(Meridiem::Pm) => hour % 12 + 12,
    };
    NaiveTime::from_hms_opt(hour, minute, second)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::FixedOffset;

    /// A fixed reference so every expectation is exact: 2026-07-30 09:15:00 in a
    /// zone two hours ahead of UTC. Nothing here reads the runner's clock or TZ.
    fn now() -> DateTime<FixedOffset> {
        FixedOffset::east_opt(2 * 3_600)
            .unwrap()
            .with_ymd_and_hms(2026, 7, 30, 9, 15, 0)
            .unwrap()
    }

    /// Parse against the fixed reference and render as UTC for comparison.
    fn parse(spec: &str) -> String {
        parse_time_spec(spec, &now())
            .unwrap_or_else(|e| panic!("{spec:?} must parse: {e}"))
            .format("%Y-%m-%dT%H:%M:%SZ")
            .to_string()
    }

    #[test]
    fn now_and_offsets_from_now() {
        assert_eq!(parse("now"), "2026-07-30T07:15:00Z");
        // A bare count is seconds, which sbatch(1) calls out as the default and
        // is the reading a migrating script depends on.
        assert_eq!(parse("now+45"), "2026-07-30T07:15:45Z");
        assert_eq!(parse("now+60"), "2026-07-30T07:16:00Z");
        assert_eq!(parse("now+30seconds"), "2026-07-30T07:15:30Z");
        assert_eq!(parse("now+30minutes"), "2026-07-30T07:45:00Z");
        assert_eq!(parse("now+1hour"), "2026-07-30T08:15:00Z");
        assert_eq!(parse("now+2days"), "2026-08-01T07:15:00Z");
        assert_eq!(parse("now+1week"), "2026-08-06T07:15:00Z");
        assert_eq!(parse("now+2weeks"), "2026-08-13T07:15:00Z");
        assert_eq!(parse("now-1hour"), "2026-07-30T06:15:00Z");
    }

    #[test]
    fn naked_forms_resolve_in_the_reference_zone() {
        // 16:00 two hours east of UTC is 14:00Z, not 16:00Z. Reading these as
        // UTC would schedule every migrating job at the wrong moment.
        assert_eq!(parse("16:00"), "2026-07-30T14:00:00Z");
        assert_eq!(parse("2010-01-20T12:34:00"), "2010-01-20T10:34:00Z");
        assert_eq!(parse("tomorrow"), "2026-07-30T22:00:00Z");
    }

    #[test]
    fn a_past_time_of_day_moves_to_the_next_day() {
        // 08:00 local has already gone at 09:15; 16:00 has not.
        assert_eq!(parse("08:00"), "2026-07-31T06:00:00Z");
        assert_eq!(parse("16:00"), "2026-07-30T14:00:00Z");
        // Today's midnight has long gone, so this lands on the 31st at 00:00
        // local — the same instant `tomorrow` names.
        assert_eq!(parse("midnight"), "2026-07-30T22:00:00Z");
        assert_eq!(parse("midnight"), parse("tomorrow"));
    }

    #[test]
    fn a_date_in_the_past_stays_in_the_past() {
        // Slurm accepts these and starts the job immediately, so rolling them
        // forward would defer a job the user expects to run now.
        assert_eq!(parse("2010-01-20T12:34:00"), "2010-01-20T10:34:00Z");
        assert_eq!(parse("2010-01-20"), "2010-01-19T22:00:00Z");
        // today names a date, so it must not become tomorrow.
        assert_eq!(parse("today"), "2026-07-29T22:00:00Z");
    }

    #[test]
    fn named_times() {
        assert_eq!(parse("elevenses"), "2026-07-30T09:00:00Z");
        assert_eq!(parse("noon"), "2026-07-30T10:00:00Z");
        assert_eq!(parse("fika"), "2026-07-30T13:00:00Z");
        assert_eq!(parse("teatime"), "2026-07-30T14:00:00Z");
        assert_eq!(parse("TeaTime"), "2026-07-30T14:00:00Z");
    }

    #[test]
    fn time_of_day_shapes() {
        assert_eq!(parse("16:00:30"), "2026-07-30T14:00:30Z");
        assert_eq!(parse("4PM"), "2026-07-30T14:00:00Z");
        assert_eq!(parse("4:30 pm"), "2026-07-30T14:30:00Z");
        assert_eq!(parse("11:00am"), "2026-07-30T09:00:00Z");
        // 12 AM is midnight and 12 PM is noon, not hour 12 and hour 24.
        assert_eq!(parse("12am"), parse("midnight"));
        assert_eq!(parse("12pm"), "2026-07-30T10:00:00Z");
    }

    #[test]
    fn date_shapes() {
        assert_eq!(parse("2026-08-15"), "2026-08-14T22:00:00Z");
        assert_eq!(parse("08/15/2026"), "2026-08-14T22:00:00Z");
        assert_eq!(parse("08/15/26"), "2026-08-14T22:00:00Z");
        assert_eq!(parse("081526"), "2026-08-14T22:00:00Z");
        assert_eq!(parse("2026-08-15T06:30"), "2026-08-15T04:30:00Z");
    }

    #[test]
    fn an_explicit_offset_is_taken_verbatim() {
        // Previously the only accepted absolute form; it must keep working and
        // must not be re-read in the local zone.
        assert_eq!(parse("2010-01-20T12:34:00Z"), "2010-01-20T12:34:00Z");
        assert_eq!(parse("2010-01-20T12:34:00+05:30"), "2010-01-20T07:04:00Z");
    }

    #[test]
    fn rejects_what_is_not_a_time() {
        for spec in [
            "",
            "   ",
            "yesterday",
            "now+1fortnight",
            "25:00",
            "13pm",
            "16",
        ] {
            assert!(
                parse_time_spec(spec, &now()).is_err(),
                "{spec:?} must not parse"
            );
        }
    }

    #[test]
    fn error_messages_name_the_offending_input() {
        let err = parse_time_spec("now+1fortnight", &now()).unwrap_err();
        assert!(err.to_string().contains("1fortnight"), "{err}");
        let err = parse_time_spec("yesterday", &now()).unwrap_err();
        assert!(err.to_string().contains("yesterday"), "{err}");
    }
}
