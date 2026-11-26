//! `datetime`: calendar dates and times of day with a UTC offset, formatting and parsing.
//!
//! A `DateTime` is a plain struct of integers (`year month day hour minute second nanosecond`
//! and `offset`, the seconds east of UTC the fields are written in), so it compares and prints
//! like any struct. All the calendar arithmetic is done here with the proleptic Gregorian
//! calendar (days from civil, after Howard Hinnant), with no dependency and no leap seconds,
//! which keeps every result identical on every platform.

use colibri_runtime::{Cap, Ctx, Error, TypedError, Vm};

use crate::config::State;
use crate::errors::error_kind;
use crate::needs;
use crate::plain;
use crate::values::value_struct;

error_kind!(
    "datetime",
    /// Why a date and time could not be made or read (`datetime.Error.OutOfRange`).
    Kind {
        /// A field is outside its range: month 13, February 30th, hour 24.
        OutOfRange,
        /// The text does not have the form the pattern (or ISO 8601) says.
        Syntax,
        /// The pattern itself is wrong: an unknown specifier, `%H` together with `%I`.
        InvalidPattern,
    }
);

/// A failure to read a date, with the variant of `datetime.Error` it is. Each failure is made
/// with the constructor that names its kind (there is no conversion from a message, which would
/// pick a kind silently).
#[derive(Debug)]
struct Bad(Kind, String);

impl Bad {
    /// A field is outside its range.
    fn out_of_range(message: impl Into<String>) -> Bad {
        Bad(Kind::OutOfRange, message.into())
    }

    /// The pattern itself is wrong.
    fn invalid_pattern(message: impl Into<String>) -> Bad {
        Bad(Kind::InvalidPattern, message.into())
    }

    fn into_error(self, message: impl FnOnce(&str) -> String) -> TypedError {
        self.0.error(message(&self.1))
    }
}

const NANOS: i128 = 1_000_000_000;

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct DateTime {
    pub year: i64,
    pub month: i64,
    pub day: i64,
    pub hour: i64,
    pub minute: i64,
    pub second: i64,
    pub nanosecond: i64,
    pub offset: i64,
}

value_struct!(DateTime {
    year: i64,
    month: i64,
    day: i64,
    hour: i64,
    minute: i64,
    second: i64,
    nanosecond: i64,
    offset: i64,
});

const MIN_YEAR: i64 = 0;
const MAX_YEAR: i64 = 9999;
/// Offsets of a real time zone are within a day.
const MAX_OFFSET: i64 = 86_400 - 1;

/// Days since 1970-01-01 of a proleptic Gregorian date.
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// The date of a day number: the inverse of [`days_from_civil`].
fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m, d)
}

fn is_leap(y: i64) -> bool {
    (y % 4 == 0 && y % 100 != 0) || y % 400 == 0
}

fn days_in_month(y: i64, m: i64) -> i64 {
    match m {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        _ if is_leap(y) => 29,
        _ => 28,
    }
}

impl DateTime {
    /// Checks every field and says which one is wrong.
    fn validated(self) -> Result<DateTime, String> {
        fn within(name: &str, v: i64, lo: i64, hi: i64) -> Result<(), String> {
            if (lo..=hi).contains(&v) {
                Ok(())
            } else {
                Err(format!("the {name} {v} is not between {lo} and {hi}"))
            }
        }
        within("year", self.year, MIN_YEAR, MAX_YEAR)?;
        within("month", self.month, 1, 12)?;
        let last = days_in_month(self.year, self.month);
        if !(1..=last).contains(&self.day) {
            return Err(format!(
                "the day {} is not a day of {}-{:02} (it has {last})",
                self.day, self.year, self.month
            ));
        }
        within("hour", self.hour, 0, 23)?;
        within("minute", self.minute, 0, 59)?;
        within("second", self.second, 0, 59)?;
        within("nanosecond", self.nanosecond, 0, 999_999_999)?;
        within("offset", self.offset, -MAX_OFFSET, MAX_OFFSET)?;
        Ok(self)
    }

    fn day_number(&self) -> i64 {
        days_from_civil(self.year, self.month, self.day)
    }

