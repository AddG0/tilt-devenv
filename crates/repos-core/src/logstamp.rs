//! Timestamps and levels for demuxed Tilt logs.
//!
//! `repos logs` splits one `tilt logs --json` stream into a file per resource and
//! lets lnav merge them back into one time-ordered view. Left to itself lnav picks
//! a single format per file from that file's opening lines, so a resource whose
//! build output is shaped differently from its runtime output gets read with the
//! wrong format and falls back to the file's mtime — and since lnav also clamps
//! each file's timestamps to be non-decreasing, one bad line at the head drags
//! every line after it.
//!
//! So the timestamp is decided here and handed to lnav out of band, in a JSON
//! field the log view never renders (see `logs_format.json`).
//!
//! Tilt's own `time` is the last resort rather than the obvious answer: it has
//! second resolution, and for replayed history it is the moment the client
//! attached rather than when the line was written.

use std::borrow::Cow;
use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use chrono::{DateTime, FixedOffset, NaiveDate, NaiveTime, TimeZone};
use serde::Serialize;

/// How long an untimed line waits for its resource's first timestamped line before
/// falling back to Tilt's time. A replayed backlog resolves well inside this; a live
/// build that logs nothing parseable stays responsive.
pub const HEAD_GRACE: Duration = Duration::from_millis(250);

/// How close behind a timestamped line an untimed one must arrive to count as a
/// continuation. Tilt delivers a message with its extra lines, so a real
/// continuation is milliseconds behind its header; a wider gap is a fresh block —
/// a rebuild's gradle chatter — that inheriting would back-date.
const CONTINUATION_WINDOW: Duration = Duration::from_millis(250);

/// Ships beside the writer it describes, so the two halves can't drift apart.
const LNAV_FORMAT: &str = include_str!("logs_format.json");

/// A log line with the timestamp lnav should order it by.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Stamped {
    pub ts: DateTime<FixedOffset>,
    /// An lnav level name, when the line states one.
    pub level: Option<&'static str>,
    pub line: String,
}

/// Assigns timestamps to demuxed log lines, one independent clock per resource.
///
/// The caller drives the clock: [`Stamper::push`] and [`Stamper::tick`] take `now`,
/// so the waits that decide a stamp are testable without sleeping.
#[derive(Debug)]
pub struct Stamper {
    grace: Duration,
    per_resource: HashMap<String, Resource>,
}

impl Default for Stamper {
    fn default() -> Stamper {
        Stamper::new()
    }
}

#[derive(Debug, Default)]
struct Resource {
    /// The stamp a continuation line inherits — sniffed from this resource's output,
    /// or Tilt's time where it has never written one.
    last_seen: Option<DateTime<FixedOffset>>,
    last_level: Option<&'static str>,
    /// Arrival time of `last_seen`: inheriting it is only sound for a line that came
    /// with the message above it.
    last_seen_at: Option<Instant>,
    /// Keeps the file non-decreasing, whatever the service's clock does.
    high_water: Option<DateTime<FixedOffset>>,
    /// Lines held because this resource hasn't logged a timestamp yet.
    head: Vec<Held>,
    held_since: Option<Instant>,
}

#[derive(Debug)]
struct Held {
    line: String,
    level: Option<&'static str>,
    tilt: DateTime<FixedOffset>,
}

impl Stamper {
    pub fn new() -> Stamper {
        Stamper::with_grace(HEAD_GRACE)
    }

    pub fn with_grace(grace: Duration) -> Stamper {
        Stamper {
            grace,
            per_resource: HashMap::new(),
        }
    }

    /// Stamps one physical line, returning the lines now ready to write for that
    /// resource — usually just this one, but a resource's first timestamped line
    /// releases everything held ahead of it, and a line held for a timestamp that
    /// never came returns nothing until [`Stamper::tick`] gives up on it.
    pub fn push(
        &mut self,
        resource: &str,
        line: String,
        tilt: DateTime<FixedOffset>,
        now: Instant,
    ) -> Vec<Stamped> {
        let state = self.per_resource.entry(resource.to_string()).or_default();
        let plain = strip_ansi(&line);
        let found = scan_timestamp(&plain, *tilt.offset());
        let level = match found {
            Some((_, body_at)) => scan_level(&plain, body_at),
            None => scan_level(&plain, 0),
        };

        match found {
            Some((ts, _)) => {
                state.remember(ts, level, now);
                let mut out = state.release_head(ts);
                out.push(state.emit(ts, level, line));
                out
            }
            // Arrived with the message above it, e.g. a stack trace under an ERROR.
            None if state.continues(now) => {
                let ts = state.last_seen.expect("continues() checked it");
                let level = level.or(state.last_level);
                vec![state.emit(ts, level, line)]
            }
            // A new block of untimed output. Tilt's time is accurate to the second for
            // a line we watch arrive; the last timestamp here may be minutes stale.
            None if state.last_seen.is_some() => {
                state.remember(tilt, level, now);
                vec![state.emit(tilt, level, line)]
            }
            None => {
                state.held_since.get_or_insert(now);
                state.head.push(Held { line, level, tilt });
                Vec::new()
            }
        }
    }

