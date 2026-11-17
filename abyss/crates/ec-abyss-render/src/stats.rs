// SPDX-License-Identifier: AGPL-3.0-only
//! Frame timing instrumentation (COMP-14 §2).
//!
//! Every composited frame records how long the CPU spent in `render_frame`
//! and in the page-flip submission. Samples land in a fixed-capacity ring —
//! no allocation once the compositor is running — and are summarised as
//! p50/p99 on a fixed interval so a regression shows up in the journal
//! without a profiler attached.

use std::time::{Duration, Instant};

/// Number of frames kept for the percentile window. At 144 Hz this is a
/// little under four seconds of history, which comfortably covers the
/// reporting interval below.
const CAPACITY: usize = 1024;

const REPORT_INTERVAL: Duration = Duration::from_secs(5);

/// A fixed-capacity ring of frame timings for one compositor.
pub struct FrameStats {
    /// Report on the interval rather than staying silent (`--stats`).
    enabled: bool,
    render_us: Box<[u32; CAPACITY]>,
    submit_us: Box<[u32; CAPACITY]>,
    next: usize,
    len: usize,
    frames: u64,
    /// Queue-to-flip latency and flip-to-flip interval, per flip (DRM only).
    flip_us: Box<[u32; CAPACITY]>,
    interval_us: Box<[u32; CAPACITY]>,
    flip_next: usize,
    flip_len: usize,
    interval_next: usize,
    interval_len: usize,
    flips: u64,
    last_report: Instant,
}

impl FrameStats {
    pub fn new(enabled: bool) -> Self {
        Self {
            enabled,
            render_us: Box::new([0; CAPACITY]),
            submit_us: Box::new([0; CAPACITY]),
            next: 0,
            len: 0,
            frames: 0,
            flip_us: Box::new([0; CAPACITY]),
            interval_us: Box::new([0; CAPACITY]),
            flip_next: 0,
            flip_len: 0,
            interval_next: 0,
            interval_len: 0,
            flips: 0,
            last_report: Instant::now(),
        }
    }

    /// Record one composited frame. Cheap and allocation-free.
    pub fn record(&mut self, render: Duration, submit: Duration) {
        self.render_us[self.next] = render.as_micros().min(u32::MAX as u128) as u32;
        self.submit_us[self.next] = submit.as_micros().min(u32::MAX as u128) as u32;
        self.next = (self.next + 1) % CAPACITY;
        self.len = (self.len + 1).min(CAPACITY);
        self.frames += 1;
    }

    /// Record one completed page flip: how long after `queue_frame` the kernel
    /// reported it, and the gap since the previous flip on that output.
    /// Cheap and allocation-free.
    pub fn record_flip(&mut self, latency: Duration, interval: Option<Duration>) {
        let us = |d: Duration| d.as_micros().min(u32::MAX as u128) as u32;
        self.flip_us[self.flip_next] = us(latency);
        // The first flip of an output has no interval.
        if let Some(i) = interval {
            self.interval_us[self.interval_next] = us(i);
            self.interval_next = (self.interval_next + 1) % CAPACITY;
            self.interval_len = (self.interval_len + 1).min(CAPACITY);
        }
        self.flip_next = (self.flip_next + 1) % CAPACITY;
        self.flip_len = (self.flip_len + 1).min(CAPACITY);
        self.flips += 1;
    }

    /// Emit a summary if the reporting interval has elapsed. Called once per
    /// frame; does nothing until it is due.
    pub fn maybe_report(&mut self) {
        if !self.enabled || self.len == 0 {
            return;
        }
        let elapsed = self.last_report.elapsed();
        if elapsed < REPORT_INTERVAL {
            return;
        }
        self.last_report = Instant::now();
        let mut render: Vec<u32> = self.render_us[..self.len].to_vec();
        let mut submit: Vec<u32> = self.submit_us[..self.len].to_vec();
        render.sort_unstable();
        submit.sort_unstable();
        let fps = self.frames as f64 / elapsed.as_secs_f64();
        tracing::info!(
            frames = self.frames,
            fps = format_args!("{fps:.1}"),
            render_p50_us = pct(&render, 50),
            render_p99_us = pct(&render, 99),
            submit_p50_us = pct(&submit, 50),
            submit_p99_us = pct(&submit, 99),
            "frame stats"
        );
        self.frames = 0;
    }