    /// Nanoseconds since the Unix epoch, UTC.
    fn unix_nanos(&self) -> i128 {
        let secs = i128::from(self.day_number()) * 86_400
            + i128::from(self.hour * 3600 + self.minute * 60 + self.second - self.offset);
        secs * NANOS + i128::from(self.nanosecond)
    }

    /// The fields of an instant seen at `offset`. `None` when the year is out of range.
    fn from_unix_nanos(nanos: i128, offset: i64) -> Option<DateTime> {
        let local = nanos + i128::from(offset) * NANOS;
        let secs = local.div_euclid(NANOS);
        let nano = local.rem_euclid(NANOS) as i64;
        let days = i64::try_from(secs.div_euclid(86_400)).ok()?;
        let in_day = secs.rem_euclid(86_400) as i64;
        let (year, month, day) = civil_from_days(days);
        if !(MIN_YEAR..=MAX_YEAR).contains(&year) {
            return None;
        }
        Some(DateTime {
            year,
            month,
            day,
            hour: in_day / 3600,
            minute: in_day / 60 % 60,
            second: in_day % 60,
            nanosecond: nano,
            offset,
        })
    }

    /// 1 is Monday, 7 is Sunday.
    fn weekday(&self) -> i64 {
        (self.day_number() + 3).rem_euclid(7) + 1
    }

    fn day_of_year(&self) -> i64 {
        self.day_number() - days_from_civil(self.year, 1, 1) + 1
    }
}

const MONTHS: [&str; 12] = [
    "January",
    "February",
    "March",
    "April",
    "May",
    "June",
    "July",
    "August",
    "September",
    "October",
    "November",
    "December",
];
const WEEKDAYS: [&str; 7] = [
    "Monday",
    "Tuesday",
    "Wednesday",
    "Thursday",
    "Friday",
    "Saturday",
    "Sunday",
];

fn offset_text(offset: i64, colon: bool) -> String {
    let sign = if offset < 0 { '-' } else { '+' };
    let a = offset.abs();
    let (h, m) = (a / 3600, a / 60 % 60);
    if colon {
        format!("{sign}{h:02}:{m:02}")
    } else {
        format!("{sign}{h:02}{m:02}")
    }
}

/// `format`: strftime-like. `Err` names a specifier it does not know.
fn format(dt: &DateTime, pattern: &str) -> Result<String, String> {
    let mut out = String::new();
    let mut chars = pattern.chars();
    while let Some(c) = chars.next() {
        if c != '%' {
            out.push(c);
            continue;
        }
        let spec = chars
            .next()
            .ok_or("the pattern ends with a lone `%` (write `%%` for a percent sign)")?;
        match spec {
            'Y' => out.push_str(&format!("{:04}", dt.year)),
            'y' => out.push_str(&format!("{:02}", dt.year % 100)),
            'm' => out.push_str(&format!("{:02}", dt.month)),
            'd' => out.push_str(&format!("{:02}", dt.day)),
            'e' => out.push_str(&format!("{:2}", dt.day)),
            'j' => out.push_str(&format!("{:03}", dt.day_of_year())),
            'H' => out.push_str(&format!("{:02}", dt.hour)),
            'I' => out.push_str(&format!("{:02}", (dt.hour + 11) % 12 + 1)),
            'M' => out.push_str(&format!("{:02}", dt.minute)),
            'S' => out.push_str(&format!("{:02}", dt.second)),
            'f' => out.push_str(&format!("{:09}", dt.nanosecond)),
            'L' => out.push_str(&format!("{:03}", dt.nanosecond / 1_000_000)),
            'p' => out.push_str(if dt.hour < 12 { "AM" } else { "PM" }),
            'B' => out.push_str(MONTHS[(dt.month - 1) as usize]),
            'b' | 'h' => out.push_str(&MONTHS[(dt.month - 1) as usize][..3]),
            'A' => out.push_str(WEEKDAYS[(dt.weekday() - 1) as usize]),
            'a' => out.push_str(&WEEKDAYS[(dt.weekday() - 1) as usize][..3]),
            'u' => out.push_str(&dt.weekday().to_string()),
            'z' => out.push_str(&offset_text(dt.offset, false)),
            'Z' => out.push_str(&if dt.offset == 0 {
                "UTC".to_string()
            } else {
                offset_text(dt.offset, true)
            }),
            's' => out.push_str(&dt.unix_nanos().div_euclid(NANOS).to_string()),
            'F' => out.push_str(&format!("{:04}-{:02}-{:02}", dt.year, dt.month, dt.day)),
            'T' => out.push_str(&format!("{:02}:{:02}:{:02}", dt.hour, dt.minute, dt.second)),
            ':' => {
                if chars.next() != Some('z') {
                    return Err("`%:` is only known as `%:z` (an offset like +05:30)".into());
                }
                out.push_str(&offset_text(dt.offset, true));
            }
            '%' => out.push('%'),
            other => return Err(format!("unknown specifier `%{other}` in the pattern")),
        }
    }
    Ok(out)
}