    /// Releases lines whose wait for a timestamp has run out, stamping them with
    /// Tilt's time. Call it as often as is convenient — it only acts on resources
    /// past the grace period.
    pub fn tick(&mut self, now: Instant) -> Vec<(String, Vec<Stamped>)> {
        let expired: Vec<String> = self
            .per_resource
            .iter()
            .filter(|(_, s)| {
                s.held_since
                    .is_some_and(|at| now.duration_since(at) >= self.grace)
            })
            .map(|(name, _)| name.clone())
            .collect();
        self.give_up_on(expired)
    }

    /// Releases everything still held, for when the stream ends.
    pub fn drain(&mut self) -> Vec<(String, Vec<Stamped>)> {
        let held: Vec<String> = self.per_resource.keys().cloned().collect();
        self.give_up_on(held)
    }

    fn give_up_on(&mut self, resources: Vec<String>) -> Vec<(String, Vec<Stamped>)> {
        let mut out = Vec::new();
        for name in resources {
            let Some(state) = self.per_resource.get_mut(&name) else {
                continue;
            };
            let flushed = state.flush_head_with_tilt_time();
            if !flushed.is_empty() {
                out.push((name, flushed));
            }
        }
        out
    }
}

impl Resource {
    fn remember(&mut self, ts: DateTime<FixedOffset>, level: Option<&'static str>, now: Instant) {
        self.last_seen = Some(ts);
        self.last_level = level;
        self.last_seen_at = Some(now);
    }

    /// Whether an untimed line arriving `now` belongs to the message above it.
    fn continues(&self, now: Instant) -> bool {
        self.last_seen.is_some()
            && self
                .last_seen_at
                .is_some_and(|at| now.duration_since(at) < CONTINUATION_WINDOW)
    }

    /// Stamps a line, never going backwards: lnav would clamp a backwards step
    /// itself, and does it by discarding the real timestamp, so we keep the
    /// decision here.
    fn emit(
        &mut self,
        ts: DateTime<FixedOffset>,
        level: Option<&'static str>,
        line: String,
    ) -> Stamped {
        let ts = match self.high_water {
            Some(high) if high > ts => high,
            _ => ts,
        };
        self.high_water = Some(ts);
        Stamped { ts, level, line }
    }

    /// Back-dates lines held ahead of the first timestamped line to that timestamp,
    /// so they sort just above it rather than at whatever time the stream reached us.
    fn release_head(&mut self, ts: DateTime<FixedOffset>) -> Vec<Stamped> {
        self.held_since = None;
        let head = std::mem::take(&mut self.head);
        head.into_iter()
            .map(|h| self.emit(ts, h.level, h.line))
            .collect()
    }

    fn flush_head_with_tilt_time(&mut self) -> Vec<Stamped> {
        self.held_since = None;
        let head = std::mem::take(&mut self.head);
        let flushed: Vec<Stamped> = head
            .into_iter()
            .map(|h| self.emit(h.tilt, h.level, h.line))
            .collect();
        // `last_seen_at` deliberately stays unset: nothing parseable has been seen, so
        // later lines should keep taking Tilt's time rather than inherit a guess.
        if let Some(last) = flushed.last() {
            self.last_seen = Some(last.ts);
            self.last_level = last.level;
        }
        flushed
    }
}

/// The per-resource log files lnav opens, written as JSON records so the
/// timestamp can travel beside the line instead of inside it.
pub struct Sink {
    dir: PathBuf,
    files: HashMap<String, File>,
}

impl Sink {
    /// `dir` is what the format's `file-pattern` matches, so it has to be the `logs`
    /// subdirectory of a `repos-logs-*` temp dir, and the caller has to create it.
    pub fn new(dir: PathBuf) -> Sink {
        Sink {
            dir,
            files: HashMap::new(),
        }
    }

