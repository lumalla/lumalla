//! Adaptive frame scheduler: estimates render/flip duration from past frames
//! and picks the next present time aligned to the display refresh period.

use std::time::{Duration, Instant};

const DEFAULT_REFRESH_MHZ: i32 = 60_000;
const MIN_FRAME_PERIOD: Duration = Duration::from_nanos(6_900_000); // ~145 Hz
const MAX_FRAME_PERIOD: Duration = Duration::from_millis(25); // ~40 Hz
const INITIAL_RENDER_ESTIMATE: Duration = Duration::from_millis(8);
const INITIAL_FLIP_MARGIN: Duration = Duration::from_millis(2);
const EMA_ALPHA: f64 = 0.15;

fn refresh_mhz_to_period(refresh_mhz: i32) -> Duration {
    let hz = refresh_mhz.max(1) as f64 / 1000.0;
    Duration::from_secs_f64(1.0 / hz)
}

fn update_ema(previous: Duration, sample: Duration) -> Duration {
    let p = previous.as_secs_f64();
    let s = sample.as_secs_f64();
    Duration::from_secs_f64(p * (1.0 - EMA_ALPHA) + s * EMA_ALPHA)
}

/// Timing samples from a completed present (CPU/GPU work before flip submit).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameTimings {
    pub render_duration: Duration,
}

/// Adaptive scheduler for when to run the next present pass.
#[derive(Debug, Clone)]
pub struct RenderScheduler {
    frame_period: Duration,
    estimated_render: Duration,
    estimated_flip_margin: Duration,
    next_present_at: Option<Instant>,
    last_vblank: Option<Instant>,
    last_present_started: Option<Instant>,
    force_immediate: bool,
}

impl Default for RenderScheduler {
    fn default() -> Self {
        Self::new(DEFAULT_REFRESH_MHZ)
    }
}

impl RenderScheduler {
    pub fn new(refresh_mhz: i32) -> Self {
        Self {
            frame_period: refresh_mhz_to_period(refresh_mhz),
            estimated_render: INITIAL_RENDER_ESTIMATE,
            estimated_flip_margin: INITIAL_FLIP_MARGIN,
            next_present_at: None,
            last_vblank: None,
            last_present_started: None,
            force_immediate: false,
        }
    }

    pub fn set_refresh_rate(&mut self, refresh_mhz: i32) {
        self.frame_period = refresh_mhz_to_period(refresh_mhz);
    }

    pub fn frame_period(&self) -> Duration {
        self.frame_period
    }

    pub fn estimated_render(&self) -> Duration {
        self.estimated_render
    }

    /// Scene content changed; schedule a present before the next vblank when possible.
    pub fn mark_dirty(&mut self, now: Instant) {
        self.schedule_next_present(now);
    }

    /// Bypass vblank alignment (seat activation, hotplug, config changes).
    pub fn request_immediate(&mut self) {
        self.force_immediate = true;
        self.next_present_at = Some(Instant::now());
    }

    /// Timeout for the main event loop so a scheduled present fires without input.
    pub fn poll_timeout(&self, now: Instant) -> Option<Duration> {
        if self.force_immediate {
            return Some(Duration::ZERO);
        }
        self.next_present_at
            .map(|at| at.saturating_duration_since(now))
    }

    /// Whether the compositor should run a present pass now.
    ///
    /// `flip_idle` should be false while a page-flip is in flight so work can
    /// coalesce instead of blocking the event loop on redundant GPU renders.
    pub fn should_present(
        &self,
        now: Instant,
        scene_dirty: bool,
        pending_frame_callbacks: bool,
        flip_idle: bool,
    ) -> bool {
        if !scene_dirty && !pending_frame_callbacks {
            return false;
        }
        if self.force_immediate {
            return true;
        }
        if !flip_idle {
            return false;
        }
        match self.next_present_at {
            Some(at) => now >= at,
            None => true,
        }
    }