/// A cursor over the text being parsed.
struct Scan<'a> {
    rest: &'a str,
}

impl Scan<'_> {
    fn peek(&self) -> Option<char> {
        self.rest.chars().next()
    }

    fn eat(&mut self, c: char) -> bool {
        if self.peek() == Some(c) {
            self.rest = &self.rest[c.len_utf8()..];
            true
        } else {
            false
        }
    }

    /// Between `min` and `max` ASCII digits, as a number.
    fn digits(&mut self, min: usize, max: usize) -> Option<(i64, usize)> {
        let n = self
            .rest
            .bytes()
            .take(max)
            .take_while(u8::is_ascii_digit)
            .count();
        if n < min || n == 0 {
            return None;
        }
        let value = self.rest[..n].parse().ok()?;
        self.rest = &self.rest[n..];
        Some((value, n))
    }

    /// One of `names` (the whole name, or its first three letters if `short`), ignoring case.
    fn name(&mut self, names: &[&str], short: bool) -> Option<usize> {
        // The long form first, so that "May" and "March" never stop at "Mar".
        for full in [true, false] {
            if !full && !short {
                continue;
            }
            for (i, name) in names.iter().enumerate() {
                let want = if full { *name } else { &name[..3] };
                if let Some(head) = self.rest.get(..want.len())
                    && head.eq_ignore_ascii_case(want)
                {
                    self.rest = &self.rest[want.len()..];
                    return Some(i);
                }
            }
        }
        None
    }

    /// `Z`, `+hh:mm`, `+hhmm` or `+hh`, as seconds east of UTC.
    fn offset(&mut self) -> Option<i64> {
        if self.eat('Z') || self.eat('z') {
            return Some(0);
        }
        let sign = if self.eat('+') {
            1
        } else if self.eat('-') {
            -1
        } else {
            return None;
        };
        let (h, _) = self.digits(2, 2)?;
        let colon = self.eat(':');
        let m = match self.digits(2, 2) {
            Some((m, _)) => m,
            None if colon => return None,
            None => 0,
        };
        if h > 23 || m > 59 {
            return None;
        }
        Some(sign * (h * 3600 + m * 60))
    }
}

/// What a pattern has read so far.
#[derive(Default)]
struct Fields {
    year: Option<i64>,
    month: Option<i64>,
    day: Option<i64>,
    day_of_year: Option<i64>,
    hour: Option<i64>,
    hour12: Option<i64>,
    pm: Option<bool>,
    minute: Option<i64>,
    second: Option<i64>,
    nanosecond: Option<i64>,
    offset: Option<i64>,
    unix: Option<i64>,
}

