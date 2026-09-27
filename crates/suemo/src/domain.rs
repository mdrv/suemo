//! Domain model: events, local-time windows, time-spec parsing, kind colors.

use anyhow::{Context, Result, anyhow, bail, ensure};
use chrono::{DateTime, Datelike, Duration, Local, LocalResult, NaiveDate, TimeZone, Utc};
use serde::{Deserialize, Serialize};

/// Default kind when `--kind` is omitted (decisions.md, round-4 addendum).
pub const DEFAULT_KIND: &str = "general";
/// Default duration: click-create and `suemo add` without an end (Q8.7).
pub const DEFAULT_DURATION_MINUTES: i64 = 60;
/// Kind colors: FNV-1a hash → this many evenly-spaced hue buckets (Q2).
pub const PALETTE_SIZE: usize = 16;

pub fn now_ms() -> i64 {
    Utc::now().timestamp_millis()
}

/// One event, two readings: upcoming = commitment, past = record
/// (proposal §Product). UTC epoch-ms everywhere; day boundaries are
/// computed at render in the system's local timezone.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Event {
    pub id: String,
    pub starts_utc: i64,
    pub ends_utc: i64,
    pub title: String,
    pub kind: String,
    #[serde(default)]
    pub note: String,
    pub created_utc: i64,
    pub updated_utc: i64,
}

impl Event {
    /// A fresh event: ULID id, created = updated = now.
    pub fn new(
        title: impl Into<String>,
        kind: Option<String>,
        starts_utc: i64,
        ends_utc: i64,
    ) -> Self {
        let now = now_ms();
        Self {
            id: ulid::Ulid::new().to_string(),
            starts_utc,
            ends_utc,
            title: title.into(),
            kind: kind.unwrap_or_else(|| DEFAULT_KIND.to_string()),
            note: String::new(),
            created_utc: now,
            updated_utc: now,
        }
    }

    pub fn validate(&self) -> Result<()> {
        ensure!(!self.title.trim().is_empty(), "title must not be empty");
        ensure!(!self.kind.trim().is_empty(), "kind must not be empty");
        ensure!(self.ends_utc > self.starts_utc, "end must be after start");
        Ok(())
    }

    /// Overlap with `[from, to)` in ms (≥ 0) — stats and grid rendering.
    pub fn clipped_ms(&self, from: i64, to: i64) -> i64 {
        (self.ends_utc.min(to) - self.starts_utc.max(from)).max(0)
    }
}

/// `[from, to)` epoch-ms of the local day containing `at`.
pub fn day_window<Tz: TimeZone>(at: DateTime<Tz>) -> (i64, i64) {
    let start = local_midnight(at.timezone(), at.date_naive());
    let end = local_midnight(at.timezone(), at.date_naive() + Duration::days(1));
    (start, end)
}

/// `[from, to)` of the local Monday-start week containing `at` (Q8.5).
pub fn week_window<Tz: TimeZone>(at: DateTime<Tz>) -> (i64, i64) {
    let monday = at.date_naive() - Duration::days(i64::from(at.weekday().num_days_from_monday()));
    let start = local_midnight(at.timezone(), monday);
    let end = local_midnight(at.timezone(), monday + Duration::days(7));
    (start, end)
}

pub fn today_window() -> (i64, i64) {
    day_window(Local::now())
}

pub fn this_week_window() -> (i64, i64) {
    week_window(Local::now())
}

fn local_midnight<Tz: TimeZone>(tz: Tz, day: NaiveDate) -> i64 {
    let naive = day.and_hms_opt(0, 0, 0).expect("midnight is representable");
    match tz.from_local_datetime(&naive) {
        LocalResult::Single(dt) => dt.timestamp_millis(),
        // DST spring-forward can skip midnight in exotic zones: take the
        // earliest instant of that day that exists.
        _ => tz
            .from_local_datetime(&day.and_hms_opt(1, 0, 0).expect("1am is representable"))
            .earliest()
            .expect("some instant of the day exists")
            .timestamp_millis(),
    }
}

