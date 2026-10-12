//! Load-phase tracking for `Loader.GetLoadPhase`, and the world-loaded milestone, per load cycle.
//!
//! Lines come from m2-sdk.dll's log stream (`m2_loghook`). Each cycle tracks the highest phase of
//! loadprobe's ladder seen (`mercs2_formats::load_ladder`) and reports the `GlobalExit - Complete`
//! milestone once. The milestone triggers OnLoad, and marks the cycle complete, so that the next fresh
//! Lua state re-arms the loader. The listener is wanted only while one of the two is still open, and
//! the caller pauses it otherwise ([`LineOutcome::keep_listening`]): the stub that carries these lines
//! runs thousands of times a frame.
//!
//! The scan is bounded: 300 s from the moment a load is underway (a phase was seen, or a cycle was
//! re-armed), 1800 s before that (menus and briefings take as long as the player likes). A load that
//! overruns stops the scan with a log line; the next load cycle ([`PhaseTracker::rearm`]) starts it again.

use mercs2_formats::load_ladder;
use std::sync::Mutex;

/// The world-loaded milestone.
pub const MILESTONE: &[u8] = b"GlobalExit - Complete";
/// The scan budget once a load is underway, in milliseconds.
pub const LOAD_BUDGET_MS: u32 = 300_000;
/// The scan budget before any load is underway, in milliseconds.
pub const IDLE_BUDGET_MS: u32 = 1_800_000;

#[derive(Debug, Default)]
struct State {
    phase: i32,
    milestone_seen: bool,
    rearms: u32,
    started_ms: Option<u32>,
    saw_load: bool,
    gave_up: bool,
}

pub struct PhaseTracker {
    track_phase: bool,
    watch_milestone: bool,
    s: Mutex<State>,
}

/// What one line did.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct LineOutcome {
    /// This line was the cycle's milestone.
    pub milestone: bool,
    /// The budget ran out on this line; the message to log.
    pub gave_up: Option<String>,
    /// Whether the listener is still wanted; when false the caller pauses it until [`PhaseTracker::rearm`].
    pub keep_listening: bool,
}

impl PhaseTracker {
    /// `track_phase`: `loader_track_phase`; `watch_milestone`: the loader is enabled and runs OnLoad or
    /// re-arms on a new load cycle.
    pub fn new(track_phase: bool, watch_milestone: bool) -> Self {
        PhaseTracker { track_phase, watch_milestone, s: Mutex::new(State { phase: -1, ..State::default() }) }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.s.lock().expect("phase lock")
    }

    fn wanted(&self, s: &State) -> bool {
        !s.gave_up
            && ((self.track_phase && s.phase < load_ladder::REACHED_WORLD as i32) || (self.watch_milestone && !s.milestone_seen))
    }

    /// Whether a listener is wanted at all right now (at startup: is there anything to scan for).
    pub fn listening(&self) -> bool {
        self.wanted(&self.lock())
    }

    /// One log line at `now_ms` (a `GetTickCount` value; wrap-around is handled).
    pub fn on_line(&self, line: &[u8], now_ms: u32) -> LineOutcome {
        let mut s = self.lock();
        if !self.wanted(&s) {
            return LineOutcome::default();
        }
        let underway = s.phase > -1 || s.rearms > 0;
        match s.started_ms {
            _ if underway && !s.saw_load => {
                s.saw_load = true;
                s.started_ms = Some(now_ms);
            }
            None => s.started_ms = Some(now_ms),
            Some(start) => {
                let budget = if s.saw_load { LOAD_BUDGET_MS } else { IDLE_BUDGET_MS };
                if now_ms.wrapping_sub(start) > budget {
                    s.gave_up = true;
                    return LineOutcome {
                        milestone: false,
                        gave_up: Some(format!(
                            "load-milestone scan ran {}s without reaching GlobalExit; stopped to protect the frame \
                             budget (phase={} milestoneSeen={} loadSeen={}). The next level load starts it again.",
                            budget / 1000,
                            s.phase,
                            u8::from(s.milestone_seen),
                            u8::from(s.saw_load)
                        )),
                        keep_listening: false,
                    };
                }
            }
        }
        if self.track_phase
            && let Some(p) = load_ladder::match_phase(line)
        {
            s.phase = s.phase.max(p as i32);
        }
        let mut milestone = false;
        if self.watch_milestone && !s.milestone_seen && line.windows(MILESTONE.len()).any(|w| w == MILESTONE) {
            s.milestone_seen = true;
            milestone = true;
        }
        LineOutcome { milestone, gave_up: None, keep_listening: self.wanted(&s) }
    }

