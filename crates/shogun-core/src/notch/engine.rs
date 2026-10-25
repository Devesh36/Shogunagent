//! NotchEngine — the integrated behavioural core (spec §3.3 + §3.4 wiring).
//!
//! This is the pure, testable heart of the adapter loop: it owns the [`HoverTracker`] and
//! [`StateMachine`], routes raw mouse samples and timer expiries through them, and emits
//! concrete [`EngineOutput`]s the macOS layer applies (schedule/cancel real timers, toggle
//! `ignoresMouseEvents`, push the webview `state` event, mark the Q2 expand commit, bump the
//! Q4 top-band counter). Keeping the routing here — including the "re-enter during Collapsing
//! becomes T5" rule and stale-timer suppression — means the integration is unit-tested off
//! device; the macOS adapter only sources events and applies outputs.

use crate::notch::geometry::{
    castle_regions, CastlePosition, GeometryParams, Point, Rect, Regions,
};
use crate::notch::hover::{HoverParams, HoverSignal, HoverTracker};
use crate::notch::statemachine::{Effect, Input, Params, State, StateMachine, Timer};
use std::collections::HashSet;

/// Relationship between one physical display's CoreGraphics and AppKit coordinate spaces.
///
/// CoreGraphics positions are top-left/y-down within `cg`; AppKit positions are
/// bottom-left/y-up within `ns`. Keeping both rectangles avoids applying a primary-display
/// height to monitors above, below, or offset from it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DisplayCoordinateSpace {
    cg: Rect,
    ns: Rect,
}

impl DisplayCoordinateSpace {
    pub fn new(cg: Rect, ns: Rect) -> Self {
        Self { cg, ns }
    }

    pub fn contains_cg(&self, point: Point) -> bool {
        self.cg.contains(point)
    }

    pub fn ns_to_cg(&self, point: Point) -> Point {
        Point::new(
            self.cg.x + point.x - self.ns.x,
            self.cg.y + self.ns.max_y() - point.y,
        )
    }

    pub fn cg_to_ns(&self, point: Point) -> Point {
        Point::new(
            self.ns.x + (point.x - self.cg.x),
            self.ns.y + self.ns.h - (point.y - self.cg.y),
        )
    }
}

/// Events fed into the engine by the macOS adapter.
#[derive(Clone, Copy, Debug)]
pub enum EngineInput {
    /// A raw mouse-move sample in CGEvent (top-left) coordinates.
    MouseCg { x: f64, y: f64, t_ms: u64, buttons: u32 },
    /// Mouse button down in CGEvent coordinates (menu-suppression / drag).
    ButtonDownCg { x: f64, y: f64, t_ms: u64 },
    /// Mouse button up.
    ButtonUp { t_ms: u64 },
    /// A scheduled timer fired.
    TimerFired(Timer),
    /// Click on the preview/panel (from the webview) → open the full panel.
    Click,
    /// Global hotkey (⌘⇧Space) → open the full panel directly.
    Hotkey,
    /// An interaction inside Expanded (from the webview) — resets the idle timeout.
    Interaction,
    /// "Open Full UI" chosen from Expanded.
    OpenFullUi,
    /// The webview reported the collapse animation done.
    AnimDone,
    /// Esc while the panel is key.
    Esc,
    /// Click in the transparent margin of the panel.
    OutsideClick,
    /// Focused app entered fullscreen (FR-NU-08).
    EnterFullscreen,
    /// Focused app left fullscreen.
    ExitFullscreen,
    /// Force collapse (display change / sleep).
    ForceCollapse,
    /// The frontend restored its compact face after a failed opening.
    OpenFailed,
}

/// Concrete actions the macOS adapter must apply.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum EngineOutput {
    /// Push this state to the webview (`state` event) — the only UI signal.
    WebviewState(State),
    /// Toggle `ignoresMouseEvents` on the panel.
    SetIgnoresMouse(bool),
    /// Schedule a one-shot timer that fires `TimerFired(timer)` after `ms`.
    ScheduleTimer { timer: Timer, ms: u64 },
    /// Cancel a pending timer (the adapter must actually stop the underlying timer).
    CancelTimer(Timer),
    /// `t0` for the preview-open latency (Idle→Hover).
    PreviewCommit,
    /// `t0` for the full-expand latency (→Expanded) — NFR-SLO-01.
    ExpandCommit,
    /// Open the separate Full UI window.
    OpenFullUi,
    /// Q4 denominator: the pointer entered the top band.
    TopBandEntry,
    /// CGEventTap early-reject zone: height from display top, width centred on display
    /// (Idle = notch silhouette + pad; open = live panel + grace).
    HoverBand { height: f64, width: f64 },
}

/// The integrated engine. One per display where the panel appears.
pub struct NotchEngine {
    hover: HoverTracker,
    sm: StateMachine,
    /// Current display's CoreGraphics↔AppKit relationship.
    coordinate_space: DisplayCoordinateSpace,
    /// Menubar band floor (NS y) — kept so `set_panel_hit_size` can re-apply regions.
    menubar_min_y: f64,
    /// Idle and dock rects for rebuilding `r_exp` on open/resize.
    idle: Rect,
    castle: CastlePosition,
    dock: Rect,
    /// Last live panel size (from `set_panel_hit_size`); floors open hit region.
    last_panel_w: f64,
    last_panel_h: f64,
    /// Keep the open footprint through a compact tab resize for the next hover transition.
    last_open_w: f64,
    last_open_h: f64,
    /// Timers the adapter currently has scheduled — used to drop stale fires after a cancel.
    active_timers: HashSet<TimerKey>,
}

