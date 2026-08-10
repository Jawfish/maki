//! Lifecycle timing for tool calls: one compact duration format and the clock
//! a tool carries from start to finish. A running tool reports the time so
//! far, a finished one keeps the time it took, and both read the same way.

use std::time::{Duration, Instant};

const MILLIS_PER_SEC: u64 = 1_000;
const SECS_PER_MIN: u64 = 60;
const SECS_PER_HOUR: u64 = 60 * SECS_PER_MIN;
/// Below this many seconds a tenth of a second is still worth reading.
const FRACTION_LIMIT_SECS: u64 = 10;

/// Compact and consistent: `120ms`, `1.2s`, `45s`, `2m10s`, `1h5m`.
pub(crate) fn format_duration(elapsed: Duration) -> String {
    let millis = elapsed.as_millis() as u64;
    let secs = millis / MILLIS_PER_SEC;
    if secs == 0 {
        return format!("{millis}ms");
    }
    if secs < FRACTION_LIMIT_SECS {
        return format!("{}.{}s", secs, (millis % MILLIS_PER_SEC) / 100);
    }
    if secs < SECS_PER_MIN {
        return format!("{secs}s");
    }
    if secs < SECS_PER_HOUR {
        return format!("{}m{}s", secs / SECS_PER_MIN, secs % SECS_PER_MIN);
    }
    format!(
        "{}h{}m",
        secs / SECS_PER_HOUR,
        (secs % SECS_PER_HOUR) / SECS_PER_MIN
    )
}

/// A tool's clock. Live while `elapsed` is unset, frozen once the tool
/// finishes, and restored from storage without a start instant at all.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct ToolTiming {
    started_at: Option<Instant>,
    elapsed: Option<Duration>,
}

impl ToolTiming {
    pub(crate) fn started() -> Self {
        Self::from_start(Instant::now())
    }

    pub(crate) fn from_start(started_at: Instant) -> Self {
        Self {
            started_at: Some(started_at),
            elapsed: None,
        }
    }

    /// A tool loaded from a session: no live clock, only the time it took.
    pub(crate) fn restored(millis: u64) -> Self {
        Self {
            started_at: None,
            elapsed: Some(Duration::from_millis(millis)),
        }
    }

    pub(crate) fn start(&self) -> Option<Instant> {
        self.started_at
    }

    pub(crate) fn finish(&mut self) {
        if self.elapsed.is_none() {
            self.elapsed = self.started_at.map(|start| start.elapsed());
        }
    }

    /// Time so far while running, final duration once finished.
    pub(crate) fn observed(&self, running: bool) -> Option<Duration> {
        if running {
            return self.started_at.map(|start| start.elapsed());
        }
        self.elapsed
            .or_else(|| self.started_at.map(|start| start.elapsed()))
    }

    pub(crate) fn millis(&self) -> Option<u64> {
        self.observed(false).map(|d| d.as_millis() as u64)
    }

    pub(crate) fn label(&self, running: bool) -> Option<String> {
        self.observed(running).map(format_duration)
    }
}

#[cfg(test)]
mod tests {
    use super::{ToolTiming, format_duration};
    use std::time::{Duration, Instant};
    use test_case::test_case;

    const RESTORED_MILLIS: u64 = 1_234;
    const RESTORED_LABEL: &str = "1.2s";
    const RUNNING_SECS: u64 = 45;
    const RUNNING_LABEL: &str = "45s";

    #[test_case(0, "0ms" ; "zero")]
    #[test_case(120, "120ms" ; "sub_second")]
    #[test_case(999, "999ms" ; "just_under_a_second")]
    #[test_case(1_234, "1.2s" ; "seconds_with_tenths")]
    #[test_case(9_950, "9.9s" ; "last_tenths_value")]
    #[test_case(45_000, "45s" ; "whole_seconds")]
    #[test_case(130_000, "2m10s" ; "minutes_and_seconds")]
    #[test_case(3_900_000, "1h5m" ; "hours_and_minutes")]
    fn formats_compactly(millis: u64, expected: &str) {
        assert_eq!(format_duration(Duration::from_millis(millis)), expected);
    }

    #[test]
    fn restored_timing_reports_final_duration_only() {
        let timing = ToolTiming::restored(RESTORED_MILLIS);
        assert_eq!(timing.label(false).as_deref(), Some(RESTORED_LABEL));
        assert_eq!(timing.millis(), Some(RESTORED_MILLIS));
        assert_eq!(timing.label(true), None);
    }

    #[test]
    fn running_timing_reports_time_so_far() {
        let timing = ToolTiming::from_start(Instant::now() - Duration::from_secs(RUNNING_SECS));
        assert_eq!(timing.label(true).as_deref(), Some(RUNNING_LABEL));
    }

    #[test]
    fn finish_freezes_the_clock() {
        let mut timing = ToolTiming::from_start(Instant::now() - Duration::from_secs(RUNNING_SECS));
        timing.finish();
        let frozen = timing.millis();
        assert_eq!(timing.label(false).as_deref(), Some(RUNNING_LABEL));
        assert_eq!(timing.millis(), frozen);
    }

    #[test]
    fn untimed_tool_shows_nothing() {
        let timing = ToolTiming::default();
        assert_eq!(timing.label(true), None);
        assert_eq!(timing.label(false), None);
        assert_eq!(timing.millis(), None);
    }
}
