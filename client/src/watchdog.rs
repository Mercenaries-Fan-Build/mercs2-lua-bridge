//! The stall watchdog: every 2 s it reads the pump's timestamps and reports a stuck bridge.
//!
//! - Path A: a chunk has been running longer than `watchdog_stuck_ms` (a native call from Lua that never
//!   returned, or an endless Lua loop). The game thread is the one stuck, so no tick can fire; this path
//!   does not depend on tick activity.
//! - Path B: chunks are queued, ticks are firing, but nothing has drained for `watchdog_stuck_ms`: either
//!   the pump is not entered with work waiting, or it is entered and does not drain.
//!
//! The pump's gate is the queue's own pending count, so there is no flag to reset: the watchdog reports.
//! Reports are at least `watchdog_stuck_ms / 2` apart. `watchdog_stuck_ms = 0` turns the watchdog off.

/// How often the watchdog wakes, in milliseconds.
pub const PERIOD_MS: u32 = 2000;
/// Ticks older than this mean the game thread is not in Lua (a loading screen, a pause): no Path B report.
pub const TICK_RECENT_MS: u32 = 2000;

/// The pump's state as the watchdog reads it. Times are `GetTickCount` values; 0 is "never".
#[derive(Debug, Clone, Copy, Default)]
pub struct Snapshot {
    pub now: u32,
    pub last_tick: u32,
    pub last_attempt: u32,
    pub last_progress: u32,
    /// When the running chunk started, 0 when none is running.
    pub exec_start: u32,
    pub pending: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Stall {
    /// Path A, with how long the chunk has run.
    InExec { elapsed_ms: u32 },
    /// Path B: the pump has not been entered for `watchdog_stuck_ms`.
    NotEntered,
    /// Path B: the pump is entered and does not drain.
    NotDraining,
}

impl Stall {
    pub fn pattern(&self) -> &'static str {
        match self {
            Stall::InExec { .. } => "in-bridge-exec-not-returning (native call from Lua hung)",
            Stall::NotEntered => "pump-not-entered (work is queued and ticks fire, but the pump is not entered)",
            Stall::NotDraining => "queue-not-draining (the pump is entered and drains nothing)",
        }
    }
}

/// The watchdog's own memory between wakes.
#[derive(Debug, Default)]
pub struct Watchdog {
    last_report: u32,
}

impl Watchdog {
    /// One wake. `Some` when the bridge is stuck and a report is due.
    pub fn check(&mut self, s: &Snapshot, stuck_ms: u32) -> Option<Stall> {
        if stuck_ms == 0 {
            return None;
        }
        let since = |t: u32| s.now.wrapping_sub(t);
        let spaced = self.last_report == 0 || since(self.last_report) >= stuck_ms / 2;
        let stall = if s.exec_start != 0 && since(s.exec_start) > stuck_ms {
            spaced.then_some(Stall::InExec { elapsed_ms: since(s.exec_start) })
        } else if s.pending > 0 && since(s.last_tick) <= TICK_RECENT_MS && since(s.last_progress) >= stuck_ms && spaced {
            Some(if since(s.last_attempt) > stuck_ms { Stall::NotEntered } else { Stall::NotDraining })
        } else {
            None
        };
        if stall.is_some() {
            self.last_report = s.now.max(1);
        }
        stall
    }
}

/// The report's log lines.
pub fn report(stall: &Stall, s: &Snapshot) -> Vec<String> {
    let since = |t: u32| s.now.wrapping_sub(t);
    let mut lines = vec![format!("[!] lua_bridge: WATCHDOG stuck-state detected — pattern: {}", stall.pattern())];
    if let Stall::InExec { elapsed_ms } = stall {
        lines.push(format!("[!] lua_bridge:   exec_elapsed={elapsed_ms}ms"));
    }
    lines.push(format!(
        "[!] lua_bridge:   PendingScripts={} since_detour={}ms since_attempt={}ms since_progress={}ms",
        s.pending,
        since(s.last_tick),
        since(s.last_attempt),
        since(s.last_progress)
    ));
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    const STUCK: u32 = 8000;

    fn snap(now: u32) -> Snapshot {
        Snapshot { now, last_tick: now, last_attempt: now, last_progress: now, ..Snapshot::default() }
    }

    #[test]
    fn path_a_fires_on_a_long_chunk_without_ticks() {
        let mut w = Watchdog::default();
        let s = Snapshot { exec_start: 1000, last_tick: 1000, ..snap(1000 + STUCK + 1) };
        assert_eq!(w.check(&s, STUCK), Some(Stall::InExec { elapsed_ms: STUCK + 1 }));
        let s2 = Snapshot { now: s.now + STUCK / 2 - 1, ..s };
        assert_eq!(w.check(&s2, STUCK), None, "reports are spaced by half the threshold");
        let s3 = Snapshot { now: s.now + STUCK / 2, ..s };
        assert!(matches!(w.check(&s3, STUCK), Some(Stall::InExec { .. })));
        let s4 = Snapshot { exec_start: s.now - STUCK, ..snap(s.now) };
        assert_eq!(Watchdog::default().check(&s4, STUCK), None, "exactly the threshold is not stuck");
    }

    #[test]
    fn path_b_tells_a_pump_not_entered_from_one_that_does_not_drain() {
        let now = 100_000;
        let base = Snapshot { pending: 1, last_progress: now - STUCK, ..snap(now) };
        assert_eq!(Watchdog::default().check(&base, STUCK), Some(Stall::NotDraining));
        let clear = Snapshot { last_attempt: now - STUCK - 1, ..base };
        assert_eq!(Watchdog::default().check(&clear, STUCK), Some(Stall::NotEntered));
    }

    #[test]
    fn path_b_needs_work_recent_ticks_and_stale_progress() {
        let now = 100_000;
        let base = Snapshot { pending: 1, last_progress: now - STUCK, ..snap(now) };
        assert_eq!(Watchdog::default().check(&Snapshot { pending: 0, ..base }, STUCK), None);
        assert_eq!(Watchdog::default().check(&Snapshot { last_tick: now - TICK_RECENT_MS - 1, ..base }, STUCK), None);
        assert_eq!(Watchdog::default().check(&Snapshot { last_progress: now - STUCK + 1, ..base }, STUCK), None);
        assert_eq!(Watchdog::default().check(&base, 0), None, "0 turns the watchdog off");
    }

    #[test]
    fn the_report_names_the_pattern_and_the_timestamps() {
        let s = Snapshot { exec_start: 1, pending: 2, ..snap(9002) };
        let lines = report(&Stall::InExec { elapsed_ms: 9001 }, &s);
        assert_eq!(lines[0], "[!] lua_bridge: WATCHDOG stuck-state detected — pattern: in-bridge-exec-not-returning (native call from Lua hung)");
        assert_eq!(lines[1], "[!] lua_bridge:   exec_elapsed=9001ms");
        assert_eq!(lines[2], "[!] lua_bridge:   PendingScripts=2 since_detour=0ms since_attempt=0ms since_progress=0ms");
    }
}