// HashSet key (Timer isn't Hash upstream; map to a small enum here).
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum TimerKey {
    Dwell,
    HoverExit,
    ExpandedIdle,
    CollapseAnim,
    HoverRelease,
}

impl From<Timer> for TimerKey {
    fn from(t: Timer) -> Self {
        match t {
            Timer::Dwell => TimerKey::Dwell,
            Timer::HoverExit => TimerKey::HoverExit,
            Timer::ExpandedIdle => TimerKey::ExpandedIdle,
            Timer::CollapseAnim => TimerKey::CollapseAnim,
            Timer::HoverRelease => TimerKey::HoverRelease,
        }
    }
}

impl NotchEngine {
    pub fn new(
        regions: Regions,
        menubar_min_y: f64,
        coordinate_space: DisplayCoordinateSpace,
        hover_params: HoverParams,
        sm_params: Params,
        screen: Rect,
        idle: Rect,
    ) -> Self {
        Self {
            hover: HoverTracker::new(regions, menubar_min_y, hover_params),
            sm: StateMachine::new(sm_params),
            coordinate_space,
            menubar_min_y,
            idle,
            castle: CastlePosition::Notch,
            dock: screen,
            last_panel_w: GeometryParams::default().expanded_w,
            last_panel_h: GeometryParams::default().expanded_h,
            last_open_w: 560.0,
            last_open_h: 360.0,
            active_timers: HashSet::new(),
        }
    }

    pub fn state(&self) -> State {
        self.sm.state()
    }

    /// Update regions after a display change without cancelling an active hover or panel.
    pub fn set_regions(
        &mut self,
        regions: Regions,
        menubar_min_y: f64,
        dock: Rect,
        idle: Rect,
    ) {
        self.hover.set_regions(regions, menubar_min_y);
        self.menubar_min_y = menubar_min_y;
        self.dock = dock;
        self.idle = idle;
        self.set_panel_hit_size(self.last_panel_w, self.last_panel_h);
    }

    /// Relocation invalidates pointer history and dwell/leave timers from the old anchor.
    pub fn set_castle(&mut self, castle: CastlePosition, dock: Rect) -> Vec<EngineOutput> {
        self.castle = castle;
        self.dock = dock;
        let regs = castle_regions(
            self.dock,
            self.idle,
            self.last_panel_w,
            self.last_panel_h,
            castle,
        );
        self.hover.reset_for_relocation(regs, self.menubar_min_y);
        let mut out = Vec::new();
        for timer in [
            Timer::Dwell,
            Timer::HoverExit,
            Timer::ExpandedIdle,
            Timer::CollapseAnim,
            Timer::HoverRelease,
        ] {
            self.active_timers.remove(&TimerKey::from(timer));
            out.push(EngineOutput::CancelTimer(timer));
        }
        if self.state() == State::Expanded {
            out.extend(self.on_input(EngineInput::Interaction));
        } else {
            out.extend(self.on_input(EngineInput::ForceCollapse));
            out.extend(self.on_input(EngineInput::AnimDone));
        }
        out
    }

    /// Switch to a display's actual CoreGraphics/AppKit relationship after its regions are set.
    pub fn set_coordinate_space(&mut self, coordinate_space: DisplayCoordinateSpace) {
        self.coordinate_space = coordinate_space;
    }

    /// Replace `r_exp` with the live NSPanel size (open / resize). Leave-grace covers full panel.
    ///
    /// The visual frame may shrink (welded hide) below [`Self::idle`]; hit regions and the
    /// CGEventTap band must still cover the full Idle silhouette so notch hover keeps working.
    pub fn set_panel_hit_size(&mut self, panel_w: f64, panel_h: f64) {
        let w = panel_w.max(1.0);
        let h = panel_h.max(1.0);
        self.last_panel_w = w;
        self.last_panel_h = h;
        if h >= 360.0 {
            self.last_open_w = w;
            self.last_open_h = h;
        }
        let (hit_w, hit_h) = if self.castle == CastlePosition::Notch {
            (w.max(self.idle.w), h.max(self.idle.h))
        } else {
            (w, h)
        };
        let regs = castle_regions(self.dock, self.idle, hit_w, hit_h, self.castle);
        self.hover.set_regions(regs, self.menubar_min_y);
    }

    /// Use the committed native frame, including clamping and manual resize movement.
    pub fn set_panel_hit_frame(&mut self, panel: Rect) {
        self.set_panel_hit_size(panel.w, panel.h);
        let mut regs = castle_regions(self.dock, self.idle, panel.w, panel.h, self.castle);
        regs.r_exp = panel.inset_all(GeometryParams::default().exp_margin);
        self.hover.set_regions(regs, self.menubar_min_y);
    }

