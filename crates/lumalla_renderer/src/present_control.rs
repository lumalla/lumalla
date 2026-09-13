//! Per-output present wake timers and scheduling glue owned by [`RendererState`].

use std::collections::{HashMap, HashSet};
use std::io;
use std::pin::Pin;
use std::time::Instant;

use io_uring::types::Timespec;
use log::{debug, error, warn};
use lumalla_shared::{
    EventLoop, monotonic_deadline_after,
};

use crate::drm::CompletedPageFlip;
use crate::scheduler::{FrameTimings, RenderScheduler};
use crate::{
    PresentStatus, RendererState, SOLID_CLEAR_COLOR,
};

/// Base of the io_uring timeout id range reserved for per-output present wakes.
///
/// App routes CQEs in `[PRESENT_WAKE_TOKEN_BASE, PRESENT_WAKE_TOKEN_BASE + COUNT)`
/// to [`RendererState::on_present_timeout`]. Must not overlap DRM device poll tokens
/// (`1 << 16`) or low Wayland/message tokens.
pub const PRESENT_WAKE_TOKEN_BASE: u64 = 1 << 17;
/// Maximum number of concurrent per-output present-wake tokens.
pub const PRESENT_WAKE_TOKEN_COUNT: u64 = 1024;

/// Returns true when `token` is in the present-wake timeout range.
pub fn is_present_wake_token(token: u64) -> bool {
    (PRESENT_WAKE_TOKEN_BASE..PRESENT_WAKE_TOKEN_BASE + PRESENT_WAKE_TOKEN_COUNT).contains(&token)
}

/// Per-output adaptive schedule + absolute wake timer state.
pub(crate) struct OutputPresentControl {
    pub(crate) scheduler: RenderScheduler,
    pub(crate) wake_token: u64,
    pub(crate) wake_ts: Box<Timespec>,
    pub(crate) wake_deadline: Option<(u64, u32)>,
    pub(crate) wake_armed: bool,
    /// Scene content not yet presented on this output.
    pub(crate) content_dirty: bool,
}

impl OutputPresentControl {
    fn new(wake_token: u64, refresh_mhz: i32) -> Self {
        Self {
            scheduler: RenderScheduler::new(refresh_mhz),
            wake_token,
            wake_ts: Box::new(Timespec::new()),
            wake_deadline: None,
            wake_armed: false,
            content_dirty: false,
        }
    }

    fn clear_wake(&mut self, event_loop: &mut EventLoop) -> io::Result<()> {
        if !self.wake_armed {
            self.wake_deadline = None;
            return Ok(());
        }
        event_loop.cancel_timeout(self.wake_token)?;
        self.wake_armed = false;
        self.wake_deadline = None;
        Ok(())
    }

    fn set_wake(&mut self, event_loop: &mut EventLoop, sec: u64, nsec: u32) -> io::Result<()> {
        if self.wake_armed && self.wake_deadline == Some((sec, nsec)) {
            return Ok(());
        }
        if self.wake_armed {
            event_loop.cancel_timeout(self.wake_token)?;
            self.wake_armed = false;
        }
        *self.wake_ts = Timespec::new().sec(sec).nsec(nsec);
        event_loop.submit_timeout_absolute(Pin::new(self.wake_ts.as_ref()), self.wake_token)?;
        self.wake_armed = true;
        self.wake_deadline = Some((sec, nsec));
        Ok(())
    }
}

/// Result of arming/ticking one or more outputs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PresentTickResult {
    pub presented_outputs: Vec<String>,
    pub status: PresentStatus,
    pub timings: Option<FrameTimings>,
}

/// Page-flip side effects for the app (Wayland presentation feedback).
#[derive(Debug, Clone)]
pub struct FlipSideEffects {
    pub status: PresentStatus,
    /// Completed flips with the owning output name and that output's refresh period in ns.
    pub completed: Vec<CompletedFlipEffect>,
}

