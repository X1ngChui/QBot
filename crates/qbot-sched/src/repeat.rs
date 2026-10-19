//! Repeat rules of group tasks: a fixed interval, every day at a local time, or chosen weekdays
//! at a local time. A repeating task is one timer whose due time the scheduler moves to the next
//! occurrence after each run, so nothing depends on the model scheduling it again.
//!
//! Occurrences are computed from the rule, never from when a run finished, so late runs do not
//! shift the schedule. An interval keeps the phase of the occurrence it follows. A daily or weekly
//! time is a wall-clock time in the task's zone: on a day when that time does not exist (the clock
//! jumps forward over it) the occurrence is moved forward by the length of the jump, and on a day
//! when it exists twice (the clock goes back) only the earlier one is used.

use std::fmt;
use std::str::FromStr;
use std::time::Duration;

use jiff::Timestamp;
use jiff::civil::{Date, Weekday};
use jiff::tz::TimeZone;
use qbot_core::UnixMillis;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RepeatError {
    #[error("`{0}` is not a known time zone")]
    BadZone(String),
    #[error("`{0}` is not a repeat rule (6h, daily@08:00, mon,thu@08:00)")]
    BadRule(String),
}

/// A time of day to the minute.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TimeOfDay(u16);

impl TimeOfDay {
    pub fn new(hour: u8, minute: u8) -> Option<Self> {
        (hour < 24 && minute < 60).then(|| Self(u16::from(hour) * 60 + u16::from(minute)))
    }

    /// Minutes since midnight, `0..1440`.
    pub fn from_minute_of_day(minute: u16) -> Option<Self> {
        (minute < 24 * 60).then_some(Self(minute))
    }

    pub fn minute_of_day(self) -> u16 {
        self.0
    }

    fn hour(self) -> i8 {
        i8::try_from(self.0 / 60).unwrap_or(0)
    }

    fn minute(self) -> i8 {
        i8::try_from(self.0 % 60).unwrap_or(0)
    }
}

impl FromStr for TimeOfDay {
    type Err = ();

    /// `HH:MM`, 24-hour.
    fn from_str(text: &str) -> Result<Self, ()> {
        let (hour, minute) = text.split_once(':').ok_or(())?;
        if hour.is_empty() || hour.len() > 2 || minute.len() != 2 {
            return Err(());
        }
        Self::new(
            hour.parse().map_err(|_| ())?,
            minute.parse().map_err(|_| ())?,
        )
        .ok_or(())
    }
}

impl fmt::Display for TimeOfDay {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:02}:{:02}", self.0 / 60, self.0 % 60)
    }
}

/// The codes of the weekdays, Monday first; they name days in rules and tool arguments.
pub const WEEKDAY_CODES: [&str; 7] = ["mon", "tue", "wed", "thu", "fri", "sat", "sun"];

/// A non-empty set of weekdays: bit 0 is Monday, bit 6 Sunday.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Weekdays(u8);

impl Weekdays {
    pub fn from_bits(bits: u8) -> Option<Self> {
        (bits != 0 && bits < 1 << 7).then_some(Self(bits))
    }

    /// Days by code (`mon` .. `sun`); `None` for an unknown code or no days.
    pub fn from_codes<'a>(codes: impl IntoIterator<Item = &'a str>) -> Option<Self> {
        let mut bits = 0u8;
        for code in codes {
            let index = WEEKDAY_CODES.iter().position(|c| *c == code)?;
            bits |= 1 << index;
        }
        Self::from_bits(bits)
    }

    pub fn bits(self) -> u8 {
        self.0
    }

    /// The codes of the days in the set, Monday first.
    pub fn codes(self) -> impl Iterator<Item = &'static str> {
        WEEKDAY_CODES
            .iter()
            .enumerate()
            .filter(move |(i, _)| self.0 & (1 << i) != 0)
            .map(|(_, code)| *code)
    }

    fn contains(self, day: Weekday) -> bool {
        let index = u8::try_from(day.to_monday_zero_offset()).unwrap_or(0);
        self.0 & (1 << index) != 0
    }
}

