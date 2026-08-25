//! Bounded line buffer backing the iOS log bridge.
//!
//! Split out of `ios.rs` so it can be compiled and tested on any host: the
//! rest of that module is `cfg(target_os = "ios")` and needs the Apple SDK, so
//! nothing in normal CI ever type-checks it.
//!
//! The Swift side (`ServerManager.rustLogs(since:)`) treats the index as an
//! absolute, monotonically increasing cursor: it asks for everything after the
//! last line it saw. That contract is why eviction has to be counted rather
//! than silently shifting every index down.

use std::collections::VecDeque;

/// Lines retained before the oldest quarter is evicted.
pub const MAX_LOG_LINES: usize = 2_000;

pub struct LogRing {
    lines: VecDeque<String>,
    /// Lines evicted from the front, so indices stay absolute across a wrap.
    dropped: u64,
}

impl LogRing {
    pub const fn new() -> Self {
        Self {
            lines: VecDeque::new(),
            dropped: 0,
        }
    }

    /// Append a line, evicting the oldest quarter once the cap is reached.
    pub fn push(&mut self, line: String) {
        if self.lines.len() >= MAX_LOG_LINES {
            let remove = MAX_LOG_LINES / 4;
            self.lines.drain(..remove);
            self.dropped += remove as u64;
        }
        self.lines.push_back(line);
    }

    /// Total lines ever buffered, evicted ones included.
    ///
    /// Never decreases: the caller stores this as its next cursor, so a value
    /// that went backwards after an eviction would make it re-read old lines
    /// or, once the count lagged its cursor, silently receive nothing.
    pub fn total(&self) -> u64 {
        self.dropped + self.lines.len() as u64
    }

    /// Lines from absolute index `from` onward, joined by `\n`.
    ///
    /// A cursor pointing at evicted lines yields the oldest retained line
    /// rather than an error — those lines are simply gone.
    pub fn since(&self, from: u64) -> String {
        let len = self.lines.len() as u64;
        let start = from.saturating_sub(self.dropped).min(len);
        let mut out = String::new();
        for (i, line) in self.lines.iter().skip(start as usize).enumerate() {
            if i > 0 {
                out.push('\n');
            }
            out.push_str(line);
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::{LogRing, MAX_LOG_LINES};

    fn fill(ring: &mut LogRing, n: usize) {
        for i in 0..n {
            ring.push(format!("line {i}"));
        }
    }

    #[test]
    fn empty_ring_reads_as_empty() {
        let ring = LogRing::new();
        assert_eq!(ring.total(), 0);
        assert_eq!(ring.since(0), "");
    }

    #[test]
    fn since_returns_the_tail_from_an_absolute_index() {
        let mut ring = LogRing::new();
        fill(&mut ring, 5);
        assert_eq!(ring.total(), 5);
        assert_eq!(ring.since(0), "line 0\nline 1\nline 2\nline 3\nline 4");
        assert_eq!(ring.since(3), "line 3\nline 4");
        assert_eq!(ring.since(5), "");
    }

    #[test]
    fn a_cursor_past_the_end_yields_nothing_rather_than_panicking() {
        let mut ring = LogRing::new();
        fill(&mut ring, 3);
        assert_eq!(ring.since(99), "");
    }

    #[test]
    fn total_never_decreases_across_an_eviction() {
        let mut ring = LogRing::new();
        fill(&mut ring, MAX_LOG_LINES);
        assert_eq!(ring.total(), MAX_LOG_LINES as u64);

        // One more line trips the eviction of the oldest quarter.
        ring.push("overflow".to_owned());
        assert_eq!(
            ring.total(),
            MAX_LOG_LINES as u64 + 1,
            "total must count evicted lines; a count that went backwards would \
             strand the Swift cursor and silently drop every later line"
        );
    }

    #[test]
    fn a_cursor_held_across_an_eviction_still_advances() {
        let mut ring = LogRing::new();
        fill(&mut ring, MAX_LOG_LINES);
        let cursor = ring.total();

        ring.push("after".to_owned());

        // The reader asks for everything after the last line it saw and must
        // get exactly the new line, not an empty string.
        assert_eq!(ring.since(cursor), "after");
        assert_eq!(ring.total(), cursor + 1);
    }

    #[test]
    fn eviction_drops_the_oldest_quarter_and_keeps_the_rest() {
        let mut ring = LogRing::new();
        fill(&mut ring, MAX_LOG_LINES);
        ring.push("newest".to_owned());

        let evicted = MAX_LOG_LINES / 4;
        let retained = ring.since(0);
        let first = retained.lines().next().expect("ring is not empty");
        assert_eq!(first, format!("line {evicted}"));
        assert_eq!(retained.lines().count(), MAX_LOG_LINES - evicted + 1);
        assert_eq!(retained.lines().last(), Some("newest"));
    }

    #[test]
    fn a_cursor_pointing_at_evicted_lines_resumes_at_the_oldest_retained() {
        let mut ring = LogRing::new();
        fill(&mut ring, MAX_LOG_LINES);
        ring.push("newest".to_owned());

        let evicted = MAX_LOG_LINES / 4;
        // Index 0 was evicted; asking for it must not panic or skip the tail.
        assert_eq!(
            ring.since(0).lines().next(),
            Some(format!("line {evicted}").as_str())
        );
    }

    #[test]
    fn lines_are_joined_without_a_trailing_newline() {
        let mut ring = LogRing::new();
        fill(&mut ring, 2);
        assert_eq!(ring.since(0), "line 0\nline 1");
    }
}