fn parse(text: &str, pattern: &str) -> Result<DateTime, Bad> {
    let fail = |what: &str, at: &Scan| {
        let consumed = text.len() - at.rest.len();
        Bad(
            Kind::Syntax,
            format!("expected {what} at position {consumed}, found `{}`", {
                let shown: String = at.rest.chars().take(12).collect();
                if shown.is_empty() {
                    "the end of the text".to_string()
                } else {
                    shown
                }
            }),
        )
    };
    let mut scan = Scan { rest: text };
    let mut f = Fields::default();
    let mut chars = pattern.chars();
    while let Some(c) = chars.next() {
        if c != '%' {
            if !scan.eat(c) {
                return Err(fail(&format!("`{c}`"), &scan));
            }
            continue;
        }
        let spec = chars
            .next()
            .ok_or_else(|| Bad::invalid_pattern("the pattern ends with a lone `%`"))?;
        macro_rules! number {
            ($slot:expr, $min:expr, $max:expr, $what:expr) => {
                match scan.digits($min, $max) {
                    Some((v, _)) => $slot = Some(v),
                    None => return Err(fail($what, &scan)),
                }
            };
        }
        match spec {
            'Y' => number!(f.year, 4, 4, "a four-digit year"),
            'y' => {
                number!(f.year, 2, 2, "a two-digit year");
                // POSIX: 69 to 99 are the 1900s, 00 to 68 the 2000s.
                let y = f.year.unwrap_or(0);
                f.year = Some(if y >= 69 { 1900 + y } else { 2000 + y });
            }
            'm' => number!(f.month, 1, 2, "a month number"),
            'd' => number!(f.day, 1, 2, "a day of the month"),
            'e' => {
                scan.eat(' ');
                number!(f.day, 1, 2, "a day of the month")
            }
            'j' => number!(f.day_of_year, 1, 3, "a day of the year"),
            'H' => number!(f.hour, 1, 2, "an hour (0 to 23)"),
            'I' => number!(f.hour12, 1, 2, "an hour (1 to 12)"),
            'M' => number!(f.minute, 1, 2, "a minute"),
            'S' => number!(f.second, 1, 2, "a second"),
            'f' => match scan.digits(1, 9) {
                Some((v, n)) => f.nanosecond = Some(v * 10_i64.pow(9 - n as u32)),
                None => return Err(fail("the digits of a fraction of a second", &scan)),
            },
            'L' => match scan.digits(3, 3) {
                Some((v, _)) => f.nanosecond = Some(v * 1_000_000),
                None => return Err(fail("three digits of milliseconds", &scan)),
            },
            'p' => {
                if scan.name(&["AM"], false).is_some() {
                    f.pm = Some(false);
                } else if scan.name(&["PM"], false).is_some() {
                    f.pm = Some(true);
                } else {
                    return Err(fail("AM or PM", &scan));
                }
            }
            'B' | 'b' | 'h' => match scan.name(&MONTHS, true) {
                Some(i) => f.month = Some(i as i64 + 1),
                None => return Err(fail("a month name", &scan)),
            },
            'A' | 'a' => {
                if scan.name(&WEEKDAYS, true).is_none() {
                    return Err(fail("a day name", &scan));
                }
            }
            'z' => match scan.offset() {
                Some(o) => f.offset = Some(o),
                None => return Err(fail("a UTC offset such as +0530, +05:30 or Z", &scan)),
            },
            ':' => {
                if chars.next() != Some('z') {
                    return Err(Bad::invalid_pattern("`%:` is only known as `%:z`"));
                }
                match scan.offset() {
                    Some(o) => f.offset = Some(o),
                    None => return Err(fail("a UTC offset such as +05:30 or Z", &scan)),
                }
            }
            's' => {
                let neg = scan.eat('-');
                match scan.digits(1, 15) {
                    Some((v, _)) => f.unix = Some(if neg { -v } else { v }),
                    None => return Err(fail("a number of seconds since 1970", &scan)),
                }
            }
            'F' => {
                number!(f.year, 4, 4, "a four-digit year");
                if !scan.eat('-') {
                    return Err(fail("`-`", &scan));
                }
                number!(f.month, 1, 2, "a month number");
                if !scan.eat('-') {
                    return Err(fail("`-`", &scan));
                }
                number!(f.day, 1, 2, "a day of the month");
            }
            'T' => {
                number!(f.hour, 1, 2, "an hour (0 to 23)");
                if !scan.eat(':') {
                    return Err(fail("`:`", &scan));
                }
                number!(f.minute, 1, 2, "a minute");
                if !scan.eat(':') {
                    return Err(fail("`:`", &scan));
                }
                number!(f.second, 1, 2, "a second");
            }
            '%' => {
                if !scan.eat('%') {
                    return Err(fail("`%`", &scan));
                }
            }
            other => {
                return Err(Bad::invalid_pattern(format!(
                    "unknown specifier `%{other}` in the pattern"
                )));
            }
        }
    }
    if !scan.rest.is_empty() {
        let consumed = text.len() - scan.rest.len();
        return Err(Bad(
            Kind::Syntax,
            format!(
                "unexpected `{}` after the end of the pattern (position {consumed})",
                scan.rest.chars().take(12).collect::<String>()
            ),
        ));
    }
    let offset = f.offset.unwrap_or(0);
    if let Some(unix) = f.unix {
        return DateTime::from_unix_nanos(i128::from(unix) * NANOS, offset).ok_or_else(|| {
            Bad::out_of_range(format!(
                "{unix} seconds since 1970 is outside the years 0 to 9999"
            ))
        });
    }
    let hour = match (f.hour, f.hour12, f.pm) {
        (Some(h), None, _) => h,
        (None, Some(h), pm) => {
            if !(1..=12).contains(&h) {
                return Err(Bad::out_of_range(format!(
                    "the hour {h} is not between 1 and 12"
                )));
            }
            h % 12 + if pm == Some(true) { 12 } else { 0 }
        }
        (Some(_), Some(_), _) => {
            return Err(Bad::invalid_pattern("the pattern has both %H and %I"));
        }
        (None, None, _) => 0,
    };
    let year = f.year.unwrap_or(1970);
    let (month, day) = match (f.month, f.day, f.day_of_year) {
        (None, None, Some(doy)) => {
            let max = if is_leap(year) { 366 } else { 365 };
            if !(1..=max).contains(&doy) {
                return Err(Bad::out_of_range(format!(
                    "the day of the year {doy} is not between 1 and {max}"
                )));
            }
            let (_, m, d) = civil_from_days(days_from_civil(year, 1, 1) + doy - 1);
            (m, d)
        }
        (m, d, _) => (m.unwrap_or(1), d.unwrap_or(1)),
    };
    DateTime {
        year,
        month,
        day,
        hour,
        minute: f.minute.unwrap_or(0),
        second: f.second.unwrap_or(0),
        nanosecond: f.nanosecond.unwrap_or(0),
        offset,
    }
    .validated()
    .map_err(Bad::out_of_range)
}