/// How a repeating task recurs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Rule {
    /// A fixed period of whole minutes, the same length in every zone and season.
    Interval(Duration),
    Daily(TimeOfDay),
    Weekly(Weekdays, TimeOfDay),
}

impl Rule {
    /// An interval of whole minutes, at least one.
    pub fn interval(period: Duration) -> Option<Self> {
        (period.as_secs() >= 60 && period.as_millis().is_multiple_of(60_000))
            .then_some(Self::Interval(period))
    }
}

/// The written form of a rule, as owners type it: an interval (`90m`, `6h`, `3d`), `daily@HH:MM`,
/// or days and a time (`mon,thu@HH:MM`).
impl FromStr for Rule {
    type Err = RepeatError;

    fn from_str(text: &str) -> Result<Self, RepeatError> {
        let bad = || RepeatError::BadRule(text.to_owned());
        if let Some((days, time)) = text.split_once('@') {
            let time: TimeOfDay = time.parse().map_err(|()| bad())?;
            if days == "daily" {
                return Ok(Self::Daily(time));
            }
            let days = Weekdays::from_codes(days.split(',')).ok_or_else(bad)?;
            return Ok(Self::Weekly(days, time));
        }
        let unit = text.chars().last().ok_or_else(bad)?;
        let count: u64 = text[..text.len() - unit.len_utf8()]
            .parse()
            .map_err(|_| bad())?;
        let minutes = match unit {
            'm' => count,
            'h' => count.checked_mul(60).ok_or_else(bad)?,
            'd' => count.checked_mul(24 * 60).ok_or_else(bad)?,
            _ => return Err(bad()),
        };
        let seconds = minutes.checked_mul(60).ok_or_else(bad)?;
        Self::interval(Duration::from_secs(seconds)).ok_or_else(bad)
    }
}

impl fmt::Display for Rule {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Interval(period) => f.write_str(&interval_text(*period)),
            Self::Daily(time) => write!(f, "daily@{time}"),
            Self::Weekly(days, time) => {
                write!(f, "{}@{time}", days.codes().collect::<Vec<_>>().join(","))
            }
        }
    }
}

/// An interval in its largest whole unit: `90m`, `6h`, `3d`.
pub fn interval_text(period: Duration) -> String {
    let minutes = period.as_secs() / 60;
    if minutes > 0 && minutes.is_multiple_of(24 * 60) {
        format!("{}d", minutes / (24 * 60))
    } else if minutes > 0 && minutes.is_multiple_of(60) {
        format!("{}h", minutes / 60)
    } else {
        format!("{minutes}m")
    }
}

/// A named IANA time zone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Zone {
    name: String,
    tz: TimeZone,
}

impl Zone {
    pub fn get(name: &str) -> Result<Self, RepeatError> {
        let tz = TimeZone::get(name).map_err(|_| RepeatError::BadZone(name.to_owned()))?;
        Ok(Self {
            name: name.to_owned(),
            tz,
        })
    }

    pub fn name(&self) -> &str {
        &self.name
    }
}

/// A rule and the zone its wall-clock times are read in. The zone is the bot's when the task is
/// created and stays with the task.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Repeat {
    pub rule: Rule,
    pub zone: Zone,
}

/// The longest stretch searched for the next matching day. A week holds every weekday; the
/// slack covers a calendar day a zone skips entirely.
const SEARCH_DAYS: i64 = 9;

impl Repeat {
    /// The first occurrence at or after `start`. For an interval that is `start` itself, which
    /// then sets the interval's phase.
    pub fn first_at_or_after(&self, start: UnixMillis) -> UnixMillis {
        match self.rule {
            Rule::Interval(_) => start,
            Rule::Daily(_) | Rule::Weekly(..) => {
                self.calendar_after(UnixMillis::new(start.get() - 1))
            }
        }
    }