    /// CGEventTap early-reject zone for a live panel size. Floors to the Idle silhouette when
    /// welded hide shrinks the visual frame — hover still finds the notch, but the band stays
    /// notch-wide (not a full menu-bar strip). Open panels grow the band to panel + grace.
    pub fn hover_band_cg_for_panel(&self, panel_w: f64, panel_h: f64) -> (f64, f64) {
        let p = GeometryParams::default();
        let idle_h = self.idle.h + p.enter_bottom;
        let idle_w = self.idle.w + 2.0 * p.enter_lr;
        // Height is the open signal — welded hide is 180×32 while idle silhouette is ~179×76.
        // Comparing width alone (180 > 179) wrongly inflated the band to panel+grace in Idle.
        let open = panel_h > self.idle.h;
        let h = if open {
            panel_h + p.exp_margin
        } else {
            idle_h
        };
        let w = if open {
            panel_w + 2.0 * p.exp_margin
        } else {
            idle_w
        };
        (h, w)
    }

    fn idle_hover_band(&self) -> (f64, f64) {
        let p = GeometryParams::default();
        (self.idle.h + p.enter_bottom, self.idle.w + 2.0 * p.enter_lr)
    }

    /// Grow `r_exp` to at least the last/open panel floor as soon as Hover/Expanded starts,
    /// so the cursor can enter the body before `set_panel_size` IPC arrives.
    fn ensure_open_hit_region(&mut self) {
        let p = GeometryParams::default();
        // Product chat defaults (560×360) exceed the 400×180 geometry defaults — the floor covers both.
        let w = self.last_open_w.max(p.expanded_w).max(560.0);
        let h = self.last_open_h.max(p.expanded_h).max(360.0);
        self.set_panel_hit_size(w, h);
    }

    /// Feed one input; returns the outputs to apply, in order.
    pub fn on_input(&mut self, input: EngineInput) -> Vec<EngineOutput> {
        let mut out = Vec::new();
        match input {
            EngineInput::MouseCg { x, y, t_ms, buttons } => {
                let ns = self.coordinate_space.cg_to_ns(Point::new(x, y));
                for sig in self.hover.on_move(ns, t_ms, buttons) {
                    self.route_hover(sig, &mut out);
                }
            }
            EngineInput::ButtonDownCg { x, y, t_ms } => {
                let ns = self.coordinate_space.cg_to_ns(Point::new(x, y));
                self.hover.on_button_down(ns, t_ms);
                self.active_timers.remove(&TimerKey::HoverRelease);
                out.push(EngineOutput::CancelTimer(Timer::HoverRelease));
                // A button press during HoverIntent cancels it (T1 note / menubar intent).
                self.apply_sm(Input::ButtonDown, &mut out);
            }
            EngineInput::ButtonUp { t_ms } => {
                if let Some(ms) = self.hover.on_button_up(t_ms) {
                    self.active_timers.insert(TimerKey::HoverRelease);
                    out.push(EngineOutput::ScheduleTimer { timer: Timer::HoverRelease, ms });
                }
            }
            EngineInput::TimerFired(t) => {
                // Drop stale fires for timers cancelled/rescheduled since (spec robustness).
                if self.active_timers.remove(&TimerKey::from(t)) {
                    match t {
                        Timer::HoverRelease => {
                            for signal in self.hover.on_release_deadline() {
                                self.route_hover(signal, &mut out);
                            }
                        }
                        Timer::Dwell => self.apply_sm(Input::DwellExpired, &mut out),
                        Timer::HoverExit => self.apply_sm(Input::HoverExitExpired, &mut out),
                        Timer::ExpandedIdle => self.apply_sm(Input::ExpandedIdleExpired, &mut out),
                        Timer::CollapseAnim => self.apply_sm(Input::AnimTimeout, &mut out),
                    }
                }
            }
            EngineInput::Click => self.apply_sm(Input::Click, &mut out),
            EngineInput::Hotkey => self.apply_sm(Input::Hotkey, &mut out),
            EngineInput::Interaction => self.apply_sm(Input::Interaction, &mut out),
            EngineInput::OpenFullUi => self.apply_sm(Input::OpenFullUi, &mut out),
            EngineInput::AnimDone => self.apply_sm(Input::AnimDone, &mut out),
            EngineInput::Esc => self.apply_sm(Input::Esc, &mut out),
            EngineInput::OutsideClick => self.apply_sm(Input::OutsideClick, &mut out),
            EngineInput::EnterFullscreen => self.apply_sm(Input::EnterFullscreen, &mut out),
            EngineInput::ExitFullscreen => self.apply_sm(Input::ExitFullscreen, &mut out),
            EngineInput::ForceCollapse => {
                self.active_timers.remove(&TimerKey::HoverRelease);
                out.push(EngineOutput::CancelTimer(Timer::HoverRelease));
                self.apply_sm(Input::ForceCollapse, &mut out);
            }
            EngineInput::OpenFailed => {
                self.apply_sm(Input::OpenFailed, &mut out);
                self.hover.rearm_enter();
            }
        }
        out
    }