#[derive(Debug, Clone)]
pub struct CompletedFlipEffect {
    pub output_name: String,
    pub flip: CompletedPageFlip,
    pub refresh_ns: u32,
}

impl RendererState {
    /// Whether `token` is a present-wake timeout owned by this renderer.
    pub fn is_present_wake_token(token: u64) -> bool {
        is_present_wake_token(token)
    }

    /// Schedule a present on every presentable output (scene changed).
    pub fn mark_dirty(&mut self, now: Instant) {
        if let Err(err) = self.sync_output_present_controls(None) {
            warn!("Unable to sync output present controls: {err}");
            return;
        }
        for control in self.output_presents.values_mut() {
            control.content_dirty = true;
            control.scheduler.mark_dirty(now);
        }
        if !self.output_presents.is_empty() {
            self.scene_dirty = true;
        }
    }

    /// Bypass vblank alignment on every presentable output.
    pub fn request_immediate(&mut self) {
        if let Err(err) = self.sync_output_present_controls(None) {
            warn!("Unable to sync output present controls: {err}");
            return;
        }
        for control in self.output_presents.values_mut() {
            control.content_dirty = true;
            control.scheduler.request_immediate();
        }
        if !self.output_presents.is_empty() {
            self.scene_dirty = true;
        }
    }

    /// Cancel all per-output present-wake timeouts (e.g. shutdown).
    pub fn clear_all_present_wakes(&mut self, event_loop: &mut EventLoop) -> io::Result<()> {
        for control in self.output_presents.values_mut() {
            control.clear_wake(event_loop)?;
        }
        Ok(())
    }

    /// Arm (or run) presents for every output based on its own scheduler.
    pub fn arm_presents(
        &mut self,
        event_loop: &mut EventLoop,
        pending_protocol_work: bool,
        seat_enabled: bool,
    ) -> io::Result<PresentTickResult> {
        self.sync_output_present_controls(Some(event_loop))?;
        let names: Vec<String> = self.output_presents.keys().cloned().collect();
        let mut presented_outputs = Vec::new();
        let mut last_timings = None;

        for name in names {
            let tick = self.arm_or_tick_output(
                event_loop,
                &name,
                pending_protocol_work,
                seat_enabled,
            )?;
            if tick.presented {
                presented_outputs.push(name);
                if tick.timings.is_some() {
                    last_timings = tick.timings;
                }
            }
        }

        Ok(PresentTickResult {
            presented_outputs,
            status: self.present_status(),
            timings: last_timings,
        })
    }

    /// Handle a fired present-wake timeout for a single output.
    pub fn on_present_timeout(
        &mut self,
        event_loop: &mut EventLoop,
        wake_token: u64,
        pending_protocol_work: bool,
        seat_enabled: bool,
    ) -> io::Result<PresentTickResult> {
        let Some(name) = self
            .output_presents
            .iter()
            .find_map(|(name, control)| (control.wake_token == wake_token).then(|| name.clone()))
        else {
            warn!("Ignoring present wake for unknown token {wake_token}");
            return Ok(PresentTickResult {
                presented_outputs: Vec::new(),
                status: self.present_status(),
                timings: None,
            });
        };

        if let Some(control) = self.output_presents.get_mut(&name) {
            control.wake_armed = false;
            control.wake_deadline = None;
        }

        let tick = self.arm_or_tick_output(
            event_loop,
            &name,
            pending_protocol_work,
            seat_enabled,
        )?;

        Ok(PresentTickResult {
            presented_outputs: if tick.presented {
                vec![name]
            } else {
                Vec::new()
            },
            status: self.present_status(),
            timings: tick.timings,
        })
    }