    pub fn on_present_started(&mut self, now: Instant) {
        self.force_immediate = false;
        self.last_present_started = Some(now);
    }

    pub fn on_present_finished(&mut self, render_duration: Duration) {
        self.estimated_render = update_ema(self.estimated_render, render_duration);
    }

    /// Called when a DRM page-flip completion is processed.
    pub fn on_flip_completed(&mut self, now: Instant) {
        if let Some(prev) = self.last_vblank {
            let interval = now.duration_since(prev);
            if interval >= MIN_FRAME_PERIOD && interval <= MAX_FRAME_PERIOD {
                self.frame_period = update_ema(self.frame_period, interval);
            }
        }
        self.last_vblank = Some(now);

        if let Some(started) = self.last_present_started {
            let flip_latency = now.saturating_duration_since(started);
            self.estimated_flip_margin = update_ema(self.estimated_flip_margin, flip_latency);
        }
    }

    /// After a flip, reschedule if more work is still pending.
    pub fn after_flip(&mut self, now: Instant, scene_dirty: bool, pending_frame_callbacks: bool) {
        self.on_flip_completed(now);
        if scene_dirty || pending_frame_callbacks {
            self.schedule_next_present(now);
            if self
                .next_present_at
                .is_some_and(|at| at.saturating_duration_since(now) > self.frame_period)
            {
                self.next_present_at = Some(now);
            }
        } else {
            self.next_present_at = None;
        }
    }

    fn lead_time(&self) -> Duration {
        self.estimated_render + self.estimated_flip_margin
    }

    fn schedule_next_present(&mut self, now: Instant) {
        let lead = self.lead_time();
        let target = if let Some(vblank) = self.last_vblank {
            let mut vblank_target = vblank + self.frame_period;
            while vblank_target < now + lead {
                vblank_target += self.frame_period;
            }
            vblank_target.checked_sub(lead).unwrap_or(now)
        } else {
            now
        };
        self.next_present_at = Some(match self.next_present_at {
            Some(existing) => existing.min(target),
            None => target,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_refresh_is_sixty_hz() {
        let scheduler = RenderScheduler::default();
        let period = scheduler.frame_period();
        assert!(period >= Duration::from_nanos(16_666_000));
        assert!(period <= Duration::from_nanos(16_667_000));
    }

    #[test]
    fn mark_dirty_schedules_present() {
        let now = Instant::now();
        let mut scheduler = RenderScheduler::default();
        scheduler.mark_dirty(now);
        assert!(scheduler.should_present(now, true, false, true));
    }

    #[test]
    fn waits_for_flip_while_not_idle() {
        let now = Instant::now();
        let mut scheduler = RenderScheduler::default();
        scheduler.mark_dirty(now);
        assert!(!scheduler.should_present(now, true, false, false));
    }

    #[test]
    fn pending_callbacks_trigger_present() {
        let now = Instant::now();
        let scheduler = RenderScheduler::default();
        assert!(scheduler.should_present(now, false, true, true));
    }

    #[test]
    fn ema_tracks_render_duration() {
        let mut scheduler = RenderScheduler::default();
        let initial = scheduler.estimated_render();
        scheduler.on_present_finished(Duration::from_millis(20));
        assert!(scheduler.estimated_render() > initial);
    }

    #[test]
    fn flip_interval_refines_frame_period() {
        let mut scheduler = RenderScheduler::default();
        let initial = scheduler.frame_period();
        let t0 = Instant::now();
        scheduler.on_flip_completed(t0);
        scheduler.on_flip_completed(t0 + Duration::from_millis(16));
        assert_ne!(scheduler.frame_period(), initial);
    }

    #[test]
    fn poll_timeout_returns_zero_when_immediate() {
        let mut scheduler = RenderScheduler::default();
        scheduler.request_immediate();
        assert_eq!(scheduler.poll_timeout(Instant::now()), Some(Duration::ZERO));
    }
}