    /// Route a hover signal to the state machine, interpreting the region boundaries per the
    /// current state (spec §6.1.1): the dwell (HoverIntent) is cancelled on leaving the inner
    /// R_stay, while the preview (Hover) uses the larger R_exp boundary for its leave-grace so
    /// small movements don't collapse it.
    fn route_hover(&mut self, sig: HoverSignal, out: &mut Vec<EngineOutput>) {
        let st = self.sm.state();
        match sig {
            HoverSignal::TopBandEntry => out.push(EngineOutput::TopBandEntry),
            HoverSignal::EnterEnter { fast } => {
                // Re-entering R_enter during Collapsing revives the preview (T5); while the
                // preview is up it just cancels any pending leave-grace; otherwise it's the
                // Idle→dwell trigger.
                let input = match st {
                    State::Collapsing => Input::ReenterEnter,
                    State::Hover => Input::HoverReenter,
                    _ => Input::HoverEnter { fast },
                };
                self.apply_sm(input, out);
            }
            // Leaving the inner region cancels a pending dwell; ignored once the preview is up.
            HoverSignal::ExitStay => {
                if st == State::HoverIntent {
                    self.apply_sm(Input::HoverExitStay, out);
                }
            }
            // Leaving / re-entering the outer region arms / disarms the preview leave-grace.
            HoverSignal::ExitExp => {
                if st == State::Hover {
                    self.apply_sm(Input::HoverExitStay, out);
                }
            }
            HoverSignal::ReenterExp => {
                if st == State::Hover {
                    self.apply_sm(Input::HoverReenter, out);
                }
            }
        }
    }