    /// Drain DRM page-flips, update per-output schedulers, and re-arm affected wakes.
    pub fn on_drm_events(
        &mut self,
        event_loop: &mut EventLoop,
        pending_protocol_work: bool,
        seat_enabled: bool,
    ) -> io::Result<FlipSideEffects> {
        let outcome = match self.dispatch_page_flips_named() {
            Ok(outcome) => outcome,
            Err(err) => {
                error!("Unable to dispatch DRM page-flip events: {err:#}");
                return Ok(FlipSideEffects {
                    status: self.present_status(),
                    completed: Vec::new(),
                });
            }
        };

        let now = Instant::now();
        let mut completed_effects = Vec::new();
        let mut touched: HashSet<String> = HashSet::new();

        for (output_name, flip) in outcome.completed {
            let refresh_ns = self
                .output_presents
                .get(&output_name)
                .map(|c| {
                    c.scheduler
                        .frame_period()
                        .as_nanos()
                        .min(u128::from(u32::MAX)) as u32
                })
                .unwrap_or(16_666_666);

            if let Some(control) = self.output_presents.get_mut(&output_name) {
                let content_dirty = control.content_dirty;
                control.scheduler.after_flip(
                    now,
                    content_dirty,
                    pending_protocol_work,
                );
            }
            touched.insert(output_name.clone());
            completed_effects.push(CompletedFlipEffect {
                output_name,
                flip,
                refresh_ns,
            });
        }

        for name in &touched {
            if let Err(err) = self.arm_or_tick_output(
                event_loop,
                name,
                pending_protocol_work,
                seat_enabled,
            ) {
                warn!("Unable to arm present wake after page flip on {name}: {err}");
            }
        }

        Ok(FlipSideEffects {
            status: outcome.status,
            completed: completed_effects,
        })
    }

    fn arm_or_tick_output(
        &mut self,
        event_loop: &mut EventLoop,
        name: &str,
        pending_protocol_work: bool,
        seat_enabled: bool,
    ) -> io::Result<OutputTick> {
        let now = Instant::now();
        let flip_idle = self.output_flip_idle(name);
        let content_dirty = self
            .output_presents
            .get(name)
            .is_some_and(|c| c.content_dirty);
        let scene_or_content = content_dirty || self.scene_dirty;

        let wake_at = {
            let Some(control) = self.output_presents.get_mut(name) else {
                return Ok(OutputTick {
                    presented: false,
                    timings: None,
                });
            };
            control.scheduler.next_wake_at(
                now,
                scene_or_content,
                pending_protocol_work,
                flip_idle,
            )
        };

        match wake_at {
            None => {
                if let Some(control) = self.output_presents.get_mut(name) {
                    control.clear_wake(event_loop)?;
                }
                Ok(OutputTick {
                    presented: false,
                    timings: None,
                })
            }
            Some(at) if at <= now => {
                if let Some(control) = self.output_presents.get_mut(name) {
                    control.clear_wake(event_loop)?;
                }
                let tick = self.tick_output(name, pending_protocol_work, seat_enabled);
                // After present, re-arm a future deadline for this output only.
                let now = Instant::now();
                let flip_idle = self.output_flip_idle(name);
                let content_dirty = self
                    .output_presents
                    .get(name)
                    .is_some_and(|c| c.content_dirty);
                let scene_or_content = content_dirty || self.scene_dirty;
                let wake_at = self.output_presents.get_mut(name).and_then(|control| {
                    control.scheduler.next_wake_at(
                        now,
                        scene_or_content,
                        pending_protocol_work,
                        flip_idle,
                    )
                });
                if let Some(at) = wake_at.filter(|at| *at > now) {
                    let remaining = at.saturating_duration_since(now);
                    let (sec, nsec) = monotonic_deadline_after(remaining)?;
                    if let Some(control) = self.output_presents.get_mut(name) {
                        control.set_wake(event_loop, sec, nsec)?;
                    }
                }
                Ok(tick)
            }
            Some(at) => {
                let remaining = at.saturating_duration_since(now);
                debug_assert!(!remaining.is_zero());
                let (sec, nsec) = monotonic_deadline_after(remaining)?;
                if let Some(control) = self.output_presents.get_mut(name) {
                    control.set_wake(event_loop, sec, nsec)?;
                }
                Ok(OutputTick {
                    presented: false,
                    timings: None,
                })
            }
        }
    }