    pub fn write(&mut self, resource: &str, lines: &[Stamped]) -> Result<()> {
        if lines.is_empty() {
            return Ok(());
        }
        // One write for the whole batch: lnav reads the file as we append, and half a
        // JSON object is a parse error where half a line of text was merely ugly.
        let mut batch = String::new();
        for line in lines {
            batch.push_str(&record(line));
            batch.push('\n');
        }
        let file = self.file_for(resource)?;
        file.write_all(batch.as_bytes())
            .with_context(|| format!("writing {resource} log"))?;
        Ok(())
    }

    fn file_for(&mut self, resource: &str) -> Result<&mut File> {
        if !self.files.contains_key(resource) {
            let path = self.dir.join(resource);
            let file = OpenOptions::new()
                .create(true)
                .append(true)
                .open(&path)
                .with_context(|| format!("opening {}", path.display()))?;
            self.files.insert(resource.to_string(), file);
        }
        Ok(self.files.get_mut(resource).expect("just inserted"))
    }
}

/// One line as the lnav format reads it. `tilt_ts` is kept out of the format's
/// `line-format`, so it orders the view without appearing in it.
#[derive(Serialize)]
struct Record<'a> {
    tilt_ts: String,
    /// Left out rather than written as null when the line states no level, so lnav
    /// treats it as unknown instead of parsing a null.
    #[serde(skip_serializing_if = "Option::is_none")]
    tilt_level: Option<&'static str>,
    tilt_msg: &'a str,
}

fn record(line: &Stamped) -> String {
    let record = Record {
        tilt_ts: line.ts.format("%Y-%m-%dT%H:%M:%S%.6f%z").to_string(),
        tilt_level: line.level,
        tilt_msg: &line.line,
    };
    serde_json::to_string(&record).expect("a log record is always serialisable")
}

/// Installs the format under `root`, returning the directory to pass to `lnav -I`.
pub fn install_lnav_format(root: &Path) -> Result<PathBuf> {
    let config = root.join("lnav");
    let formats = config.join("formats").join("tilt-demux");
    fs::create_dir_all(&formats).with_context(|| format!("creating {}", formats.display()))?;
    let path = formats.join("format.json");
    fs::write(&path, LNAV_FORMAT).with_context(|| format!("writing {}", path.display()))?;
    Ok(config)
}

/// A Tilt message split into the lines lnav will show: Tilt terminates each message
/// with a newline and may pack several lines into one entry (a stack trace, a proto
/// dump), and every one of them needs its own stamped record.
pub fn physical_lines(message: &str) -> impl Iterator<Item = &str> {
    message.strip_suffix('\n').unwrap_or(message).split('\n')
}

/// Several services write colour codes *before* their timestamp
/// (`\x1b[30m2026-08-14T…`), where they would hide it from the scan.
fn strip_ansi(line: &str) -> Cow<'_, str> {
    if !line.contains('\x1b') {
        return Cow::Borrowed(line);
    }
    let mut out = String::with_capacity(line.len());
    let mut rest = line;
    while let Some(esc) = rest.find('\x1b') {
        out.push_str(&rest[..esc]);
        let after = &rest[esc..];
        // CSI (`ESC [ … final`) covers the colour codes we see; anything else is
        // dropped one byte at a time so a stray ESC can't stall the scan.
        rest = match after.strip_prefix("\x1b[") {
            Some(params) => match params.find(|c: char| c.is_ascii_alphabetic()) {
                Some(end) => &params[end + 1..],
                None => "",
            },
            None => &after[1..],
        };
    }
    out.push_str(rest);
    Cow::Owned(out)
}

