//! The loader's once-per-load-cycle work, decided for each Lua state m2-sdk.dll reports
//! (`m2_lua_on_state`), and the re-arm that starts a new cycle.
//!
//! A main-menu return wipes `_G` and the next load builds fresh Lua states, so a state this process has
//! never seen (`is_new_cycle`), arriving after the previous cycle's world-loaded milestone, is the start
//! of a new load cycle. Boot's own second state arrives before any milestone, so it does not re-arm. The
//! re-arm needs only the milestone, not OnLoad: OnBoot and OnKey re-arm with `loader_onload = 0` too.

use std::sync::atomic::{AtomicBool, Ordering};

/// The configuration the decisions read.
#[derive(Debug, Clone, Copy)]
pub struct Flags {
    pub loader_enabled: bool,
    pub rearm_on_menu: bool,
    pub onboot: bool,
    pub onload: bool,
}

/// What to do on this state, in this order.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Actions {
    /// Start a new load cycle: re-arm phase tracking, the milestone, and the steps below.
    pub rearm: bool,
    /// Scan the OnKey folder and (re)build the key bindings.
    pub init_keys: bool,
    /// Run the OnBoot folder on this state.
    pub run_onboot: bool,
    /// List the OnLoad folder in `lua_loader.ini` without running it (it runs at the milestone).
    pub register_onload: bool,
}

#[derive(Default)]
pub struct Cycle {
    keys_initialized: AtomicBool,
    onboot_executed: AtomicBool,
    onload_registered: AtomicBool,
}

/// Claim a once-per-cycle step: true for the first caller since the last re-arm.
fn claim(flag: &AtomicBool) -> bool {
    !flag.swap(true, Ordering::AcqRel)
}

impl Cycle {
    pub const fn new() -> Self {
        Cycle {
            keys_initialized: AtomicBool::new(false),
            onboot_executed: AtomicBool::new(false),
            onload_registered: AtomicBool::new(false),
        }
    }

    /// The decisions for one reported state. `milestone_seen`: the current cycle reached the world.
    pub fn on_state(&self, is_new_cycle: bool, milestone_seen: bool, f: Flags) -> Actions {
        if !f.loader_enabled {
            return Actions::default();
        }
        let rearm = f.rearm_on_menu && is_new_cycle && milestone_seen;
        if rearm {
            self.keys_initialized.store(false, Ordering::Release);
            self.onboot_executed.store(false, Ordering::Release);
            self.onload_registered.store(false, Ordering::Release);
        }
        Actions {
            rearm,
            init_keys: claim(&self.keys_initialized),
            run_onboot: f.onboot && claim(&self.onboot_executed),
            register_onload: f.onload && claim(&self.onload_registered),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALL: Flags = Flags { loader_enabled: true, rearm_on_menu: true, onboot: true, onload: true };

    #[test]
    fn the_first_state_does_everything_once() {
        let c = Cycle::new();
        let first = c.on_state(true, false, ALL);
        assert_eq!(first, Actions { rearm: false, init_keys: true, run_onboot: true, register_onload: true });
        assert_eq!(c.on_state(true, false, ALL), Actions::default(), "boot's second state: nothing again");
        assert_eq!(c.on_state(false, true, ALL), Actions::default(), "a state seen before never re-arms");
    }

    #[test]
    fn a_fresh_state_after_the_milestone_starts_a_new_cycle() {
        let c = Cycle::new();
        c.on_state(true, false, ALL);
        let next = c.on_state(true, true, ALL);
        assert_eq!(next, Actions { rearm: true, init_keys: true, run_onboot: true, register_onload: true });
    }

    #[test]
    fn rearm_does_not_need_onload() {
        let f = Flags { onload: false, ..ALL };
        let c = Cycle::new();
        assert!(c.on_state(true, false, f).run_onboot);
        let next = c.on_state(true, true, f);
        assert!(next.rearm && next.run_onboot && next.init_keys && !next.register_onload);
    }

    #[test]
    fn switches_turn_steps_off() {
        let c = Cycle::new();
        assert_eq!(c.on_state(true, true, Flags { loader_enabled: false, ..ALL }), Actions::default());
        let c = Cycle::new();
        c.on_state(true, false, ALL);
        assert!(!c.on_state(true, true, Flags { rearm_on_menu: false, ..ALL }).rearm);
        let c = Cycle::new();
        let a = c.on_state(true, false, Flags { onboot: false, ..ALL });
        assert!(a.init_keys && !a.run_onboot);
    }
}