fn parse_iso(text: &str) -> Result<DateTime, Bad> {
    let mut scan = Scan { rest: text.trim() };
    let bad = |what: &str| {
        Bad(
            Kind::Syntax,
            format!("`{text}` is not an ISO 8601 date and time: {what}"),
        )
    };
    let (year, _) = scan
        .digits(4, 4)
        .ok_or_else(|| bad("expected a four-digit year"))?;
    if !scan.eat('-') {
        return Err(bad("expected `-` after the year"));
    }
    let (month, _) = scan
        .digits(2, 2)
        .ok_or_else(|| bad("expected a two-digit month"))?;
    if !scan.eat('-') {
        return Err(bad("expected `-` after the month"));
    }
    let (day, _) = scan
        .digits(2, 2)
        .ok_or_else(|| bad("expected a two-digit day"))?;
    let (mut hour, mut minute, mut second, mut nanosecond, mut offset) = (0, 0, 0, 0, 0);
    if scan.eat('T') || scan.eat('t') || scan.eat(' ') {
        (hour, _) = scan
            .digits(2, 2)
            .ok_or_else(|| bad("expected a two-digit hour"))?;
        if !scan.eat(':') {
            return Err(bad("expected `:` after the hour"));
        }
        (minute, _) = scan
            .digits(2, 2)
            .ok_or_else(|| bad("expected a two-digit minute"))?;
        if scan.eat(':') {
            (second, _) = scan
                .digits(2, 2)
                .ok_or_else(|| bad("expected a two-digit second"))?;
            if scan.eat('.') || scan.eat(',') {
                let (v, n) = scan
                    .digits(1, 9)
                    .ok_or_else(|| bad("expected digits after the decimal point"))?;
                nanosecond = v * 10_i64.pow(9 - n as u32);
                // More digits than nanoseconds are cut.
                while scan.peek().is_some_and(|c| c.is_ascii_digit()) {
                    scan.rest = &scan.rest[1..];
                }
            }
        }
        if !scan.rest.is_empty() {
            offset = scan
                .offset()
                .ok_or_else(|| bad("expected `Z` or an offset such as +02:00"))?;
        }
    }
    if !scan.rest.is_empty() {
        return Err(bad(&format!("unexpected `{}`", scan.rest)));
    }
    DateTime {
        year,
        month,
        day,
        hour,
        minute,
        second,
        nanosecond,
        offset,
    }
    .validated()
    .map_err(|e| Bad(Kind::OutOfRange, bad(&e).1))
}