    /// The highest phase this cycle, -1 before any.
    pub fn phase(&self) -> i32 {
        self.lock().phase
    }

    /// `(index, name)` for `Loader.GetLoadPhase`; the name is empty before any phase.
    pub fn current(&self) -> (i32, &'static str) {
        let p = self.phase();
        (p, load_ladder::phase_name(p).unwrap_or(""))
    }

    /// Whether this cycle's milestone was seen: the load completed.
    pub fn milestone_seen(&self) -> bool {
        self.lock().milestone_seen
    }

    /// A new load cycle: phase back to -1, the milestone armed again, the budget restarted. Returns the phase the
    /// previous cycle had reached, and whether the listener is wanted again.
    pub fn rearm(&self) -> (i32, bool) {
        let mut s = self.lock();
        let was = s.phase;
        *s = State { phase: -1, rearms: s.rearms.wrapping_add(1).max(1), ..State::default() };
        let wanted = self.wanted(&s);
        (was, wanted)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn phases_rise_and_the_milestone_is_reported_once_per_cycle() {
        let t = PhaseTracker::new(true, true);
        assert!(t.listening());
        assert_eq!(t.current(), (-1, ""));
        let o = t.on_line(b"[lua] Playing EA logo", 0);
        assert!(!o.milestone && o.keep_listening);
        assert_eq!(t.phase(), 4);
        let o = t.on_line(b"[lua] GlobalExit - Complete", 10);
        assert!(o.milestone);
        assert!(!o.keep_listening, "the world is loaded and the milestone seen: nothing left to scan for");
        assert_eq!(t.current(), (20, "World fully loaded (GlobalExit)"));
        assert_eq!(t.on_line(b"[lua] GlobalExit - Complete", 20), LineOutcome::default(), "once per cycle");
        assert_eq!(t.phase(), 20);
        assert_eq!(t.rearm(), (20, true));
        assert_eq!(t.phase(), -1);
        assert!(!t.milestone_seen());
        assert!(t.on_line(b"GlobalExit - Complete", 30).milestone, "armed again in the next cycle");
    }

    #[test]
    fn only_the_wanted_scans_keep_the_listener() {
        let t = PhaseTracker::new(false, true);
        assert!(t.on_line(b"Playing EA logo", 0).keep_listening);
        assert_eq!(t.phase(), -1, "phase tracking off");
        assert!(!t.on_line(b"GlobalExit - Complete", 1).keep_listening);

        let t = PhaseTracker::new(true, false);
        let o = t.on_line(b"GlobalExit - Complete", 0);
        assert!(!o.milestone, "the milestone is not watched");
        assert!(!o.keep_listening, "the world-loaded phase closes phase tracking");

        let t = PhaseTracker::new(false, false);
        assert!(!t.listening());
    }

    #[test]
    fn the_budget_runs_from_the_load_and_a_rearm_restarts_it() {
        let t = PhaseTracker::new(true, true);
        assert!(t.on_line(b"menu", 0).keep_listening);
        let late = IDLE_BUDGET_MS - 10;
        assert!(t.on_line(b"menu", late).keep_listening, "a long menu stay is within the idle budget");
        // A phase line makes the load underway; the 300 s clock starts at the next line.
        t.on_line(b"Playing EA logo", late + 1);
        assert!(t.on_line(b"x", late + 2).keep_listening);
        assert!(t.on_line(b"x", late + 2 + LOAD_BUDGET_MS).keep_listening);
        let o = t.on_line(b"x", late + 3 + LOAD_BUDGET_MS);
        assert!(o.gave_up.as_deref().unwrap().starts_with("load-milestone scan ran 300s"));
        assert!(!o.keep_listening);
        assert_eq!(t.on_line(b"GlobalExit - Complete", 0), LineOutcome::default(), "stopped until a rearm");
        assert!(!t.listening());
        assert_eq!(t.rearm(), (4, true));
        assert!(t.on_line(b"GlobalExit - Complete", 5).milestone);
    }

    #[test]
    fn the_idle_budget_applies_before_any_load_and_the_clock_wraps() {
        let t = PhaseTracker::new(true, true);
        let start = u32::MAX - 10;
        t.on_line(b"menu", start);
        assert!(t.on_line(b"menu", start.wrapping_add(IDLE_BUDGET_MS)).keep_listening);
        assert!(t.on_line(b"menu", start.wrapping_add(IDLE_BUDGET_MS + 1)).gave_up.unwrap().contains("1800s"));
    }
}