    fn tick_output(
        &mut self,
        name: &str,
        pending_protocol_work: bool,
        seat_enabled: bool,
    ) -> OutputTick {
        if !seat_enabled || self.presents_halted() {
            return OutputTick {
                presented: false,
                timings: None,
            };
        }

        let now = Instant::now();
        let flip_idle = self.output_flip_idle(name);
        let content_dirty = self
            .output_presents
            .get(name)
            .is_some_and(|c| c.content_dirty);
        let scene_or_content = content_dirty || self.scene_dirty;

        let should = self
            .output_presents
            .get(name)
            .is_some_and(|c| {
                c.scheduler.should_present(
                    now,
                    scene_or_content,
                    pending_protocol_work,
                    flip_idle,
                )
            });
        if !should {
            return OutputTick {
                presented: false,
                timings: None,
            };
        }

        let force = pending_protocol_work && !scene_or_content;
        match self.present_named(name, SOLID_CLEAR_COLOR, force) {
            Ok(outcome) => {
                if outcome.presented {
                    if let Some(control) = self.output_presents.get_mut(name) {
                        control.content_dirty = false;
                        control.scheduler.on_present_started(now);
                        if let Some(timings) = outcome.timings {
                            control.scheduler.on_present_finished(timings.render_duration);
                        }
                    }
                    // Virtual outputs have no DRM flip — advance scheduler now.
                    // This output's content_dirty was cleared above; do not consult
                    // global scene_dirty (it still reflects pre-recompute state and
                    // would force another immediate present on every cursor move).
                    if self
                        .scanouts
                        .get(name)
                        .is_some_and(|s| s.physical.is_none() && s.pending.is_none())
                    {
                        if let Some(control) = self.output_presents.get_mut(name) {
                            control.scheduler.after_flip(
                                now,
                                false,
                                pending_protocol_work,
                            );
                        }
                    }
                    self.recompute_scene_dirty();
                }
                OutputTick {
                    presented: outcome.presented,
                    timings: outcome.timings,
                }
            }
            Err(err) => {
                if let Some(control) = self.output_presents.get_mut(name) {
                    control.scheduler.on_present_started(now);
                }
                error!("Unable to present output {name}: {err:#}");
                OutputTick {
                    presented: false,
                    timings: None,
                }
            }
        }
    }

    fn recompute_scene_dirty(&mut self) {
        self.scene_dirty = self.output_presents.values().any(|c| c.content_dirty);
    }

    pub(crate) fn output_flip_idle(&self, name: &str) -> bool {
        self.scanouts
            .get(name)
            .map(|scanout| scanout.pending.is_none())
            .unwrap_or(true)
    }

    /// Ensure `output_presents` matches currently presentable outputs.
    ///
    /// Removed outputs are only dropped when `event_loop` is provided so their
    /// armed timeouts can be cancelled.
    pub(crate) fn sync_output_present_controls(
        &mut self,
        event_loop: Option<&mut EventLoop>,
    ) -> io::Result<()> {
        let desired = self.presentable_output_refresh();

        if let Some(event_loop) = event_loop {
            let desired_names: HashSet<String> = desired.keys().cloned().collect();
            let stale: Vec<String> = self
                .output_presents
                .keys()
                .filter(|name| !desired_names.contains(*name))
                .cloned()
                .collect();
            for name in stale {
                if let Some(mut control) = self.output_presents.remove(&name) {
                    control.clear_wake(event_loop)?;
                    self.free_present_wake_tokens.push(control.wake_token);
                    debug!("Removed present control for output {name}");
                }
            }
        }

        for (name, refresh_mhz) in desired {
            if let Some(control) = self.output_presents.get_mut(&name) {
                control.scheduler.set_refresh_rate(refresh_mhz);
            } else {
                let token = self.alloc_present_wake_token()?;
                self.output_presents
                    .insert(name.clone(), OutputPresentControl::new(token, refresh_mhz));
                debug!("Added present control for output {name} token={token}");
            }
        }
        Ok(())
    }