    /// Summarise the ring without disturbing it. Allocates two vectors the
    /// size of the window; called only from the control socket, never from
    /// the render path.
    pub fn snapshot(&self) -> Snapshot {
        if self.len == 0 {
            return Snapshot::default();
        }
        let mut render: Vec<u32> = self.render_us[..self.len].to_vec();
        let mut submit: Vec<u32> = self.submit_us[..self.len].to_vec();
        render.sort_unstable();
        submit.sort_unstable();
        let mut flip: Vec<u32> = self.flip_us[..self.flip_len].to_vec();
        let mut interval: Vec<u32> = self.interval_us[..self.interval_len].to_vec();
        flip.sort_unstable();
        interval.sort_unstable();
        Snapshot {
            samples: self.len,
            frames: self.frames,
            flips: self.flips,
            flip_p50_us: pct(&flip, 50),
            flip_p99_us: pct(&flip, 99),
            interval_p50_us: pct(&interval, 50),
            interval_p99_us: pct(&interval, 99),
            render_p50_us: pct(&render, 50),
            render_p99_us: pct(&render, 99),
            submit_p50_us: pct(&submit, 50),
            submit_p99_us: pct(&submit, 99),
        }
    }
}

/// A point-in-time summary of the ring, for `get_metrics` (COMP-13 §2.1).
#[derive(Debug, Clone, Copy, Default)]
pub struct Snapshot {
    /// Samples currently in the ring.
    pub samples: usize,
    /// Frames composited since the last periodic report.
    pub frames: u64,
    pub render_p50_us: u32,
    pub render_p99_us: u32,
    pub submit_p50_us: u32,
    pub submit_p99_us: u32,
    /// Page flips completed since start (DRM backend only).
    pub flips: u64,
    /// `queue_frame` to the flip's completion event.
    pub flip_p50_us: u32,
    pub flip_p99_us: u32,
    /// Between consecutive flips on one output.
    pub interval_p50_us: u32,
    pub interval_p99_us: u32,
}

/// Nearest-rank percentile over an already sorted slice.
fn pct(sorted: &[u32], p: usize) -> u32 {
    if sorted.is_empty() {
        return 0;
    }
    let rank = (sorted.len() * p).div_ceil(100).max(1) - 1;
    sorted[rank.min(sorted.len() - 1)]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percentiles_are_nearest_rank() {
        let v: Vec<u32> = (1..=100).collect();
        assert_eq!(pct(&v, 50), 50);
        assert_eq!(pct(&v, 99), 99);
        assert_eq!(pct(&[], 50), 0);
        assert_eq!(pct(&[7], 99), 7);
    }

    #[test]
    fn flips_report_latency_and_interval() {
        let mut s = FrameStats::new(false);
        s.record(Duration::from_micros(1), Duration::from_micros(1));
        s.record_flip(Duration::from_micros(50_000), None);
        s.record_flip(Duration::from_micros(48_000), Some(Duration::from_micros(50_000)));
        let snap = s.snapshot();
        assert_eq!(snap.flips, 2);
        assert_eq!(snap.flip_p99_us, 50_000);
        assert_eq!(snap.interval_p50_us, 50_000);
    }

    #[test]
    fn ring_wraps_without_growing() {
        let mut s = FrameStats::new(false);
        for _ in 0..(CAPACITY * 2 + 3) {
            s.record(Duration::from_micros(10), Duration::from_micros(1));
        }
        assert_eq!(s.len, CAPACITY);
    }
}
