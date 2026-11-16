//! Deciding when to replace a language server process
//!
//! Some servers (haskell-language-server) leak memory for as long as they
//! live. lspmux can replace the process behind its connected clients, the
//! recycle itself is in [`crate::instance`], this module holds what's pure:
//! reading the numbers and deciding.
//!
//! Two kinds of numbers are watched: the live heap bytes the server logs on
//! stderr (`Live bytes: 3514.31MB Heap size: 7528.78MB`) and the process'
//! `VmRSS + VmSwap` from `/proc/<pid>/status`.

use std::collections::VecDeque;
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};

use crate::config::RecycleConfig;

/// A soft recycle doesn't run within this long after the previous recycle
pub const SOFT_INTERVAL: Duration = Duration::from_secs(5 * 60);

/// Forced recycles are counted within this window
pub const FORCED_WINDOW: Duration = Duration::from_secs(15 * 60);

/// An instance stops recycling when this many forced recycles happened
/// within [`FORCED_WINDOW`]
pub const FORCED_LIMIT: usize = 3;

/// A server's heap statistics line
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct HeapStats {
    pub live_mb: f64,
    pub heap_mb: f64,
}

/// A number followed by `MB` at the start of `input`, and the rest
fn take_megabytes(input: &str) -> Option<(f64, &str)> {
    let input = input.trim_start();
    let end = input.find(|c: char| !(c.is_ascii_digit() || c == '.'))?;
    let value = input[..end].parse::<f64>().ok()?;
    let rest = input[end..].strip_prefix("MB")?;
    if rest.starts_with(|c: char| c.is_ascii_alphanumeric()) {
        return None;
    }
    Some((value, rest))
}

/// Parse `Live bytes: <n>MB Heap size: <n>MB` anywhere in a stderr `line`
///
/// Other units or anything else yields `None`.
pub fn parse_heap_line(line: &str) -> Option<HeapStats> {
    let start = line.find("Live bytes:")? + "Live bytes:".len();
    let (live_mb, rest) = take_megabytes(&line[start..])?;
    let rest = rest.trim_start().strip_prefix("Heap size:")?;
    let (heap_mb, _) = take_megabytes(rest)?;
    Some(HeapStats { live_mb, heap_mb })
}

/// Resident and swapped memory of a process
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProcMemory {
    pub rss_kb: u64,
    pub swap_kb: u64,
}

impl ProcMemory {
    /// `VmRSS + VmSwap` in MB
    pub fn total_mb(&self) -> f64 {
        (self.rss_kb + self.swap_kb) as f64 / 1024.0
    }
}

/// Parse the text of `/proc/<pid>/status`
///
/// `None` without `VmRSS` (kernel threads, zombies). A missing `VmSwap` (no
/// swap support) is 0.
pub fn parse_proc_status(text: &str) -> Option<ProcMemory> {
    fn kilobytes(text: &str, key: &str) -> Option<u64> {
        text.lines().find_map(|line| {
            line.strip_prefix(key)?
                .trim()
                .strip_suffix("kB")?
                .trim()
                .parse()
                .ok()
        })
    }
    Some(ProcMemory {
        rss_kb: kilobytes(text, "VmRSS:")?,
        swap_kb: kilobytes(text, "VmSwap:").unwrap_or(0),
    })
}

/// Memory of a process, `None` when it can't be read
pub fn read_proc_memory(pid: u32) -> Option<ProcMemory> {
    parse_proc_status(&std::fs::read_to_string(format!("/proc/{pid}/status")).ok()?)
}

/// Latest numbers of the current server process, in MB
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Sample {
    pub live_mb: Option<f64>,
    pub rss_mb: Option<f64>,
}

/// How urgent a recycle is
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    /// At the next settled moment
    Soft,
    /// Immediately
    Forced,
}