fn to_iso(dt: &DateTime) -> String {
    let mut out = format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}",
        dt.year, dt.month, dt.day, dt.hour, dt.minute, dt.second
    );
    if dt.nanosecond != 0 {
        let frac = format!("{:09}", dt.nanosecond);
        out.push('.');
        out.push_str(frac.trim_end_matches('0'));
    }
    if dt.offset == 0 {
        out.push('Z');
    } else {
        out.push_str(&offset_text(dt.offset, true));
    }
    out
}

/// The local offset, in seconds east of UTC, at the instant `unix` (whole seconds).
fn local_offset_at(state: &State, unix: i64) -> i64 {
    if let Some(fixed) = state.local_offset {
        return i64::from(fixed);
    }
    system_offset(unix)
}

#[cfg(unix)]
#[allow(clippy::unnecessary_cast, clippy::useless_conversion)]
fn system_offset(unix: i64) -> i64 {
    let t = unix as libc::time_t;
    // SAFETY: `tm` is plain data that `localtime_r` fills in; `t` outlives the call. The
    // reentrant variant keeps no global state of ours.
    unsafe {
        let mut tm: libc::tm = std::mem::zeroed();
        if libc::localtime_r(&t, &mut tm).is_null() {
            return 0;
        }
        i64::from(tm.tm_gmtoff)
    }
}

/// On the web the page knows the time zone (`host_utc_offset`, from `Date`).
#[cfg(target_family = "wasm")]
fn system_offset(unix: i64) -> i64 {
    crate::config::host_offset(unix)
}

/// Without a way to ask the machine, local time is UTC.
#[cfg(not(any(unix, target_family = "wasm")))]
fn system_offset(_unix: i64) -> i64 {
    0
}

fn out_of_range(cx: &mut Ctx, what: &str) -> ! {
    cx.raise(format!(
        "datetime.{what}: the result is outside the years {MIN_YEAR} to {MAX_YEAR}"
    ))
}

/// Unix seconds as a float to nanoseconds: the whole seconds and the fraction apart, so that
/// the fraction is not blurred by the size of the whole part.
fn unix_float_nanos(seconds: f64) -> i128 {
    let whole = seconds.floor();
    i128::from(whole as i64) * NANOS + ((seconds - whole) * 1e9).round() as i128
}

/// Seconds to nanoseconds, rejecting what is not a time span.
fn span_nanos(cx: &mut Ctx, what: &str, seconds: f64) -> i128 {
    let nanos = (seconds * 1e9).round();
    if !seconds.is_finite() || nanos.abs() > 4e20 {
        cx.raise(format!(
            "datetime.{what}: {seconds} is not a number of seconds that fits a date (it must be finite and within about 10000 years)"
        ));
    }
    nanos as i128
}

/// The wall clock as (whole seconds, nanoseconds).
fn wall_clock(cx: &mut Ctx) -> i128 {
    let secs = cx
        .state_mut::<State>()
        .map_or(0.0, |s| s.clock.unix_seconds());
    unix_float_nanos(secs)
}