/// The timestamp a service wrote at the start of a line, plus where the rest of
/// the line begins.
///
/// One scanner covers every shape seen in practice, which differ only in their
/// separators: Ruby's `I, [2026-08-14T09:14:27.217696 #1]`, ISO with an offset or
/// `Z`, logback's comma milliseconds, pino's `[… -0500]` and Caddy's `2026/08/14`.
/// A service that states no offset is read in `fallback`.
fn scan_timestamp(line: &str, fallback: FixedOffset) -> Option<(DateTime<FixedOffset>, usize)> {
    let b = line.as_bytes();
    let mut i = 0;
    while matches!(b.get(i), Some(b' ' | b'\t')) {
        i += 1;
    }
    // Ruby Logger's severity prefix, e.g. `I, [`.
    if matches!(b.get(i), Some(c) if c.is_ascii_uppercase())
        && b.get(i + 1) == Some(&b',')
        && b.get(i + 2) == Some(&b' ')
    {
        i += 3;
    }
    if b.get(i) == Some(&b'[') {
        i += 1;
    }

    let year = digits(b, i, 4)?;
    let date_sep = *b.get(i + 4)?;
    if date_sep != b'-' && date_sep != b'/' {
        return None;
    }
    let month = digits(b, i + 5, 2)?;
    if *b.get(i + 7)? != date_sep {
        return None;
    }
    let day = digits(b, i + 8, 2)?;
    i += 10;

    if !matches!(b.get(i), Some(b'T' | b' ')) {
        return None;
    }
    i += 1;

    let hour = digits(b, i, 2)?;
    if *b.get(i + 2)? != b':' {
        return None;
    }
    let min = digits(b, i + 3, 2)?;
    if *b.get(i + 5)? != b':' {
        return None;
    }
    let sec = digits(b, i + 6, 2)?;
    i += 8;

    let mut nanos = 0;
    if matches!(b.get(i), Some(b'.' | b',')) {
        let start = i + 1;
        let mut end = start;
        while end < b.len() && b[end].is_ascii_digit() && end - start < 9 {
            end += 1;
        }
        if end == start {
            return None;
        }
        nanos = digits(b, start, end - start)? * 10u32.pow((9 - (end - start)) as u32);
        i = end;
    }

    let (offset, after_offset) = scan_offset(b, i).unwrap_or((fallback, i));

    let date = NaiveDate::from_ymd_opt(year as i32, month, day)?;
    let time = NaiveTime::from_hms_nano_opt(hour, min, sec, nanos)?;
    let ts = offset.from_local_datetime(&date.and_time(time)).single()?;
    Some((ts, after_offset))
}

/// A `Z`, `+HH:MM` or `+HHMM` offset at `i`, allowing the single space pino puts
/// in front of it. Returns where the offset ends.
fn scan_offset(b: &[u8], i: usize) -> Option<(FixedOffset, usize)> {
    let start = if b.get(i) == Some(&b' ') { i + 1 } else { i };
    match b.get(start) {
        Some(b'Z') => Some((FixedOffset::east_opt(0)?, start + 1)),
        Some(sign @ (b'+' | b'-')) => {
            let sign = if *sign == b'-' { -1 } else { 1 };
            let hours = digits(b, start + 1, 2)?;
            let (mins, end) = match b.get(start + 3) {
                Some(b':') => (digits(b, start + 4, 2)?, start + 6),
                _ => (digits(b, start + 3, 2)?, start + 5),
            };
            let secs = sign * (hours * 3600 + mins * 60) as i32;
            Some((FixedOffset::east_opt(secs)?, end))
        }
        _ => None,
    }
}

fn digits(b: &[u8], at: usize, len: usize) -> Option<u32> {
    let slice = b.get(at..at + len)?;
    if !slice.iter().all(u8::is_ascii_digit) {
        return None;
    }
    std::str::from_utf8(slice).ok()?.parse().ok()
}

/// The level a line states, in lnav's vocabulary, so `:set-min-log-level` and the
/// error histogram work uniformly across services that each spell theirs differently.
///
/// Only the first few fields after the timestamp count, so prose and paths that
/// happen to contain "error" don't colour a line.
fn scan_level(line: &str, body_at: usize) -> Option<&'static str> {
    // Ruby Logger states it as a leading severity letter.
    let head = line.trim_start().as_bytes();
    if head.len() > 2
        && head[1] == b','
        && head[2] == b' '
        && let Some(level) = ruby_severity(head[0])
    {
        return Some(level);
    }
    line.get(body_at..)?
        .split_whitespace()
        .take(3)
        .find_map(|token| level_word(token.trim_matches(|c: char| !c.is_alphabetic())))
}

fn ruby_severity(letter: u8) -> Option<&'static str> {
    match letter {
        b'D' => Some("debug"),
        b'I' => Some("info"),
        b'W' => Some("warning"),
        b'E' => Some("error"),
        b'F' => Some("critical"),
        _ => None,
    }
}