/// Start spec: `HH:MM` (today, local) or `now`.
pub fn parse_start(spec: &str) -> Result<i64> {
    if spec == "now" {
        return Ok(now_ms());
    }
    let (h, m) = parse_hh_mm(spec)?;
    let today = Local::now().date_naive();
    let naive = today.and_hms_opt(h, m, 0).context("invalid time")?;
    Ok(Local
        .from_local_datetime(&naive)
        .earliest()
        .context("time does not exist in the local timezone")?
        .timestamp_millis())
}

/// End spec: `HH:MM` (today) or `+90m`/`+2h`/`+1h30m` offset from `base`.
pub fn parse_end(spec: &str, base: i64) -> Result<i64> {
    match spec.strip_prefix('+') {
        Some(offset) => Ok(base + parse_offset(offset)?.num_milliseconds()),
        None => parse_start(spec),
    }
}

fn parse_hh_mm(spec: &str) -> Result<(u32, u32)> {
    let bad = || anyhow!("expected zero-padded HH:MM, `now`, or a +offset, got {spec:?}");
    let (h, m) = spec.split_once(':').ok_or_else(bad)?;
    let digits = |s: &str| s.len() == 2 && s.bytes().all(|b| b.is_ascii_digit());
    if !digits(h) || !digits(m) {
        return Err(bad());
    }
    let (h, m): (u32, u32) = (h.parse()?, m.parse()?);
    ensure!(h < 24 && m < 60, "time out of range: {spec:?}");
    Ok((h, m))
}

/// `90m`, `2h`, `1h30m` → Duration (minute granularity).
fn parse_offset(spec: &str) -> Result<Duration> {
    let mut minutes: i64 = 0;
    let mut rest = spec;
    while !rest.is_empty() {
        let digits = rest.len() - rest.trim_start_matches(|c: char| c.is_ascii_digit()).len();
        ensure!(digits > 0, "expected a number in +{spec:?}");
        let (num, tail) = rest.split_at(digits);
        let num: i64 = num
            .parse()
            .with_context(|| format!("bad number in +{spec:?}"))?;
        let unit = tail.chars().next().context("offset needs an m/h unit")?;
        match unit {
            'm' => minutes += num,
            'h' => minutes += num * 60,
            other => bail!("bad offset unit {other:?} in +{spec:?}"),
        }
        rest = &tail[1..];
    }
    ensure!(minutes > 0, "offset must be positive");
    Ok(Duration::minutes(minutes))
}

/// Stable kind color as gpui-ordered Hsla `[hue_turns, s, l, a]` — hue in
/// turns (0..1), fixed S/L for accessibility (decisions.md Q2).
pub fn kind_hsla(kind: &str) -> [f32; 4] {
    let mut hash: u32 = 0x811c_9dc5; // FNV-1a — stable across runs and versions
    for byte in kind.as_bytes() {
        hash ^= u32::from(*byte);
        hash = hash.wrapping_mul(0x0100_0193);
    }
    let hue_turns = (hash % PALETTE_SIZE as u32) as f32 / PALETTE_SIZE as f32;
    [hue_turns, 0.5, 0.6, 1.0]
}

/// WCAG black/white pick for text on `bg` (`[h, s, l, a]`) — sRGB luminance
/// ratio, the upperadd `contrast_text` approach. `bg` must be opaque.
pub fn contrast_text(bg: [f32; 4]) -> [f32; 4] {
    let (r, g, b) = hsl_to_rgb(bg[0], bg[1], bg[2]);
    let lin = |c: f32| {
        if c <= 0.04045 {
            c / 12.92
        } else {
            ((c + 0.055) / 1.055).powf(2.4)
        }
    };
    let lum = 0.2126 * lin(r) + 0.7152 * lin(g) + 0.0722 * lin(b);
    // ratio(white) = 1.05/(L+.05) vs ratio(black) = (L+.05)/.05
    if 1.05 / (lum + 0.05) >= (lum + 0.05) / 0.05 {
        [0.0, 0.0, 1.0, 1.0] // white
    } else {
        [0.0, 0.0, 0.0, 1.0] // black
    }
}