pub(crate) fn register(vm: &mut Vm) -> Result<(), Error> {
    Kind::declare(vm)?;
    let mut m = vm.std_module("datetime");
    m.register_const_float("MINUTE", 60.0)?;
    m.register_const_float("HOUR", 3600.0)?;
    m.register_const_float("DAY", 86_400.0)?;
    m.register_const_float("WEEK", 604_800.0)?;

    // The wall clock is machine state, so reading it needs `os` (like `time.unix`).
    needs(
        Cap::Os,
        m.register_fn_named("now", [], |cx: &mut Ctx| {
            let nanos = wall_clock(cx);
            DateTime::from_unix_nanos(nanos, 0).unwrap_or_else(|| out_of_range(cx, "now"))
        }),
    )?;
    needs(
        Cap::Os,
        m.register_fn_named("now_local", [], |cx: &mut Ctx| {
            let nanos = wall_clock(cx);
            let secs = i64::try_from(nanos.div_euclid(NANOS)).unwrap_or(0);
            let offset = cx
                .state_mut::<State>()
                .map_or(0, |s| local_offset_at(s, secs));
            DateTime::from_unix_nanos(nanos, offset)
                .unwrap_or_else(|| out_of_range(cx, "now_local"))
        }),
    )?;
    plain(m.register_fn_named(
        "new",
        ["year", "month", "day", "hour", "minute", "second"],
        |year: i64, month: i64, day: i64, hour: i64, minute: i64, second: i64| {
            DateTime {
                year,
                month,
                day,
                hour,
                minute,
                second,
                nanosecond: 0,
                offset: 0,
            }
            .validated()
            .map_err(|e| Kind::OutOfRange.error(format!("datetime.new: {e}")))
        },
    ))?;
    plain(
        m.register_fn_named("from_unix", ["seconds"], |cx: &mut Ctx, seconds: f64| {
            if !seconds.is_finite() || seconds.abs() > 4e11 {
                cx.raise(format!(
                    "datetime.from_unix: {seconds} is not a number of seconds that fits a date"
                ));
            }
            let nanos = unix_float_nanos(seconds);
            DateTime::from_unix_nanos(nanos, 0).unwrap_or_else(|| out_of_range(cx, "from_unix"))
        }),
    )?;
    plain(m.register_fn_named("to_unix", ["dt"], |dt: DateTime| {
        dt.unix_nanos() as f64 / 1e9
    }))?;
    // The same instant written in UTC, in the local time zone, or at a given offset.
    plain(m.register_fn_named("to_utc", ["dt"], |dt: DateTime| {
        DateTime::from_unix_nanos(dt.unix_nanos(), 0).unwrap_or(dt)
    }))?;
    plain(
        m.register_fn_named("to_local", ["dt"], |cx: &mut Ctx, dt: DateTime| {
            let nanos = dt.unix_nanos();
            let secs = i64::try_from(nanos.div_euclid(NANOS)).unwrap_or(0);
            let offset = cx
                .state_mut::<State>()
                .map_or(0, |s| local_offset_at(s, secs));
            DateTime::from_unix_nanos(nanos, offset).unwrap_or_else(|| out_of_range(cx, "to_local"))
        }),
    )?;
    plain(m.register_fn_named(
        "with_offset",
        ["dt", "seconds"],
        |cx: &mut Ctx, dt: DateTime, seconds: i64| {
            if seconds.abs() > MAX_OFFSET {
                cx.raise(format!(
                    "datetime.with_offset: the offset {seconds} is not within a day (86399 seconds either way)"
                ));
            }
            DateTime::from_unix_nanos(dt.unix_nanos(), seconds)
                .unwrap_or_else(|| out_of_range(cx, "with_offset"))
        },
    ))?;
    plain(m.register_fn_named(
        "add",
        ["dt", "seconds"],
        |cx: &mut Ctx, dt: DateTime, seconds: f64| {
            let span = span_nanos(cx, "add", seconds);
            DateTime::from_unix_nanos(dt.unix_nanos() + span, dt.offset)
                .unwrap_or_else(|| out_of_range(cx, "add"))
        },
    ))?;
    // Seconds from `b` to `a`: positive when `a` is later.
    plain(
        m.register_fn_named("diff", ["a", "b"], |a: DateTime, b: DateTime| {
            (a.unix_nanos() - b.unix_nanos()) as f64 / 1e9
        }),
    )?;
    plain(m.register_fn_named("weekday", ["dt"], |dt: DateTime| dt.weekday()))?;
    plain(m.register_fn_named("day_of_year", ["dt"], |dt: DateTime| dt.day_of_year()))?;
    plain(m.register_fn_named("is_leap_year", ["year"], is_leap))?;
    plain(m.register_fn_named(
        "days_in_month",
        ["year", "month"],
        |cx: &mut Ctx, year: i64, month: i64| {
            if !(1..=12).contains(&month) {
                cx.raise(format!(
                    "datetime.days_in_month: the month {month} is not between 1 and 12"
                ));
            }
            days_in_month(year, month)
        },
    ))?;
    plain(m.register_fn_named(
        "format",
        ["dt", "pattern"],
        |cx: &mut Ctx, dt: DateTime, pattern: String| {
            format(&dt, &pattern).unwrap_or_else(|e| cx.raise(format!("datetime.format: {e}")))
        },
    ))?;
    plain(m.register_fn_named(
        "parse",
        ["text", "pattern"],
        |text: String, pattern: String| {
            parse(&text, &pattern)
                .map_err(|e| e.into_error(|m| format!("cannot parse `{text}` as `{pattern}`: {m}")))
        },
    ))?;
    plain(m.register_fn_named("to_iso", ["dt"], |dt: DateTime| to_iso(&dt)))?;
    plain(m.register_fn_named("parse_iso", ["text"], |text: String| {
        parse_iso(&text).map_err(|e| e.into_error(str::to_string))
    }))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dt(y: i64, mo: i64, d: i64, h: i64, mi: i64, s: i64) -> DateTime {
        DateTime {
            year: y,
            month: mo,
            day: d,
            hour: h,
            minute: mi,
            second: s,
            nanosecond: 0,
            offset: 0,
        }
    }

    #[test]
    fn the_epoch_and_a_known_instant() {
        assert_eq!(days_from_civil(1970, 1, 1), 0);
        assert_eq!(dt(1970, 1, 1, 0, 0, 0).weekday(), 4); // a Thursday
        // 1_700_000_000 is 2023-11-14 22:13:20 UTC, a Tuesday.
        let d = DateTime::from_unix_nanos(1_700_000_000 * NANOS, 0).unwrap();
        assert_eq!(d, dt(2023, 11, 14, 22, 13, 20));
        assert_eq!(d.weekday(), 2);
        assert_eq!(d.unix_nanos(), 1_700_000_000 * NANOS);
    }

    #[test]
    fn days_round_trip_over_four_centuries() {
        for z in (-800_000..800_000).step_by(37) {
            let (y, m, d) = civil_from_days(z);
            assert_eq!(days_from_civil(y, m, d), z);
            assert!((1..=days_in_month(y, m)).contains(&d));
        }
    }

    #[test]
    fn leap_years() {
        assert!(is_leap(2000) && is_leap(2024) && !is_leap(1900) && !is_leap(2023));
        assert!(dt(2024, 2, 29, 0, 0, 0).validated().is_ok());
        assert!(dt(2023, 2, 29, 0, 0, 0).validated().is_err());
    }

    #[test]
    fn negative_instants_floor_correctly() {
        let d = DateTime::from_unix_nanos(-1, 0).unwrap();
        assert_eq!((d.year, d.second, d.nanosecond), (1969, 59, 999_999_999));
    }

    #[test]
    fn formatting() {
        let d = dt(2024, 3, 5, 14, 7, 9);
        assert_eq!(
            format(&d, "%Y-%m-%d %H:%M:%S").unwrap(),
            "2024-03-05 14:07:09"
        );
        assert_eq!(
            format(&d, "%A %e %B %y %I%p %j").unwrap(),
            "Tuesday  5 March 24 02PM 065"
        );
        assert_eq!(
            format(&d, "%a %b %% %z %:z %Z %s").unwrap(),
            "Tue Mar % +0000 +00:00 UTC 1709647629"
        );
        assert!(format(&d, "%Q").is_err());
        assert!(format(&d, "100%").is_err());
    }

    #[test]
    fn parsing() {
        let d = parse("2024-03-05 02:07:09 PM", "%Y-%m-%d %I:%M:%S %p").unwrap();
        assert_eq!(d, dt(2024, 3, 5, 14, 7, 9));
        let d = parse("Tue, 05 Mar 2024 14:07:09 +0530", "%a, %d %b %Y %T %z").unwrap();
        assert_eq!((d.hour, d.offset), (14, 19_800));
        assert!(parse("2024-13-01", "%F").is_err());
        assert!(parse("2024-02-30", "%F").is_err());
        assert!(parse("2024-02-03x", "%F").is_err());
        assert_eq!(
            parse("2024 060", "%Y %j").unwrap(),
            dt(2024, 2, 29, 0, 0, 0)
        );
        assert_eq!(parse("May 5 99", "%B %d %y").unwrap().year, 1999);
    }

    #[test]
    fn iso() {
        let d = parse_iso("2024-03-05T14:07:09.25+05:30").unwrap();
        assert_eq!(to_iso(&d), "2024-03-05T14:07:09.25+05:30");
        assert_eq!(to_iso(&dt(2024, 1, 1, 0, 0, 0)), "2024-01-01T00:00:00Z");
        assert_eq!(parse_iso("2024-01-01").unwrap(), dt(2024, 1, 1, 0, 0, 0));
        assert!(parse_iso("2024-1-1").is_err());
        assert!(parse_iso("2024-01-01T25:00:00Z").is_err());
    }
}