    /// The first occurrence strictly after both `occurrence` and `after`, on the schedule
    /// `occurrence` belongs to. `occurrence` is the one that just came due; every occurrence up to
    /// `after` is passed over, so a run that was late, or a process that was down, does not make
    /// up the missed ones.
    pub fn next_after(&self, occurrence: UnixMillis, after: UnixMillis) -> UnixMillis {
        let after = after.max(occurrence);
        match self.rule {
            Rule::Interval(period) => {
                let period = i64::try_from(period.as_millis()).unwrap_or(i64::MAX).max(1);
                let steps = (after.get() - occurrence.get()) / period + 1;
                UnixMillis::new(
                    occurrence
                        .get()
                        .saturating_add(steps.saturating_mul(period)),
                )
            }
            Rule::Daily(_) | Rule::Weekly(..) => self.calendar_after(after),
        }
    }

    /// The first daily or weekly occurrence strictly after `after`.
    fn calendar_after(&self, after: UnixMillis) -> UnixMillis {
        let (days, time) = match self.rule {
            Rule::Daily(time) => (None, time),
            Rule::Weekly(days, time) => (Some(days), time),
            Rule::Interval(_) => return after,
        };
        let start = Timestamp::from_millisecond(after.get())
            .unwrap_or(Timestamp::UNIX_EPOCH)
            .to_zoned(self.zone.tz.clone())
            .date();
        // The day before is searched too: a time the clock jumped over on that day can land
        // after `after` on the next.
        let mut date = start.yesterday().unwrap_or(start);
        for _ in 0..=SEARCH_DAYS {
            if days.is_none_or(|d| d.contains(date.weekday()))
                && let Some(at) = self.on(date, time)
                && at > after
            {
                return at;
            }
            let Ok(next) = date.tomorrow() else { break };
            date = next;
        }
        // Unreachable for a real zone; one day on keeps the task moving forward.
        after.plus(Duration::from_secs(24 * 3600))
    }