fn hsl_to_rgb(h: f32, s: f32, l: f32) -> (f32, f32, f32) {
    if s == 0.0 {
        return (l, l, l);
    }
    let q = if l < 0.5 {
        l * (1.0 + s)
    } else {
        l + s - l * s
    };
    let p = 2.0 * l - q;
    let hue = |mut t: f32| {
        if t < 0.0 {
            t += 1.0;
        }
        if t > 1.0 {
            t -= 1.0;
        }
        if t < 1.0 / 6.0 {
            p + (q - p) * 6.0 * t
        } else if t < 0.5 {
            q
        } else if t < 2.0 / 3.0 {
            p + (q - p) * (2.0 / 3.0 - t) * 6.0
        } else {
            p
        }
    };
    (hue(h + 1.0 / 3.0), hue(h), hue(h - 1.0 / 3.0))
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::FixedOffset;

    #[test]
    fn start_specs_parse() {
        let now = parse_start("now").unwrap();
        let h_m = parse_start("09:30").unwrap();
        assert!(h_m > now - 86_400_000); // today, not last year
        assert!(parse_start("9:30").is_err()); // zero-padded only
        assert!(parse_start("25:00").is_err());
        assert!(parse_start("noon").is_err());
    }

    #[test]
    fn end_specs_parse_relative_and_absolute() {
        let base = parse_start("08:00").unwrap();
        assert_eq!(parse_end("+90m", base).unwrap(), base + 90 * 60_000);
        assert_eq!(parse_end("+2h", base).unwrap(), base + 120 * 60_000);
        assert_eq!(parse_end("+1h30m", base).unwrap(), base + 90 * 60_000);
        assert_eq!(
            parse_end("09:30", base).unwrap(),
            parse_start("09:30").unwrap()
        );
        assert!(parse_end("+0m", base).is_err());
        assert!(parse_end("+5x", base).is_err());
    }

    #[test]
    fn day_and_week_windows_are_monday_based_and_contiguous() {
        // 2026-09-23 is a Wednesday (UTC).
        let tz = FixedOffset::east_opt(2 * 3600).unwrap();
        let at = tz.with_ymd_and_hms(2026, 9, 23, 15, 0, 0).unwrap();
        let (day_from, day_to) = day_window(at);
        assert_eq!(day_to - day_from, 86_400_000);
        let (week_from, week_to) = week_window(at);
        assert_eq!(week_to - week_from, 7 * 86_400_000);
        let monday = DateTime::from_timestamp_millis(week_from)
            .unwrap()
            .with_timezone(&tz);
        assert_eq!(monday.weekday(), chrono::Weekday::Mon);
        assert!(day_from >= week_from && day_to <= week_to);
    }

    #[test]
    fn clipped_ms_never_goes_negative() {
        let ev = Event::new("e", None, 1_000, 2_000);
        assert_eq!(ev.clipped_ms(0, 5_000), 1_000);
        assert_eq!(ev.clipped_ms(1_500, 5_000), 500);
        assert_eq!(ev.clipped_ms(2_000, 5_000), 0); // [from,to) excludes end
        assert_eq!(ev.clipped_ms(5_000, 9_000), 0);
    }

    #[test]
    fn validate_rejects_empty_and_inverted() {
        let mut ev = Event::new("ok", None, 1_000, 2_000);
        assert!(ev.validate().is_ok());
        ev.title = "  ".into();
        assert!(ev.validate().is_err());
        ev.title = "t".into();
        ev.kind = "".into();
        assert!(ev.validate().is_err());
        ev.kind = "work".into();
        ev.ends_utc = ev.starts_utc;
        assert!(ev.validate().is_err());
    }

    #[test]
    fn kind_color_is_stable_and_bucketed() {
        let a = kind_hsla("deep-work");
        let b = kind_hsla("deep-work");
        assert_eq!(a, b);
        assert_eq!(a[1], 0.5);
        assert_eq!(a[2], 0.6);
        let hue_bucket = (a[0] * PALETTE_SIZE as f32).round() as i64;
        assert!((0..PALETTE_SIZE as i64).contains(&hue_bucket));
    }

    #[test]
    fn contrast_text_picks_sides() {
        let white_bg = [0.0, 0.0, 1.0, 1.0];
        let black_bg = [0.0, 0.0, 0.0, 1.0];
        assert_eq!(contrast_text(white_bg), [0.0, 0.0, 0.0, 1.0]); // black on white
        assert_eq!(contrast_text(black_bg), [0.0, 0.0, 1.0, 1.0]); // white on black
    }
}