fn level_word(token: &str) -> Option<&'static str> {
    // lnav's own level names, so the format needs no mapping table.
    match token.to_ascii_lowercase().as_str() {
        "trace" => Some("trace"),
        "debug" | "verbose" => Some("debug"),
        "info" | "notice" => Some("info"),
        "warn" | "warning" => Some("warning"),
        "error" | "severe" => Some("error"),
        "critical" | "fatal" | "panic" => Some("critical"),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn offset() -> FixedOffset {
        FixedOffset::west_opt(5 * 3600).unwrap()
    }

    /// Tilt's own timestamp for a line: second resolution, machine offset.
    fn tilt(hms: &str) -> DateTime<FixedOffset> {
        DateTime::parse_from_rfc3339(&format!("2026-08-14T{hms}-05:00")).unwrap()
    }

    fn ts_of(line: &str) -> String {
        let (ts, _) = scan_timestamp(&strip_ansi(line), offset()).expect("should find a timestamp");
        ts.to_rfc3339()
    }

    fn level_of(line: &str) -> Option<&'static str> {
        let plain = strip_ansi(line);
        let body_at = scan_timestamp(&plain, offset()).map_or(0, |(_, at)| at);
        scan_level(&plain, body_at)
    }

    fn stamp(s: &mut Stamper, resource: &str, line: &str, at: &str) -> Vec<Stamped> {
        s.push(resource, line.to_string(), tilt(at), Instant::now())
    }

    #[test]
    fn should_read_the_ruby_logger_timestamp_in_the_machines_offset() {
        assert_eq!(
            ts_of("I, [2026-08-14T09:14:27.217696 #277416]  INFO -- : handled Health/Check"),
            "2026-08-14T09:14:27.217696-05:00"
        );
    }

    #[test]
    fn should_honour_an_offset_the_service_states_itself() {
        assert_eq!(
            ts_of("2026-08-14T09:22:48.099563-0500 [info     ] 2 changes detected"),
            "2026-08-14T09:22:48.099563-05:00"
        );
    }

    #[test]
    fn should_read_a_utc_timestamp_as_utc_rather_than_local() {
        assert_eq!(
            ts_of("\x1b[2m2026-08-14T14:17:39.154860Z\x1b[0m [\x1b[32minfo\x1b[0m] 2 changes"),
            "2026-08-14T14:17:39.154860+00:00"
        );
    }

    #[test]
    fn should_read_logback_comma_milliseconds_behind_a_colour_code() {
        assert_eq!(
            ts_of("\x1b[30m2026-08-14T09:17:39,641\x1b[m \x1b[32mINFO  \x1b[m[grpc-executor-0] hi"),
            "2026-08-14T09:17:39.641-05:00"
        );
    }

    #[test]
    fn should_read_a_bracketed_pino_timestamp_with_a_spaced_offset() {
        assert_eq!(
            ts_of("[2026-08-14 09:17:37.232 -0500] \x1b[34mDEBUG\x1b[39m (275168):"),
            "2026-08-14T09:17:37.232-05:00"
        );
    }

    #[test]
    fn should_read_caddys_slash_separated_date() {
        assert_eq!(
            ts_of("2026/08/14 09:17:47.171\tDEBUG\thttp.handlers.reverse_proxy\tselected upstream"),
            "2026-08-14T09:17:47.171-05:00"
        );
    }

    #[test]
    fn should_find_no_timestamp_in_build_output_or_continuation_lines() {
        let untimed = [
            "  error: error reading /build/generated/foo.java",
            "    module: \"grpc\"",
            "Running cmd: direnv exec . bash -c 'gradle -t classes'",
            "  at async Object.unary (/app/src/server.ts:12:3)",
            "",
        ];
        for line in untimed {
            assert!(
                scan_timestamp(&strip_ansi(line), offset()).is_none(),
                "should not read a timestamp from {line:?}"
            );
        }
    }

    #[test]
    fn should_not_mistake_a_path_or_prose_date_for_a_line_timestamp() {
        assert!(
            scan_timestamp(
                &strip_ansi("wrote /var/log/2026-08-14T09:14:27.000000/report.txt"),
                offset()
            )
            .is_none()
        );
    }

    #[test]
    fn should_take_the_level_from_each_services_own_spelling() {
        assert_eq!(
            level_of("I, [2026-08-14T09:14:27.217696 #277416]  INFO -- : handled Health/Check"),
            Some("info")
        );
        assert_eq!(
            level_of("E, [2026-08-14T09:14:27.217696 #277416] ERROR -- : boom"),
            Some("error")
        );
        assert_eq!(
            level_of("\x1b[30m2026-08-14T09:17:39,641\x1b[m \x1b[36mDEBUG \x1b[m[exec-0] hi"),
            Some("debug")
        );
        assert_eq!(
            level_of("[2026-08-14 09:17:37.232 -0500] \x1b[34mWARN\x1b[39m (275168): slow"),
            Some("warning")
        );
        assert_eq!(
            level_of("2026-08-14T09:22:48.099563-0500 [info     ] 2 changes detected"),
            Some("info")
        );
        // Gradle writes it lowercase and without a timestamp at all.
        assert_eq!(
            level_of("  error: error reading /build/generated/foo.java"),
            Some("error")
        );
    }

    #[test]
    fn should_leave_prose_and_paths_unlevelled() {
        assert_eq!(level_of("    module: \"grpc\""), None);
        assert_eq!(
            level_of("2026/08/14 09:17:47.171\tselected upstream\t{\"to\": \"/error/handler\"}"),
            None
        );
        assert_eq!(
            level_of("Export NAV_COUNT_ENTITIES doesn't exist in target module"),
            None
        );
    }

    #[test]
    fn should_stamp_a_timestamped_line_with_its_own_time() {
        let mut s = Stamper::new();
        let out = stamp(
            &mut s,
            "backend",
            "I, [2026-08-14T09:14:27.217696 #1]  INFO -- : hi",
            "09:14:30",
        );
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].ts.to_rfc3339(), "2026-08-14T09:14:27.217696-05:00");
        assert_eq!(out[0].level, Some("info"));
    }

    #[test]
    fn should_give_a_continuation_line_the_timestamp_and_level_above_it() {
        let mut s = Stamper::new();
        stamp(
            &mut s,
            "frontend",
            "[2026-08-14 09:17:37.232 -0500] ERROR (1): request failed",
            "09:17:40",
        );
        let out = stamp(
            &mut s,
            "frontend",
            "    at async Object.unary (server.ts:12)",
            "09:17:40",
        );
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].ts.to_rfc3339(), "2026-08-14T09:17:37.232-05:00");
        assert_eq!(out[0].level, Some("error"));
    }

    #[test]
    fn should_back_date_held_build_output_to_the_first_timestamped_line() {
        // A resource can open with untimed build errors and then log real timestamps
        // from an hour before the stream was attached.
        let mut s = Stamper::new();
        assert!(
            stamp(
                &mut s,
                "svc",
                "  error: error reading /build/a.java",
                "10:20:35"
            )
            .is_empty()
        );
        assert!(
            stamp(
                &mut s,
                "svc",
                "  error: error reading /build/b.java",
                "10:20:35"
            )
            .is_empty()
        );
        let out = stamp(
            &mut s,
            "svc",
            "2026-08-14T09:14:53,560 INFO  [main] started",
            "10:20:35",
        );
        let stamps: Vec<String> = out.iter().map(|s| s.ts.to_rfc3339()).collect();
        assert_eq!(
            stamps,
            [
                "2026-08-14T09:14:53.560-05:00",
                "2026-08-14T09:14:53.560-05:00",
                "2026-08-14T09:14:53.560-05:00"
            ],
            "held lines should sort with the line that revealed the time, not at attach time"
        );
    }

    #[test]
    fn should_fall_back_to_tilt_time_once_the_wait_for_a_timestamp_runs_out() {
        let mut s = Stamper::with_grace(Duration::from_millis(250));
        let start = Instant::now();
        assert!(
            s.push(
                "gradle-wrappers",
                "Running cmd: gradle wrapper".into(),
                tilt("10:20:35"),
                start
            )
            .is_empty()
        );
        assert!(
            s.tick(start + Duration::from_millis(100)).is_empty(),
            "should still be waiting"
        );

        let flushed = s.tick(start + Duration::from_millis(300));
        assert_eq!(flushed.len(), 1);
        let (resource, lines) = &flushed[0];
        assert_eq!(resource, "gradle-wrappers");
        assert_eq!(lines[0].ts.to_rfc3339(), "2026-08-14T10:20:35-05:00");
    }

    #[test]
    fn should_stamp_later_lines_of_a_never_timestamped_resource_from_tilt_time() {
        let mut s = Stamper::with_grace(Duration::from_millis(0));
        let start = Instant::now();
        s.push(
            "git-status",
            "Running cmd: repos status --watch".into(),
            tilt("10:20:35"),
            start,
        );
        s.tick(start);
        let out = s.push(
            "git-status",
            "  M Caddyfile".into(),
            tilt("10:20:36"),
            start,
        );
        assert_eq!(
            out.len(),
            1,
            "a resource with no timestamps must still reach the log view"
        );
        assert_eq!(
            out[0].ts.to_rfc3339(),
            "2026-08-14T10:20:36-05:00",
            "it should keep taking Tilt's time rather than freeze at the first flush"
        );
    }

    #[test]
    fn should_not_back_date_untimed_output_that_arrives_after_a_pause() {
        // A restart prints untimed build chatter seconds after the service's last
        // timestamped line; inheriting that stamp files it above everything since.
        let mut s = Stamper::new();
        let start = Instant::now();
        s.push(
            "auth-service",
            "2026-08-14T11:15:45,274 INFO  [grpc-executor-0] before the restart".into(),
            tilt("11:15:45"),
            start,
        );

        let out = s.push(
            "auth-service",
            "BUILD SUCCESSFUL in 512ms".into(),
            tilt("11:15:50"),
            start + Duration::from_secs(5),
        );

        assert_eq!(
            out[0].ts.to_rfc3339(),
            "2026-08-14T11:15:50-05:00",
            "a fresh block of untimed output belongs where it was emitted"
        );
    }

    #[test]
    fn should_still_group_a_multi_line_message_arriving_all_at_once() {
        let mut s = Stamper::new();
        let start = Instant::now();
        s.push(
            "frontend",
            "[2026-08-14 11:15:45.100 -0500] ERROR (1): request failed".into(),
            tilt("11:15:45"),
            start,
        );

        let out = s.push(
            "frontend",
            "    at async Object.unary (server.ts:12)".into(),
            tilt("11:15:45"),
            start + Duration::from_millis(2),
        );

        assert_eq!(out[0].ts.to_rfc3339(), "2026-08-14T11:15:45.100-05:00");
        assert_eq!(out[0].level, Some("error"));
    }

    #[test]
    fn should_release_held_lines_when_the_stream_ends_without_a_timestamp() {
        let mut s = Stamper::new();
        s.push(
            "gradle-wrappers",
            "Initial Build".into(),
            tilt("10:20:35"),
            Instant::now(),
        );
        let flushed = s.drain();
        assert_eq!(flushed.len(), 1);
        assert_eq!(flushed[0].1.len(), 1);
    }

    #[test]
    fn should_never_stamp_a_resource_backwards() {
        let mut s = Stamper::new();
        let late = stamp(
            &mut s,
            "svc",
            "I, [2026-08-14T09:50:20.000000 #1]  INFO -- : t=20",
            "09:50:20",
        );
        let back = stamp(
            &mut s,
            "svc",
            "I, [2026-08-14T09:50:05.000000 #1]  INFO -- : t=05",
            "09:50:20",
        );
        assert_eq!(late[0].ts.to_rfc3339(), "2026-08-14T09:50:20-05:00");
        assert_eq!(
            back[0].ts.to_rfc3339(),
            "2026-08-14T09:50:20-05:00",
            "a backwards jump holds at the high-water mark"
        );
    }

    /// Feeds lines through the real writer and asks lnav — the actual consumer —
    /// what order it puts them in, so the format and the stamper are checked
    /// against each other rather than against our assumptions.
    ///
    /// Needs `lnav` on PATH, as the crate's other tests need `git`.
    fn merged_order(lines: &[(&str, &str, &str)]) -> Vec<String> {
        let dir = tempfile::Builder::new()
            .prefix("repos-logs-")
            .tempdir()
            .expect("temp dir");
        let logs = dir.path().join("logs");
        std::fs::create_dir(&logs).expect("logs dir");
        let config = install_lnav_format(dir.path()).expect("lnav format");

        let mut stamper = Stamper::new();
        let mut sink = Sink::new(logs.clone());
        let mut paths = std::collections::BTreeSet::new();
        for (resource, tilt_time, line) in lines {
            let ready = stamper.push(
                resource,
                (*line).to_string(),
                tilt(tilt_time),
                Instant::now(),
            );
            sink.write(resource, &ready).expect("write");
            paths.insert(logs.join(resource));
        }
        for (resource, ready) in stamper.drain() {
            sink.write(&resource, &ready).expect("write");
        }

        let out = dir.path().join("order.json");
        // lnav falls back to `~/.config` for an XDG_CONFIG_HOME that doesn't exist.
        let lnav_config = dir.path().join("lnav-config");
        std::fs::create_dir_all(&lnav_config).expect("lnav config dir");

        let status = std::process::Command::new("lnav")
            // `-I` adds a format directory rather than replacing the developer's
            // own, where a format from another lnav version fails its SQL
            // compiler — and this test with it.
            .env("XDG_CONFIG_HOME", &lnav_config)
            .arg("-I")
            .arg(&config)
            .arg("-n")
            .arg("-c")
            .arg(";SELECT log_body FROM all_logs")
            .arg("-c")
            .arg(format!(":write-json-to {}", out.display()))
            .args(&paths)
            .output()
            .expect("lnav should be on PATH, as git is for the other tests");
        assert!(
            status.status.success(),
            "lnav failed: {}",
            String::from_utf8_lossy(&status.stderr)
        );

        let exported = std::fs::read_to_string(&out).expect("lnav should have written the export");
        let rows: Vec<HashMap<String, String>> =
            serde_json::from_str(&exported).expect("lnav writes a JSON array of rows");
        rows.into_iter()
            .filter_map(|mut r| r.remove("log_body"))
            .collect()
    }

    #[test]
    fn should_interleave_services_by_their_own_timestamps_not_the_order_tilt_replayed_them() {
        // Tilt replays each resource's history in one burst, so the files are written
        // service by service.
        let order = merged_order(&[
            (
                "backend",
                "10:20:35",
                "I, [2026-08-14T10:14:37.233566 #1]  INFO -- : backend first",
            ),
            (
                "backend",
                "10:20:35",
                "I, [2026-08-14T10:14:39.100000 #1]  INFO -- : backend third",
            ),
            (
                "frontend",
                "10:20:35",
                "[2026-08-14 10:14:38.000 -0500] DEBUG (1): frontend second",
            ),
            (
                "frontend",
                "10:20:35",
                "[2026-08-14 10:14:40.000 -0500] DEBUG (1): frontend fourth",
            ),
        ]);
        let tails: Vec<&str> = order
            .iter()
            .map(|line| line.rsplit(' ').next().unwrap_or_default())
            .collect();
        assert_eq!(tails, ["first", "second", "third", "fourth"]);
    }

    #[test]
    fn should_not_let_untimed_build_output_strand_a_service_at_the_end_of_the_view() {
        // The original bug: untimed build errors at the head made lnav date the whole
        // file from the filesystem, sorting an hour of history after everything.
        let order = merged_order(&[
            (
                "backend",
                "10:20:35",
                "  error: error reading /build/generated/a.java",
            ),
            (
                "backend",
                "10:20:35",
                "2026-08-14T09:14:53,560 INFO  [main] backend started",
            ),
            (
                "frontend",
                "10:20:35",
                "[2026-08-14 10:14:38.000 -0500] DEBUG (1): frontend much later",
            ),
        ]);
        assert_eq!(
            order,
            [
                "  error: error reading /build/generated/a.java",
                "2026-08-14T09:14:53,560 INFO  [main] backend started",
                "[2026-08-14 10:14:38.000 -0500] DEBUG (1): frontend much later"
            ],
            "the 09:14 history must sort before the 10:14 line, build noise included"
        );
    }

    #[test]
    fn should_order_one_services_mixed_log_shapes_against_each_other() {
        // A Rails service writes three shapes into one stream: untimed build output,
        // Sidekiq's UTC `Z` lines, and the Ruby logger's naive local ones. One format for
        // the file — all lnav can manage alone — gets one of them wrong.
        let order = merged_order(&[
            ("backend", "11:07:28", "Running cmd: bundle exec puma"),
            (
                "backend",
                "11:07:28",
                "2026-08-14T16:02:18.081Z pid=1513049 tid=wm45 INFO: Sidekiq 7.3.5 connecting to Redis",
            ),
            (
                "backend",
                "11:07:28",
                "I, [2026-08-14T11:02:18.477918 #1513049]  INFO -- : handled Health/Check",
            ),
            (
                "frontend",
                "11:07:28",
                "[2026-08-14 11:02:18.300 -0500] DEBUG (1): frontend between the two",
            ),
        ]);
        let marks: Vec<&str> = order
            .iter()
            .map(|line| line.rsplit(' ').next().unwrap_or_default())
            .collect();
        assert_eq!(
            marks,
            ["puma", "Redis", "two", "Health/Check"],
            "Sidekiq's 16:02:18.081Z is 11:02:18.081 local, so the frontend line at \
             11:02:18.300 belongs between it and the 11:02:18.477 Ruby logger line"
        );
    }

    #[test]
    fn should_render_only_the_service_line_so_the_timestamp_is_not_shown_twice() {
        let order = merged_order(&[(
            "backend",
            "10:20:35",
            "I, [2026-08-14T10:14:37.233566 #277416]  INFO -- : handled Health/Check status=OK",
        )]);
        assert_eq!(
            order,
            ["I, [2026-08-14T10:14:37.233566 #277416]  INFO -- : handled Health/Check status=OK"],
            "lnav should show the service's line verbatim, with no stamp of ours added"
        );
    }

    #[test]
    fn should_keep_each_resources_clock_independent() {
        let mut s = Stamper::new();
        stamp(
            &mut s,
            "a",
            "I, [2026-08-14T09:50:20.000000 #1]  INFO -- : a",
            "09:50:20",
        );
        let b = stamp(
            &mut s,
            "b",
            "I, [2026-08-14T09:50:05.000000 #1]  INFO -- : b",
            "09:50:20",
        );
        assert_eq!(
            b[0].ts.to_rfc3339(),
            "2026-08-14T09:50:05-05:00",
            "one resource's high-water mark must not drag another's forward"
        );
    }
}