    /// `time` on `date` in the zone. A time the clock skips is moved forward by the skip, and a
    /// time that happens twice is the earlier one.
    fn on(&self, date: Date, time: TimeOfDay) -> Option<UnixMillis> {
        let local = date.at(time.hour(), time.minute(), 0, 0);
        let zoned = self.zone.tz.to_ambiguous_zoned(local).compatible().ok()?;
        Some(UnixMillis::new(zoned.timestamp().as_millisecond()))
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn ms(text: &str) -> UnixMillis {
        UnixMillis::new(text.parse::<Timestamp>().unwrap().as_millisecond())
    }

    fn repeat(rule: &str, zone: &str) -> Repeat {
        Repeat {
            rule: rule.parse().unwrap(),
            zone: Zone::get(zone).unwrap(),
        }
    }

    #[test]
    fn rules_parse_and_print_in_one_form() {
        for text in [
            "90m",
            "6h",
            "3d",
            "daily@08:00",
            "mon,thu@07:05",
            "sun@23:59",
        ] {
            assert_eq!(text.parse::<Rule>().unwrap().to_string(), text);
        }
        assert_eq!("120m".parse::<Rule>().unwrap().to_string(), "2h");
        assert_eq!(
            "thu,mon@8:00".parse::<Rule>().unwrap().to_string(),
            "mon,thu@08:00"
        );
        for bad in [
            "",
            "0h",
            "h",
            "5s",
            "daily@24:00",
            "daily@8",
            "xyz@08:00",
            "@08:00",
            "mon@",
        ] {
            assert!(bad.parse::<Rule>().is_err(), "{bad:?}");
        }
    }

    #[test]
    fn an_interval_keeps_its_phase_and_skips_what_passed() {
        let r = repeat("2h", "UTC");
        let first = ms("2030-01-01T09:00:00Z");
        assert_eq!(r.first_at_or_after(first), first);
        // On time and a little late: the next one is two hours after the occurrence.
        assert_eq!(r.next_after(first, first), ms("2030-01-01T11:00:00Z"));
        assert_eq!(
            r.next_after(first, ms("2030-01-01T09:40:00Z")),
            ms("2030-01-01T11:00:00Z")
        );
        // Down for most of a day: the missed ones are passed over, the phase stays.
        assert_eq!(
            r.next_after(first, ms("2030-01-01T20:30:00Z")),
            ms("2030-01-01T21:00:00Z")
        );
        // Exactly on a later occurrence: that one counts as passed.
        assert_eq!(
            r.next_after(first, ms("2030-01-01T13:00:00Z")),
            ms("2030-01-01T15:00:00Z")
        );
    }

    #[test]
    fn daily_and_weekly_follow_the_wall_clock_of_the_zone() {
        let r = repeat("daily@08:00", "Asia/Shanghai");
        // 08:00 in Shanghai is 00:00 UTC.
        assert_eq!(
            r.first_at_or_after(ms("2030-01-01T00:00:00Z")),
            ms("2030-01-01T00:00:00Z")
        );
        assert_eq!(
            r.first_at_or_after(ms("2030-01-01T00:00:01Z")),
            ms("2030-01-02T00:00:00Z")
        );
        assert_eq!(
            r.next_after(ms("2030-01-01T00:00:00Z"), ms("2030-01-04T03:00:00Z")),
            ms("2030-01-05T00:00:00Z"),
            "days missed while down are passed over"
        );
        // 2030-01-07 is a Monday.
        let w = repeat("mon,thu@20:30", "Asia/Shanghai");
        let monday = ms("2030-01-07T12:30:00Z");
        assert_eq!(w.first_at_or_after(ms("2030-01-06T00:00:00Z")), monday);
        assert_eq!(w.next_after(monday, monday), ms("2030-01-10T12:30:00Z"));
        assert_eq!(
            w.next_after(ms("2030-01-10T12:30:00Z"), ms("2030-01-10T12:30:00Z")),
            ms("2030-01-14T12:30:00Z")
        );
    }

    #[test]
    fn daylight_saving_moves_a_skipped_time_forward_and_uses_a_doubled_one_once() {
        // New York, 2030: clocks jump 02:00 -> 03:00 on 10 March and go 02:00 -> 01:00 on 3 November.
        let r = repeat("daily@02:30", "America/New_York");
        let before = ms("2030-03-09T07:30:00Z"); // 02:30 EST
        let skipped = r.next_after(before, before);
        assert_eq!(
            skipped,
            ms("2030-03-10T07:30:00Z"),
            "02:30 does not exist; 03:30 EDT"
        );
        assert_eq!(
            r.next_after(skipped, skipped),
            ms("2030-03-11T06:30:00Z"),
            "02:30 EDT"
        );

        let r = repeat("daily@01:30", "America/New_York");
        let before = ms("2030-11-02T05:30:00Z"); // 01:30 EDT
        let doubled = r.next_after(before, before);
        assert_eq!(
            doubled,
            ms("2030-11-03T05:30:00Z"),
            "the earlier 01:30, still EDT"
        );
        assert_eq!(
            r.next_after(doubled, doubled),
            ms("2030-11-04T06:30:00Z"),
            "the repeated 01:30 EST that day is not a second occurrence"
        );
        // Across the change a daily time stays at its wall-clock time, while an interval keeps
        // its length.
        let i = repeat("1d", "America/New_York");
        assert_eq!(i.next_after(before, before), ms("2030-11-03T05:30:00Z"));
        assert_eq!(i.next_after(doubled, doubled), ms("2030-11-04T05:30:00Z"));
    }

    #[test]
    fn a_calendar_day_a_zone_skips_does_not_fire_twice() {
        // Samoa skipped 30 December 2011 entirely.
        let r = repeat("daily@08:00", "Pacific/Apia");
        let mut at = r.first_at_or_after(ms("2011-12-27T00:00:00Z"));
        let mut seen = Vec::new();
        for _ in 0..6 {
            seen.push(at);
            at = r.next_after(at, at);
        }
        // 08:00 at UTC-10 until the 29th; the 30th's 08:00, read in the offset before the jump,
        // is the instant of the 31st's 08:00 at UTC+14, so the two are one occurrence.
        let expected = [
            "2011-12-27T18:00:00Z",
            "2011-12-28T18:00:00Z",
            "2011-12-29T18:00:00Z",
            "2011-12-30T18:00:00Z",
            "2011-12-31T18:00:00Z",
            "2012-01-01T18:00:00Z",
        ]
        .map(ms);
        assert_eq!(seen, expected);
    }

    #[test]
    fn unknown_zones_are_refused() {
        assert_eq!(
            Zone::get("Mars/Olympus"),
            Err(RepeatError::BadZone("Mars/Olympus".into()))
        );
    }
}