    fn presentable_output_refresh(&self) -> HashMap<String, i32> {
        let mut desired = HashMap::new();

        for device in self.drm_devices.opened().values() {
            for connector in device.connectors() {
                if !connector.connected {
                    continue;
                }
                let config = self.output_configs.get(&connector.name);
                let enabled = config.map(|c| c.enabled).unwrap_or(true);
                if !enabled {
                    continue;
                }
                let refresh_mhz = self
                    .scanouts
                    .get(&connector.name)
                    .and_then(|s| s.physical.as_ref())
                    .map(|p| (p.output.mode.refresh_hz() as i32).saturating_mul(1000).max(1))
                    .unwrap_or(60_000);
                desired.insert(connector.name.clone(), refresh_mhz);
            }
        }

        for virtual_output in self.virtual_outputs.values() {
            desired
                .entry(virtual_output.name.clone())
                .or_insert(virtual_output.refresh_mhz);
        }

        desired
    }

    fn alloc_present_wake_token(&mut self) -> io::Result<u64> {
        if let Some(token) = self.free_present_wake_tokens.pop() {
            return Ok(token);
        }
        if self.next_present_wake_token >= PRESENT_WAKE_TOKEN_COUNT {
            return Err(io::Error::other(
                "exhausted present-wake timeout token space",
            ));
        }
        let token = PRESENT_WAKE_TOKEN_BASE + self.next_present_wake_token;
        self.next_present_wake_token += 1;
        Ok(token)
    }
}

struct OutputTick {
    presented: bool,
    timings: Option<FrameTimings>,
}

/// Named flip completions from [`RendererState::dispatch_page_flips_named`].
pub(crate) struct NamedFlipDispatchOutcome {
    pub status: PresentStatus,
    pub completed: Vec<(String, CompletedPageFlip)>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn present_wake_token_range() {
        assert!(is_present_wake_token(PRESENT_WAKE_TOKEN_BASE));
        assert!(is_present_wake_token(
            PRESENT_WAKE_TOKEN_BASE + PRESENT_WAKE_TOKEN_COUNT - 1
        ));
        assert!(!is_present_wake_token(PRESENT_WAKE_TOKEN_BASE - 1));
        assert!(!is_present_wake_token(
            PRESENT_WAKE_TOKEN_BASE + PRESENT_WAKE_TOKEN_COUNT
        ));
        // DRM device tokens live at 1<<16.
        assert!(!is_present_wake_token(1 << 16));
    }

    #[test]
    fn independent_schedulers_keep_distinct_deadlines() {
        let now = Instant::now();
        let mut a = RenderScheduler::new(60_000);
        let mut b = RenderScheduler::new(144_000);
        a.on_flip_completed(now);
        b.on_flip_completed(now);
        a.mark_dirty(now);
        b.mark_dirty(now);

        let wake_a = a.next_wake_at(now, true, false, true).unwrap();
        let wake_b = b.next_wake_at(now, true, false, true).unwrap();
        // 144 Hz schedules sooner after the same vblank sample.
        assert!(wake_b <= wake_a);

        // Advancing only A must not clear B's deadline.
        a.after_flip(now + Duration::from_millis(16), false, false);
        assert!(b.next_wake_at(now + Duration::from_millis(16), true, false, true).is_some());
        assert_eq!(
            a.next_wake_at(now + Duration::from_millis(16), false, false, true),
            None
        );
    }

    #[test]
    fn busy_output_does_not_block_other_scheduler() {
        let now = Instant::now();
        let mut a = RenderScheduler::new(60_000);
        let mut b = RenderScheduler::new(60_000);
        a.mark_dirty(now);
        b.mark_dirty(now);
        assert!(!a.should_present(now, true, false, false));
        assert!(b.should_present(now, true, false, true));
    }
}
