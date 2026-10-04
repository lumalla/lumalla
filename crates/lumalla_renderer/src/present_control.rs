//! Per-output present wake timers and scheduling glue owned by [`RendererState`].

use std::collections::HashMap;
use std::path::PathBuf;
use std::io;
use std::time::Instant;

use log::{debug, error, warn};
use lumalla_shared::{
    EventLoop, PresentationFlipInfo, PresentationNotify, monotonic_deadline_after,
};
use stumpalo::Arena;

use crate::drm::CompletedPageFlip;
use crate::output::{OutputId, OutputPresentControl, OutputState};
use crate::scheduler::FrameTimings;
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

/// Result of arming/ticking one or more outputs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PresentTickResult {
    pub presented_outputs: Vec<String>,
    pub status: PresentStatus,
    pub timings: Option<FrameTimings>,
    /// At least one output finished presenting this tick with no outstanding
    /// page-flip (virtual output or blocking KMS update). Async DRM flips are
    /// excluded — wait for [`FlipSideEffects::completed`] instead.
    pub presented_without_pending_flip: bool,
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
    pub fn mark_dirty(&mut self, now: Instant, arena: &Arena) {
        if let Err(err) = self.sync_output_present_controls(None, arena) {
            warn!("Unable to sync output present controls: {err}");
            return;
        }
        for control in self.outputs.values_mut().map(|o| &mut o.present) {
            control.content_dirty = true;
            control.scheduler.mark_dirty(now);
        }
        if !self.outputs.is_empty() {
            self.scene_dirty = true;
        }
        self.bump_screencast_content();
    }

    /// Bypass vblank alignment on every presentable output.
    pub fn request_immediate(&mut self, arena: &Arena) {
        if let Err(err) = self.sync_output_present_controls(None, arena) {
            warn!("Unable to sync output present controls: {err}");
            return;
        }
        for control in self.outputs.values_mut().map(|o| &mut o.present) {
            control.content_dirty = true;
            control.scheduler.request_immediate();
        }
        if !self.outputs.is_empty() {
            self.scene_dirty = true;
        }
        self.bump_screencast_content();
    }

    /// Cancel all per-output present-wake timeouts (e.g. shutdown).
    pub fn clear_all_present_wakes(&mut self, event_loop: &mut EventLoop) -> io::Result<()> {
        for control in self.outputs.values_mut().map(|o| &mut o.present) {
            control.clear_wake(event_loop)?;
        }
        Ok(())
    }

    /// Arm (or run) presents for every output based on its own scheduler.
    pub fn arm_presents(
        &mut self,
        event_loop: &mut EventLoop,
        notify: &mut dyn PresentationNotify,
        seat_enabled: bool,
        frame_time_msec: u32,
        arena: &Arena,
    ) -> io::Result<PresentTickResult> {
        let pending_protocol_work = notify.pending_present_work();
        self.sync_output_present_controls(Some(event_loop), arena)?;
        let mut names = allocator_api2::vec::Vec::new_in(arena);
        names.extend(self.outputs.keys().cloned());
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

        let presented_without_pending_flip = presented_outputs
            .iter()
            .any(|name| self.output_flip_idle(name));
        let result = PresentTickResult {
            presented_outputs,
            status: self.present_status(),
            timings: last_timings,
            presented_without_pending_flip,
        };
        self.deliver_tick_notifications(&result, notify, frame_time_msec);
        Ok(result)
    }

    /// Handle a fired present-wake timeout for a single output.
    pub fn on_present_timeout(
        &mut self,
        event_loop: &mut EventLoop,
        wake_token: u64,
        notify: &mut dyn PresentationNotify,
        seat_enabled: bool,
        frame_time_msec: u32,
    ) -> io::Result<PresentTickResult> {
        let pending_protocol_work = notify.pending_present_work();
        let Some(name) = self
            .outputs
            .iter()
            .find_map(|(name, output)| (output.present.wake_token == wake_token).then(|| name.clone()))
        else {
            warn!("Ignoring present wake for unknown token {wake_token}");
            return Ok(PresentTickResult {
                presented_outputs: Vec::new(),
                status: self.present_status(),
                timings: None,
                presented_without_pending_flip: false,
            });
        };

        if let Some(control) = self.outputs.get_mut(&name).map(|o| &mut o.present) {
            control.wake_armed = false;
            control.wake_deadline = None;
        }

        let tick = self.arm_or_tick_output(
            event_loop,
            &name,
            pending_protocol_work,
            seat_enabled,
        )?;

        let presented_outputs = if tick.presented {
            vec![name]
        } else {
            Vec::new()
        };
        let presented_without_pending_flip = presented_outputs
            .iter()
            .any(|name| self.output_flip_idle(name));
        let result = PresentTickResult {
            presented_outputs,
            status: self.present_status(),
            timings: tick.timings,
            presented_without_pending_flip,
        };
        self.deliver_tick_notifications(&result, notify, frame_time_msec);
        Ok(result)
    }

    /// Drain DRM page-flips, update per-output schedulers, and re-arm affected wakes.
    pub fn on_drm_events(
        &mut self,
        event_loop: &mut EventLoop,
        notify: &mut dyn PresentationNotify,
        seat_enabled: bool,
        frame_time_msec: u32,
        arena: &Arena,
    ) -> io::Result<FlipSideEffects> {
        let pending_protocol_work = notify.pending_present_work();
        let outcome = match self.dispatch_page_flips_named(arena) {
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
        let mut completed_effects = allocator_api2::vec::Vec::new_in(arena);
        let mut touched = allocator_api2::vec::Vec::new_in(arena);

        for (output_name, flip) in outcome.completed {
            let refresh_ns = self
                .outputs
                .get(&output_name)
                .map(|o| {
                    o.present.scheduler
                        .frame_period()
                        .as_nanos()
                        .min(u128::from(u32::MAX)) as u32
                })
                .unwrap_or(16_666_666);

            if let Some(control) = self.outputs.get_mut(&output_name).map(|o| &mut o.present) {
                let content_dirty = control.content_dirty;
                control.scheduler.after_flip(
                    now,
                    content_dirty,
                    pending_protocol_work,
                );
            }
            if !touched.contains(&output_name) {
                touched.push(output_name.clone());
            }
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

        let effects = FlipSideEffects {
            status: outcome.status,
            completed: completed_effects.into_iter().collect(),
        };
        self.deliver_flip_notifications(&effects, notify, frame_time_msec);
        Ok(effects)
    }

    /// Complete Wayland presentation/frame objects after a present tick (virtual/blocking).
    fn deliver_tick_notifications(
        &self,
        result: &PresentTickResult,
        notify: &mut dyn PresentationNotify,
        frame_time_msec: u32,
    ) {
        if !result.presented_outputs.is_empty()
            && result
                .presented_outputs
                .iter()
                .any(|name| self.output_is_virtual(name))
            && notify.pending_presentation_feedback()
        {
            let refresh_ns = result
                .presented_outputs
                .iter()
                .find_map(|name| self.output_refresh_ns(name))
                .unwrap_or(16_666_666);
            let (tv_sec, tv_usec) = monotonic_time_sec_usec();
            notify.presentation_completed(PresentationFlipInfo {
                tv_sec,
                tv_usec,
                sequence: 0,
                refresh_ns,
            });
        }
        // Virtual/blocking present finished, or paced wake with nothing in flight.
        if (result.presented_without_pending_flip || result.status.idle)
            && notify.pending_frame_callbacks()
        {
            notify.frames_completed(frame_time_msec.max(1));
        }
    }

    /// Complete Wayland presentation/frame objects after DRM page-flips.
    fn deliver_flip_notifications(
        &self,
        effects: &FlipSideEffects,
        notify: &mut dyn PresentationNotify,
        frame_time_msec: u32,
    ) {
        for completed in &effects.completed {
            notify.presentation_completed(PresentationFlipInfo {
                tv_sec: completed.flip.tv_sec,
                tv_usec: completed.flip.tv_usec,
                sequence: completed.flip.sequence,
                refresh_ns: completed.refresh_ns,
            });
        }
        // A completed flip is enough even if another flip is already in flight.
        if (!effects.completed.is_empty() || effects.status.idle)
            && notify.pending_frame_callbacks()
        {
            notify.frames_completed(frame_time_msec.max(1));
        }
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
            .outputs
            .get(name)
            .is_some_and(|o| o.present.content_dirty);
        let scene_or_content = content_dirty || self.scene_dirty;

        let wake_at = {
            let Some(control) = self.outputs.get_mut(name).map(|o| &mut o.present) else {
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
                if let Some(control) = self.outputs.get_mut(name).map(|o| &mut o.present) {
                    control.clear_wake(event_loop)?;
                }
                Ok(OutputTick {
                    presented: false,
                    timings: None,
                })
            }
            Some(at) if at <= now => {
                if let Some(control) = self.outputs.get_mut(name).map(|o| &mut o.present) {
                    control.clear_wake(event_loop)?;
                }
                let tick = self.tick_output(name, pending_protocol_work, seat_enabled);
                // After present, re-arm a future deadline for this output only.
                let now = Instant::now();
                let flip_idle = self.output_flip_idle(name);
                let content_dirty = self
                    .outputs
                    .get(name)
                    .is_some_and(|o| o.present.content_dirty);
                let scene_or_content = content_dirty || self.scene_dirty;
                let wake_at = self.outputs.get_mut(name).map(|o| &mut o.present).and_then(|control| {
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
                    if let Some(control) = self.outputs.get_mut(name).map(|o| &mut o.present) {
                        control.set_wake(event_loop, sec, nsec)?;
                    }
                }
                Ok(tick)
            }
            Some(at) => {
                let remaining = at.saturating_duration_since(now);
                debug_assert!(!remaining.is_zero());
                let (sec, nsec) = monotonic_deadline_after(remaining)?;
                if let Some(control) = self.outputs.get_mut(name).map(|o| &mut o.present) {
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
            .outputs
            .get(name)
            .is_some_and(|o| o.present.content_dirty);
        let scene_or_content = content_dirty || self.scene_dirty;

        let should = self
            .outputs
            .get(name)
            .is_some_and(|o| {
                o.present.scheduler.should_present(
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
                    if let Some(control) = self.outputs.get_mut(name).map(|o| &mut o.present) {
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
                        .outputs
                        .get(name)
                        .is_some_and(|s| s.physical.is_none() && !s.primary_flip_busy())
                    {
                        if let Some(control) = self.outputs.get_mut(name).map(|o| &mut o.present) {
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
                if let Some(control) = self.outputs.get_mut(name).map(|o| &mut o.present) {
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
        self.scene_dirty = self.outputs.values().any(|o| o.present.content_dirty);
    }

    pub(crate) fn output_flip_idle(&self, name: &str) -> bool {
        self.outputs
            .get(name)
            .map(|output| !output.primary_flip_busy())
            .unwrap_or(true)
    }

    /// Ensure `outputs` present controls match currently presentable outputs.
    ///
    /// Removed outputs are only dropped when `event_loop` is provided so their
    /// armed timeouts can be cancelled.
    pub(crate) fn sync_output_present_controls(
        &mut self,
        event_loop: Option<&mut EventLoop>,
        arena: &Arena,
    ) -> io::Result<()> {
        let desired = self.presentable_output_refresh();

        if let Some(event_loop) = event_loop {
            let mut desired_names = allocator_api2::vec::Vec::new_in(arena);
            desired_names.extend(desired.keys().cloned());
            let mut stale = allocator_api2::vec::Vec::new_in(arena);
            for name in self.outputs.keys() {
                if !desired_names.iter().any(|desired| desired == name) {
                    stale.push(name.clone());
                }
            }
            for name in stale {
                if let Some(mut output) = self.outputs.remove(&name) {
                    output.present.clear_wake(event_loop)?;
                    self.free_present_wake_tokens.push(output.present.wake_token);
                    debug!("Removed present control for output {name}");
                }
            }
        }

        for (name, refresh_mhz) in desired {
            if let Some(output) = self.outputs.get_mut(&name) {
                output.present.scheduler.set_refresh_rate(refresh_mhz);
            } else {
                let token = self.alloc_present_wake_token()?;
                let id = if self.virtual_outputs.contains_key(&name) {
                    OutputId::virtual_output(name.clone())
                } else {
                    OutputId::physical(PathBuf::new(), 0, name.clone())
                };
                self.outputs.insert(
                    name.clone(),
                    OutputState::new(id, OutputPresentControl::new(token, refresh_mhz)),
                );
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
                    .outputs
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

    pub(crate) fn alloc_present_wake_token(&mut self) -> io::Result<u64> {
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

fn monotonic_time_sec_usec() -> (u32, u32) {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: `ts` is a valid timespec out-parameter.
    let rc = unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
    if rc != 0 {
        return (0, 0);
    }
    let sec = u32::try_from(ts.tv_sec.max(0)).unwrap_or(u32::MAX);
    let usec = u32::try_from(ts.tv_nsec.max(0) / 1000).unwrap_or(u32::MAX);
    (sec, usec)
}

/// Named flip completions from [`RendererState::dispatch_page_flips_named`].
pub(crate) struct NamedFlipDispatchOutcome<'a> {
    pub status: PresentStatus,
    pub completed: allocator_api2::vec::Vec<(String, CompletedPageFlip), &'a Arena>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scheduler::RenderScheduler;
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