/// Which limit of `config` the `sample` exceeds
pub fn level(config: &RecycleConfig, sample: &Sample) -> Option<Level> {
    let reaches = |value: Option<f64>, limit: Option<u64>| match (value, limit) {
        (Some(value), Some(limit)) => value >= limit as f64,
        _ => false,
    };
    if reaches(sample.live_mb, config.forced_live_mb) || reaches(sample.rss_mb, config.max_memory_mb)
    {
        Some(Level::Forced)
    } else if reaches(sample.live_mb, config.soft_live_mb) {
        Some(Level::Soft)
    } else {
        None
    }
}

/// What to do about the latest numbers
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    /// Within the limits
    Idle,
    /// Over the soft limit, waiting for a settled moment
    Pending,
    /// Over the soft limit and settled, but the previous recycle is too
    /// recent
    Held,
    /// Recycle now at a settled moment
    Soft,
    /// Recycle now
    Forced,
    /// Recycling was given up for this instance
    Stopped,
}

/// Recycle history of one instance, protects against recycle loops
#[derive(Debug, Default)]
pub struct Guard {
    last_recycle: Option<Instant>,
    forced: VecDeque<Instant>,
    stopped: bool,
}

impl Guard {
    #[cfg(test)]
    fn stopped(&self) -> bool {
        self.stopped
    }

    /// Decide about a `level` reached by the latest numbers
    ///
    /// `settled` is the settled moment of a soft recycle: nothing in flight,
    /// no server progress, and the server quiet for `settle_ms`.
    pub fn decide(&self, level: Option<Level>, settled: bool, now: Instant) -> Decision {
        let Some(level) = level else {
            return Decision::Idle;
        };
        if self.stopped {
            return Decision::Stopped;
        }
        match level {
            Level::Forced => Decision::Forced,
            Level::Soft if !settled => Decision::Pending,
            Level::Soft => {
                let recent = self
                    .last_recycle
                    .is_some_and(|last| now.saturating_duration_since(last) < SOFT_INTERVAL);
                if recent {
                    Decision::Held
                } else {
                    Decision::Soft
                }
            }
        }
    }

    /// Record a recycle which ran at `now`, returns `true` when it was the
    /// last one this instance gets
    pub fn record(&mut self, level: Level, now: Instant) -> bool {
        self.last_recycle = Some(now);
        if level == Level::Forced {
            self.forced.push_back(now);
        }
        while self
            .forced
            .front()
            .is_some_and(|&at| now.saturating_duration_since(at) > FORCED_WINDOW)
        {
            self.forced.pop_front();
        }
        if self.forced.len() >= FORCED_LIMIT && !self.stopped {
            self.stopped = true;
            return true;
        }
        false
    }
}

/// Numbers of the server process of one generation, written by the stderr
/// and the polling tasks
///
/// Writes from an older generation are ignored, so what the old process
/// reports can't trigger anything after the swap.
#[derive(Default)]
pub struct Readings {
    inner: Mutex<Inner>,
}

#[derive(Default)]
struct Inner {
    generation: u64,
    sample: Sample,
    recycles: u64,
}