    fn apply_sm(&mut self, input: Input, out: &mut Vec<EngineOutput>) {
        for effect in self.sm.step(input) {
            match effect {
                Effect::Transition(s) => {
                    out.push(EngineOutput::WebviewState(s));
                    if matches!(s, State::Hover | State::Expanded) {
                        self.ensure_open_hit_region();
                        let p = GeometryParams::default();
                        out.push(EngineOutput::HoverBand {
                            height: self.last_panel_h + p.exp_margin,
                            width: self.last_panel_w + 2.0 * p.exp_margin,
                        });
                    } else if matches!(s, State::Idle | State::Hidden) {
                        // Idle: notch silhouette only (±enter pad) — not a full-width top strip.
                        let (h, w) = self.idle_hover_band();
                        out.push(EngineOutput::HoverBand { height: h, width: w });
                    }
                }
                Effect::SetIgnoresMouse(b) => out.push(EngineOutput::SetIgnoresMouse(b)),
                Effect::MarkPreviewCommit => out.push(EngineOutput::PreviewCommit),
                Effect::MarkExpandCommit => out.push(EngineOutput::ExpandCommit),
                Effect::OpenFullUi => out.push(EngineOutput::OpenFullUi),
                Effect::StartTimer { timer, ms } => {
                    self.active_timers.insert(TimerKey::from(timer));
                    out.push(EngineOutput::ScheduleTimer { timer, ms });
                }
                Effect::CancelTimer(timer) => {
                    self.active_timers.remove(&TimerKey::from(timer));
                    out.push(EngineOutput::CancelTimer(timer));
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::notch::geometry::{idle_rect, regions, GeometryParams, Rect};

    fn engine() -> (NotchEngine, Regions, f64) {
        // Primary display 1512×982, top-left origin at CG y=0 == NS y=982.
        let screen = Rect::new(0.0, 0.0, 1512.0, 982.0);
        let idle = idle_rect(screen, 200.0, 32.0);
        let regs = regions(screen, idle, GeometryParams::default());
        let menubar_min_y = screen.max_y() - 24.0;
        (
            NotchEngine::new(
                regs,
                menubar_min_y,
                DisplayCoordinateSpace::new(screen, screen),
                HoverParams::default(),
                Params::default(),
                screen,
                idle,
            ),
            regs,
            982.0,
        )
    }

    #[test]
    fn menu_release_opens_without_another_pointer_sample() {
        let (mut e, regs, primary_h) = engine();
        let (x, y) = cg(200.0, 980.0, primary_h);
        e.on_input(EngineInput::ButtonDownCg { x, y, t_ms: 100 });
        let out = e.on_input(EngineInput::ButtonUp { t_ms: 200 });
        assert!(out.contains(&EngineOutput::ScheduleTimer {
            timer: Timer::HoverRelease,
            ms: 300
        }));
        let (x, y) = cg(regs.r_enter.mid_x(), regs.r_enter.y + 2.0, primary_h);
        e.on_input(EngineInput::MouseCg {
            x,
            y,
            t_ms: 300,
            buttons: 0,
        });
        assert_eq!(e.state(), State::Idle);
        e.on_input(EngineInput::TimerFired(Timer::HoverRelease));
        assert_eq!(e.state(), State::HoverIntent);
        e.on_input(EngineInput::TimerFired(Timer::Dwell));
        assert_eq!(e.state(), State::Hover);
        assert!(e
            .on_input(EngineInput::TimerFired(Timer::HoverRelease))
            .is_empty());
    }

    #[test]
    fn menu_release_does_not_open_after_leaving_or_pressing_again() {
        for press_again in [false, true] {
            let (mut e, regs, primary_h) = engine();
            let (menu_x, menu_y) = cg(200.0, 980.0, primary_h);
            e.on_input(EngineInput::ButtonDownCg {
                x: menu_x,
                y: menu_y,
                t_ms: 100,
            });
            e.on_input(EngineInput::ButtonUp { t_ms: 200 });
            let (x, y) = cg(regs.r_enter.mid_x(), regs.r_enter.y + 2.0, primary_h);
            e.on_input(EngineInput::MouseCg {
                x,
                y,
                t_ms: 300,
                buttons: 0,
            });
            if press_again {
                e.on_input(EngineInput::ButtonDownCg { x, y, t_ms: 350 });
            } else {
                e.on_input(EngineInput::MouseCg {
                    x: 10.0,
                    y: 500.0,
                    t_ms: 350,
                    buttons: 0,
                });
            }
            e.on_input(EngineInput::TimerFired(Timer::HoverRelease));
            assert_eq!(e.state(), State::Idle);
        }
    }

    #[test]
    fn drag_release_enters_without_an_extra_move_and_force_close_cancels_release() {
        for force_close in [false, true] {
            let (mut e, regs, primary_h) = engine();
            e.on_input(EngineInput::ButtonDownCg { x: 10.0, y: 500.0, t_ms: 100 });
            let (x, y) = cg(regs.r_enter.mid_x(), regs.r_enter.y + 2.0, primary_h);
            e.on_input(EngineInput::MouseCg { x, y, t_ms: 200, buttons: 1 });
            assert_eq!(e.state(), State::Idle);
            let out = e.on_input(EngineInput::ButtonUp { t_ms: 300 });
            assert!(out.contains(&EngineOutput::ScheduleTimer { timer: Timer::HoverRelease, ms: 0 }));
            if force_close { e.on_input(EngineInput::ForceCollapse); }
            e.on_input(EngineInput::TimerFired(Timer::HoverRelease));
            assert_eq!(e.state(), if force_close { State::Idle } else { State::HoverIntent });
        }
    }

    #[test]
    fn failed_open_rearms_same_anchor_and_cancels_old_timers() {
        let (mut e, regs, primary_h) = engine();
        let (x, y) = cg(regs.r_enter.mid_x(), regs.r_enter.y + 2.0, primary_h);
        e.on_input(EngineInput::MouseCg {
            x,
            y,
            t_ms: 100,
            buttons: 0,
        });
        e.on_input(EngineInput::TimerFired(Timer::Dwell));
        assert_eq!(e.state(), State::Hover);
        let reset = e.on_input(EngineInput::OpenFailed);
        assert!(!reset.contains(&EngineOutput::WebviewState(State::Collapsing)));
        assert_eq!(e.state(), State::Idle);
        assert!(e
            .on_input(EngineInput::TimerFired(Timer::CollapseAnim))
            .is_empty());
        e.on_input(EngineInput::MouseCg {
            x,
            y,
            t_ms: 500,
            buttons: 0,
        });
        assert_eq!(e.state(), State::HoverIntent);
        e.on_input(EngineInput::TimerFired(Timer::Dwell));
        assert_eq!(e.state(), State::Hover);
    }

    #[test]
    fn click_with_tiny_drag_in_enter_does_not_reopen_on_release() {
        let (mut e, regs, primary_h) = engine();
        let (x, y) = cg(regs.r_enter.mid_x(), regs.r_enter.y + 2.0, primary_h);
        e.on_input(EngineInput::MouseCg { x, y, t_ms: 100, buttons: 0 });
        assert_eq!(e.state(), State::HoverIntent);
        e.on_input(EngineInput::ButtonDownCg { x, y, t_ms: 150 });
        assert_eq!(e.state(), State::Idle);
        e.on_input(EngineInput::MouseCg { x: x + 1.0, y, t_ms: 200, buttons: 1 });
        let out = e.on_input(EngineInput::ButtonUp { t_ms: 250 });
        assert!(!out
            .iter()
            .any(|o| matches!(o, EngineOutput::ScheduleTimer { timer: Timer::HoverRelease, .. })));
        e.on_input(EngineInput::MouseCg { x: x + 2.0, y, t_ms: 300, buttons: 0 });
        assert_eq!(e.state(), State::Idle);
    }

    #[test]
    fn repeated_failed_open_waits_for_the_pointer_to_leave() {
        let (mut e, regs, primary_h) = engine();
        let (x, y) = cg(regs.r_enter.mid_x(), regs.r_enter.y + 2.0, primary_h);
        let mut t = 100;
        let mut hover_and_fail = |e: &mut NotchEngine| {
            e.on_input(EngineInput::MouseCg { x, y, t_ms: t, buttons: 0 });
            t += 100;
            if e.state() == State::HoverIntent {
                e.on_input(EngineInput::TimerFired(Timer::Dwell));
                assert_eq!(e.state(), State::Hover);
                e.on_input(EngineInput::OpenFailed);
            }
            e.state()
        };
        assert_eq!(hover_and_fail(&mut e), State::Idle); // first open fails
        assert_eq!(hover_and_fail(&mut e), State::Idle); // one re-armed retry also fails
        e.on_input(EngineInput::MouseCg { x, y, t_ms: 900, buttons: 0 });
        assert_eq!(e.state(), State::Idle); // no loop while the pointer stays
        e.on_input(EngineInput::MouseCg { x: 10.0, y: 500.0, t_ms: 1000, buttons: 0 });
        e.on_input(EngineInput::MouseCg { x, y, t_ms: 1100, buttons: 0 });
        assert_eq!(e.state(), State::HoverIntent);
    }

    /// Convert an NS point to the CG coordinates the engine expects as input.
    fn cg(ns_x: f64, ns_y: f64, primary_h: f64) -> (f64, f64) {
        (ns_x, primary_h - ns_y)
    }

    /// Move the pointer into R_enter (top band + dwell arm).
    fn enter_notch(e: &mut NotchEngine, regs: &Regions, h: f64, t_ms: u64) -> Vec<EngineOutput> {
        let cx = regs.r_enter.mid_x();
        let cy = regs.r_enter.y + regs.r_enter.h / 2.0;
        let (gx, gy) = cg(cx, cy, h);
        e.on_input(EngineInput::MouseCg { x: gx, y: gy, t_ms, buttons: 0 })
    }

    #[test]
    fn mouse_into_notch_schedules_dwell_then_opens_preview() {
        let (mut e, regs, h) = engine();
        let out = enter_notch(&mut e, &regs, h, 100);
        assert!(out.contains(&EngineOutput::TopBandEntry));
        assert!(out.contains(&EngineOutput::WebviewState(State::HoverIntent)));
        assert!(out.iter().any(|o| matches!(o, EngineOutput::ScheduleTimer { timer: Timer::Dwell, .. })));

        // Dwell fires → Hover(preview) + PreviewCommit + passthrough off (NOT ExpandCommit).
        let out = e.on_input(EngineInput::TimerFired(Timer::Dwell));
        assert!(out.contains(&EngineOutput::PreviewCommit));
        assert!(out.contains(&EngineOutput::WebviewState(State::Hover)));
        assert!(out.contains(&EngineOutput::SetIgnoresMouse(false)));
        assert!(!out.contains(&EngineOutput::ExpandCommit));
        assert_eq!(e.state(), State::Hover);
    }

    #[test]
    fn preview_commit_is_once_per_fresh_dwell() {
        let (mut e, regs, h) = engine();
        let has_commit = |out: Vec<EngineOutput>| out.contains(&EngineOutput::PreviewCommit);
        assert!(!has_commit(enter_notch(&mut e, &regs, h, 100)));
        // A fly-by cancels the dwell; its stale timer must remain silent.
        assert!(!has_commit(e.on_input(EngineInput::MouseCg {
            x: 0.0,
            y: 500.0,
            t_ms: 200,
            buttons: 0,
        })));
        assert!(!has_commit(
            e.on_input(EngineInput::TimerFired(Timer::Dwell))
        ));

        enter_notch(&mut e, &regs, h, 300);
        let out = e.on_input(EngineInput::TimerFired(Timer::Dwell));
        assert_eq!(
            out.iter()
                .filter(|o| **o == EngineOutput::PreviewCommit)
                .count(),
            1
        );
        assert!(!has_commit(
            e.on_input(EngineInput::TimerFired(Timer::Dwell))
        ));
        assert!(!has_commit(enter_notch(&mut e, &regs, h, 400)));
        assert!(!has_commit(e.on_input(EngineInput::Click)));
        assert!(!has_commit(e.on_input(EngineInput::ForceCollapse)));
        e.on_input(EngineInput::MouseCg {
            x: 0.0,
            y: 500.0,
            t_ms: 500,
            buttons: 0,
        });
        assert!(!has_commit(enter_notch(&mut e, &regs, h, 600)));
        assert_eq!(e.state(), State::Hover);
        assert!(!has_commit(e.on_input(EngineInput::Hotkey)));
        e.on_input(EngineInput::ForceCollapse);
        e.on_input(EngineInput::AnimDone);
        assert!(!has_commit(e.on_input(EngineInput::Hotkey)));
    }

    #[test]
    fn click_promotes_preview_to_expanded() {
        let (mut e, regs, h) = engine();
        enter_notch(&mut e, &regs, h, 100);
        e.on_input(EngineInput::TimerFired(Timer::Dwell)); // Hover
        let out = e.on_input(EngineInput::Click);
        assert!(out.contains(&EngineOutput::ExpandCommit));
        assert!(out.contains(&EngineOutput::WebviewState(State::Expanded)));
        assert!(out.iter().any(|o| matches!(o, EngineOutput::ScheduleTimer { timer: Timer::ExpandedIdle, .. })));
        assert_eq!(e.state(), State::Expanded);
    }

    #[test]
    fn hotkey_opens_expanded_directly_from_idle() {
        let (mut e, _regs, _h) = engine();
        let out = e.on_input(EngineInput::Hotkey);
        assert!(out.contains(&EngineOutput::ExpandCommit));
        assert!(out.contains(&EngineOutput::WebviewState(State::Expanded)));
        assert!(out.contains(&EngineOutput::SetIgnoresMouse(false)));
        assert_eq!(e.state(), State::Expanded);
    }

    #[test]
    fn stale_hover_exit_fire_after_cancel_is_dropped() {
        let (mut e, regs, h) = engine();
        enter_notch(&mut e, &regs, h, 100);
        e.on_input(EngineInput::TimerFired(Timer::Dwell)); // Hover(preview)

        // Leave R_exp entirely (below the open hit floor) → HoverExit grace scheduled.
        let cx = regs.r_enter.mid_x();
        // After Hover opens, r_exp grows to the product floor (~360pt); leave well below it.
        let (bx, by) = cg(cx, 100.0, h);
        let out = e.on_input(EngineInput::MouseCg { x: bx, y: by, t_ms: 200, buttons: 0 });
        assert!(out.iter().any(|o| matches!(o, EngineOutput::ScheduleTimer { timer: Timer::HoverExit, .. })));

        // Re-enter R_exp → grace cancelled.
        let out = enter_notch(&mut e, &regs, h, 250);
        assert!(out.contains(&EngineOutput::CancelTimer(Timer::HoverExit)));

        // A stale HoverExit fire now must be ignored (still Hover).
        let out = e.on_input(EngineInput::TimerFired(Timer::HoverExit));
        assert!(out.is_empty());
        assert_eq!(e.state(), State::Hover);
    }

    #[test]
    fn reenter_during_collapsing_revives_preview() {
        let (mut e, regs, h) = engine();
        enter_notch(&mut e, &regs, h, 100);
        e.on_input(EngineInput::TimerFired(Timer::Dwell)); // Hover
        // Leave R_exp + grace expiry → Collapsing.
        let cx = regs.r_enter.mid_x();
        let (bx, by) = cg(cx, 100.0, h);
        e.on_input(EngineInput::MouseCg { x: bx, y: by, t_ms: 200, buttons: 0 });
        e.on_input(EngineInput::TimerFired(Timer::HoverExit));
        assert_eq!(e.state(), State::Collapsing);

        // Re-enter R_enter → EnterEnter routed as ReenterEnter → Hover(preview).
        let out = enter_notch(&mut e, &regs, h, 300);
        assert!(out.contains(&EngineOutput::WebviewState(State::Hover)));
        assert_eq!(e.state(), State::Hover);
    }

    #[test]
    fn fullscreen_hides_and_restores() {
        let (mut e, _regs, _h) = engine();
        e.on_input(EngineInput::Hotkey); // Expanded
        let out = e.on_input(EngineInput::EnterFullscreen);
        assert!(out.contains(&EngineOutput::WebviewState(State::Hidden)));
        assert_eq!(e.state(), State::Hidden);
        let out = e.on_input(EngineInput::ExitFullscreen);
        assert!(out.contains(&EngineOutput::WebviewState(State::Idle)));
    }

    #[test]
    fn open_full_ui_emits_output() {
        let (mut e, _regs, _h) = engine();
        e.on_input(EngineInput::Hotkey); // Expanded
        let out = e.on_input(EngineInput::OpenFullUi);
        assert!(out.contains(&EngineOutput::OpenFullUi));
        assert_eq!(e.state(), State::Collapsing);
    }

    #[test]
    fn welded_hide_floors_hover_band_and_chin_hit() {
        // Visual welded hide is 180×32; Idle silhouette is notch_w × (notch_h + content drop).
        let screen = Rect::new(0.0, 0.0, 1512.0, 982.0);
        let idle_h = 32.0 + 44.0;
        let idle = idle_rect(screen, 180.0, idle_h);
        let regs = regions(screen, idle, GeometryParams::default());
        let menubar_min_y = screen.max_y() - 24.0;
        let mut e = NotchEngine::new(
            regs,
            menubar_min_y,
            DisplayCoordinateSpace::new(screen, screen),
            HoverParams::default(),
            Params::default(),
            screen,
            idle,
        );
        e.set_panel_hit_size(180.0, 32.0);
        let p = GeometryParams::default();
        let (band_h, band_w) = e.hover_band_cg_for_panel(180.0, 32.0);
        assert!(band_h >= idle_h + p.enter_bottom);
        // Width stays notch-sized — must not become a full menu-bar catch strip.
        assert!(band_w <= idle.w + 2.0 * p.enter_lr + 1.0);
        // 180×32 welded hide: 180 > 179 must not trigger open-band width (212pt).
        assert!((band_w - (idle.w + 2.0 * p.enter_lr)).abs() < 1.0);

        // Lower chin: inside full Idle rect but below the 32pt visual panel.
        let cx = idle.mid_x();
        let chin_ns_y = idle.y + 20.0;
        let (gx, gy) = cg(cx, chin_ns_y, 982.0);
        let out = e.on_input(EngineInput::MouseCg { x: gx, y: gy, t_ms: 100, buttons: 0 });
        assert!(out.iter().any(|o| matches!(o, EngineOutput::WebviewState(State::HoverIntent))));
    }

    #[test]
    fn display_coordinate_spaces_open_notches_on_offset_monitors() {
        let primary = Rect::new(0.0, 0.0, 1512.0, 982.0);
        let cases = [
            (
                Rect::new(1512.0, 0.0, 1920.0, 1080.0),
                Rect::new(1512.0, -98.0, 1920.0, 1080.0),
            ),
            (
                Rect::new(-1280.0, 0.0, 1280.0, 1024.0),
                Rect::new(-1280.0, -42.0, 1280.0, 1024.0),
            ),
            (
                Rect::new(0.0, -900.0, 1440.0, 900.0),
                Rect::new(0.0, 982.0, 1440.0, 900.0),
            ),
            (
                Rect::new(0.0, 982.0, 1680.0, 1050.0),
                Rect::new(0.0, -1050.0, 1680.0, 1050.0),
            ),
        ];

        for (cg_screen, ns_screen) in cases {
            let idle = idle_rect(ns_screen, 180.0, 24.0);
            let regs = regions(ns_screen, idle, GeometryParams::default());
            let mut engine = NotchEngine::new(
                regions(
                    primary,
                    idle_rect(primary, 200.0, 32.0),
                    GeometryParams::default(),
                ),
                primary.max_y() - 24.0,
                DisplayCoordinateSpace::new(primary, primary),
                HoverParams::default(),
                Params::default(),
                primary,
                idle_rect(primary, 200.0, 32.0),
            );
            engine.set_regions(regs, ns_screen.max_y() - 24.0, ns_screen, idle);
            engine.set_coordinate_space(DisplayCoordinateSpace::new(cg_screen, ns_screen));

            let ns_point = Point::new(regs.r_enter.mid_x(), regs.r_enter.y + regs.r_enter.h / 2.0);
            let cg_point = Point::new(
                cg_screen.x + (ns_point.x - ns_screen.x),
                cg_screen.y + ns_screen.max_y() - ns_point.y,
            );
            let out = engine.on_input(EngineInput::MouseCg {
                x: cg_point.x,
                y: cg_point.y,
                t_ms: 100,
                buttons: 0,
            });
            assert!(out.contains(&EngineOutput::WebviewState(State::HoverIntent)));
        }
    }

    #[test]
    fn edge_castles_activate_at_tab_and_keep_actual_expanded_frame() {
        use crate::notch::geometry::{castle_origin, castle_tab_size};
        for castle in [CastlePosition::LeftEdge, CastlePosition::RightEdge,
            CastlePosition::BottomLeft, CastlePosition::BottomCenter, CastlePosition::BottomRight] {
            let (mut e, _, height) = engine();
            let dock = Rect::new(0.0, 40.0, 1512.0, 918.0);
            e.set_castle(castle, dock);
            let (w, h) = castle_tab_size(castle);
            let origin = castle_origin(dock, w, h, castle);
            let (x, y) = cg(origin.x + w / 2.0, origin.y + h / 2.0, height);
            e.on_input(EngineInput::MouseCg { x, y, t_ms: 100, buttons: 0 });
            assert_eq!(e.state(), State::HoverIntent, "{castle:?}");
            e.on_input(EngineInput::TimerFired(Timer::Dwell));
            assert_eq!(e.state(), State::Hover);
            let frame = Rect::new(300.0, 200.0, 650.0, 450.0);
            e.set_panel_hit_frame(frame);
            let (x, y) = cg(800.0, 300.0, height);
            let output = e.on_input(EngineInput::MouseCg { x, y, t_ms: 200, buttons: 0 });
            assert!(!output.iter().any(|o| matches!(o, EngineOutput::ScheduleTimer { timer: Timer::HoverExit, .. })));
        }
    }

    #[test]
    fn edge_hover_restores_last_open_hit_size_before_native_resize() {
        use crate::notch::geometry::{castle_origin, castle_tab_size};
        let (mut e, _, height) = engine();
        let castle = CastlePosition::LeftEdge;
        let dock = Rect::new(0.0, 40.0, 1512.0, 918.0);
        e.set_castle(castle, dock);
        e.set_panel_hit_size(800.0, 500.0);
        let (w, h) = castle_tab_size(castle);
        e.set_panel_hit_size(w, h);
        let origin = castle_origin(dock, w, h, castle);
        let (x, y) = cg(origin.x + w / 2.0, origin.y + h / 2.0, height);
        e.on_input(EngineInput::MouseCg { x, y, t_ms: 100, buttons: 0 });
        e.on_input(EngineInput::TimerFired(Timer::Dwell));
        assert_eq!(e.state(), State::Hover);
        assert_eq!((e.last_panel_w, e.last_panel_h), (800.0, 500.0));
    }

    #[test]
    fn relocation_cancels_dwell_and_old_leave_but_preserves_expanded_view() {
        let (mut e, regs, height) = engine();
        enter_notch(&mut e, &regs, height, 100);
        let dock = Rect::new(0.0, 0.0, 1512.0, 950.0);
        e.set_castle(CastlePosition::LeftEdge, dock);
        e.on_input(EngineInput::TimerFired(Timer::Dwell));
        assert_eq!(e.state(), State::Idle);
        e.on_input(EngineInput::Hotkey);
        e.set_castle(CastlePosition::RightEdge, dock);
        e.on_input(EngineInput::TimerFired(Timer::HoverExit));
        assert_eq!(e.state(), State::Expanded);
    }

    #[test]
    fn ns_cg_round_trip_for_displays_above_and_below_primary() {
        for (cg_frame, ns_frame) in [
            (Rect::new(-1280.0, -900.0, 1280.0, 900.0), Rect::new(-1280.0, 982.0, 1280.0, 900.0)),
            (Rect::new(300.0, 982.0, 1920.0, 1080.0), Rect::new(300.0, -1080.0, 1920.0, 1080.0)),
        ] {
            let space = DisplayCoordinateSpace::new(cg_frame, ns_frame);
            let p = Point::new(ns_frame.x + 12.0, ns_frame.y + 200.0);
            assert_eq!(space.cg_to_ns(space.ns_to_cg(p)), p);
        }
    }

}