impl Readings {
    pub fn new(generation: u64) -> Readings {
        Readings {
            inner: Mutex::new(Inner {
                generation,
                ..Inner::default()
            }),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The server process was replaced, forget the old process' numbers
    pub fn recycled(&self, generation: u64) {
        let mut inner = self.lock();
        inner.generation = generation;
        inner.sample = Sample::default();
        inner.recycles += 1;
    }

    /// Returns `false` for a write of an older generation
    pub fn record_live(&self, generation: u64, live_mb: f64) -> bool {
        let mut inner = self.lock();
        let current = inner.generation == generation;
        if current {
            inner.sample.live_mb = Some(live_mb);
        }
        current
    }

    /// Returns `false` for a write of an older generation
    pub fn record_rss(&self, generation: u64, rss_mb: Option<f64>) -> bool {
        let mut inner = self.lock();
        let current = inner.generation == generation;
        if current {
            inner.sample.rss_mb = rss_mb;
        }
        current
    }

    pub fn sample(&self) -> Sample {
        self.lock().sample
    }

    pub fn recycles(&self) -> u64 {
        self.lock().recycles
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LIMITS: RecycleConfig = RecycleConfig {
        soft_live_mb: Some(12288),
        forced_live_mb: Some(24576),
        max_memory_mb: Some(32768),
    };

    fn sample(live_mb: Option<f64>, rss_mb: Option<f64>) -> Sample {
        Sample { live_mb, rss_mb }
    }

    #[test]
    fn heap_line() {
        assert_eq!(
            parse_heap_line("Live bytes: 3514.31MB Heap size: 7528.78MB"),
            Some(HeapStats {
                live_mb: 3514.31,
                heap_mb: 7528.78
            })
        );
        // with a prefix and a suffix, whole numbers, extra spaces
        assert_eq!(
            parse_heap_line("2026-11-16 ghcide: Live bytes:  12MB   Heap size: 40MB (gc)"),
            Some(HeapStats {
                live_mb: 12.0,
                heap_mb: 40.0
            })
        );
        for line in [
            "",
            "garbage",
            "Live bytes: 3.5GB Heap size: 7.5GB",
            "Live bytes: 3514.31MB Heap size: 7.5GB",
            "Live bytes: 3514.31KB Heap size: 7528.78MB",
            "Live bytes: 3514.31MB",
            "Live bytes: MB Heap size: 1MB",
            "Live bytes: 1.2.3MB Heap size: 1MB",
            "Live bytes: 1e3MB Heap size: 1MB",
            "Live bytes: -1MB Heap size: 1MB",
            "Live bytes: 1MBs Heap size: 1MB",
            "Heap size: 7528.78MB Live bytes: 3514.31MB",
        ] {
            assert_eq!(parse_heap_line(line), None, "{line:?}");
        }
    }

    #[test]
    fn proc_status() {
        let text = "Name:\thaskell\nVmPeak:\t 99 kB\nVmRSS:\t  2048 kB\nRssAnon:\t 1 kB\nVmSwap:\t  1024 kB\nThreads:\t4\n";
        let memory = parse_proc_status(text).unwrap();
        assert_eq!(
            memory,
            ProcMemory {
                rss_kb: 2048,
                swap_kb: 1024
            }
        );
        assert_eq!(memory.total_mb(), 3.0);

        // no swap line
        let memory = parse_proc_status("VmRSS:\t 1024 kB\n").unwrap();
        assert_eq!(memory.swap_kb, 0);
        assert_eq!(memory.total_mb(), 1.0);

        // no VmRSS (kernel thread), or not kB
        assert_eq!(parse_proc_status("Name:\tkthreadd\n"), None);
        assert_eq!(parse_proc_status("VmRSS:\t 1024 MB\n"), None);
        assert_eq!(parse_proc_status("VmRSS:\t lots kB\n"), None);
        assert_eq!(parse_proc_status(""), None);
    }

    #[test]
    fn levels() {
        assert_eq!(level(&LIMITS, &Sample::default()), None);
        assert_eq!(level(&LIMITS, &sample(Some(12287.9), Some(100.0))), None);
        assert_eq!(
            level(&LIMITS, &sample(Some(12288.0), None)),
            Some(Level::Soft)
        );
        assert_eq!(
            level(&LIMITS, &sample(Some(24576.0), None)),
            Some(Level::Forced)
        );
        // RSS + swap alone forces
        assert_eq!(
            level(&LIMITS, &sample(Some(1.0), Some(32768.0))),
            Some(Level::Forced)
        );
        assert_eq!(level(&LIMITS, &sample(None, Some(32767.0))), None);
        // limits which aren't configured don't exist
        let only_soft = RecycleConfig {
            soft_live_mb: Some(10),
            ..RecycleConfig::default()
        };
        assert_eq!(
            level(&only_soft, &sample(Some(1e9), Some(1e9))),
            Some(Level::Soft)
        );
        assert_eq!(
            level(&RecycleConfig::default(), &sample(Some(1e9), Some(1e9))),
            None
        );
    }

    #[test]
    fn soft_is_pending_until_settled() {
        let now = Instant::now();
        let guard = Guard::default();
        assert_eq!(guard.decide(None, true, now), Decision::Idle);
        assert_eq!(guard.decide(Some(Level::Soft), false, now), Decision::Pending);
        assert_eq!(guard.decide(Some(Level::Soft), true, now), Decision::Soft);
    }

    #[test]
    fn forced_is_immediate_and_ignores_the_soft_guard() {
        let start = Instant::now();
        let mut guard = Guard::default();
        assert_eq!(
            guard.decide(Some(Level::Forced), false, start),
            Decision::Forced
        );
        assert!(!guard.record(Level::Forced, start));
        // right after a recycle: forced again, soft held back
        let soon = start + Duration::from_secs(1);
        assert_eq!(
            guard.decide(Some(Level::Forced), false, soon),
            Decision::Forced
        );
        assert_eq!(guard.decide(Some(Level::Soft), true, soon), Decision::Held);
        assert_eq!(
            guard.decide(Some(Level::Soft), false, soon),
            Decision::Pending
        );
    }

    #[test]
    fn soft_guard_is_five_minutes_after_any_recycle() {
        let start = Instant::now();
        let mut guard = Guard::default();
        guard.record(Level::Soft, start);
        let almost = start + SOFT_INTERVAL - Duration::from_millis(1);
        assert_eq!(guard.decide(Some(Level::Soft), true, almost), Decision::Held);
        assert_eq!(
            guard.decide(Some(Level::Soft), true, start + SOFT_INTERVAL),
            Decision::Soft
        );

        // a forced recycle starts the interval too
        let mut guard = Guard::default();
        guard.record(Level::Forced, start);
        assert_eq!(guard.decide(Some(Level::Soft), true, almost), Decision::Held);
    }

    #[test]
    fn three_forced_in_fifteen_minutes_stop_recycling() {
        let start = Instant::now();
        let at = |secs| start + Duration::from_secs(secs);
        let mut guard = Guard::default();
        assert!(!guard.record(Level::Forced, at(0)));
        assert!(!guard.record(Level::Forced, at(60)));
        // soft recycles don't count
        assert!(!guard.record(Level::Soft, at(400)));
        assert!(!guard.stopped());
        assert!(guard.record(Level::Forced, at(900)));
        assert!(guard.stopped());
        for level in [Level::Soft, Level::Forced] {
            assert_eq!(
                guard.decide(Some(level), true, at(10_000)),
                Decision::Stopped
            );
        }
        // nothing to do within the limits, stopped or not
        assert_eq!(guard.decide(None, true, at(10_000)), Decision::Idle);
        // the stop is reported once
        assert!(!guard.record(Level::Forced, at(901)));
    }

    #[test]
    fn forced_recycles_spread_over_the_window_dont_stop() {
        let start = Instant::now();
        let mut guard = Guard::default();
        // 3 within 15 minutes would stop, these slide out of the window
        for minute in [0, 8, 16, 24, 32] {
            let now = start + Duration::from_secs(minute * 60);
            assert!(!guard.record(Level::Forced, now), "minute {minute}");
        }
        assert!(!guard.stopped());
    }

    #[test]
    fn readings_of_an_old_process_are_ignored() {
        let readings = Readings::new(1);
        assert!(readings.record_live(1, 100.0));
        assert!(readings.record_rss(1, Some(200.0)));
        assert_eq!(readings.sample(), sample(Some(100.0), Some(200.0)));
        assert_eq!(readings.recycles(), 0);

        readings.recycled(2);
        assert_eq!(readings.sample(), Sample::default());
        assert_eq!(readings.recycles(), 1);

        // the old process' stderr task and poll still deliver
        assert!(!readings.record_live(1, 30_000.0));
        assert!(!readings.record_rss(1, Some(40_000.0)));
        assert_eq!(readings.sample(), Sample::default());

        assert!(readings.record_live(2, 5.0));
        assert_eq!(readings.sample(), sample(Some(5.0), None));
    }
}
