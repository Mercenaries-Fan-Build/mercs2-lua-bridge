# Parity with lua_bridge_DEV v0.5.4

Each behaviour of the C bridge (`lua_bridge_DEV.c`, v0.5.4), where it lives in `client/src`, and the test
that covers it. Fixes and deliberate differences are listed in each table and summarised in the
[CHANGELOG](CHANGELOG.md).

| Area | Modules | Table |
|---|---|---|
| `lua_bridge.ini`, `lua_loader_data.ini`, source encoding | `config`, `store`, `encoding` | [parity/config_store_encoding.md](parity/config_store_encoding.md) |
| Script folders, `lua_loader.ini`, keys, OnKey | `folders`, `keys`, `onkey` | [parity/loader_keys.md](parity/loader_keys.md) |
| `Loader`, `Tcp`, `math`, the polyfill | `loader_ns`, `tcp_ns`, `math_ns`, `polyfill` | [parity/namespaces.md](parity/namespaces.md) |
| Chunk queue, raw-TCP REPL, WebSocket | `queue`, `repl`, `ws` | [parity/network.md](parity/network.md) |

## Integration (`phase`, `pump`, `rearm`, `watchdog`, `win`)

| Behaviour | C lines | Rust | Test |
|---|---|---|---|
| Load phase: the highest ladder phase seen in the current cycle, for `GetLoadPhase` | 1383-1397, 1556-1559 | `phase::PhaseTracker::on_line` | `phases_rise_and_the_milestone_is_reported_once_per_cycle` |
| OnLoad milestone `GlobalExit - Complete`, once per cycle | 1560-1564 | `PhaseTracker::on_line` → `Pump::schedule_onload` | `phases_rise_and_the_milestone_is_reported_once_per_cycle` |
| The scan runs only while a phase or the milestone is still open | 1492-1495, 1504-1507 | `LineOutcome::keep_listening` → `m2_loghook_set_active` | `only_the_wanted_scans_keep_the_listener` |
| Scan budget: 300 s from a load underway, 1800 s before; a rearm starts it again | 1488-1551 | `PhaseTracker::on_line`, `rearm` | `the_budget_runs_from_the_load_and_a_rearm_restarts_it`, `the_idle_budget_applies_before_any_load_and_the_clock_wraps` |
| Pump fast path: nothing to do costs two loads | 1192-1195 | `pump::Pump::tick` | `an_idle_tick_touches_nothing_but_the_tick_stamp` |
| Work is taken only for a usable state (a tick can carry a state that has ended); the polyfill runs before each batch and folder | 1131-1137, 1208, 1214, 2354-2357 | `Pump::tick` (`TickHost::usable`, `begin_batch`), `m2_lua_state_ok` | `an_unusable_state_takes_nothing_and_the_next_usable_tick_does` |
| OnLoad runs before the queue, once per milestone | 1206-1212 | `Pump::tick` | `onload_runs_once_before_the_queue_drains_in_order` |
| Each chunk's result: `lua_bridge.log`, the raw session that asked | 1154-1181 | `Pump::tick`, `Queue::deliver` | `only_the_raw_session_that_asked_gets_a_result` |
| Watchdog telemetry (tick, attempt, progress, chunk start), including OnBoot/OnLoad | 1143-1188, 2376-2380 | `Pump::{tick, timed, snapshot}` | `onload_runs_once_before_the_queue_drains_in_order` |
| On each new state: key scan, OnBoot, OnLoad registration, once per cycle | 1335-1354 | `rearm::Cycle::on_state` | `the_first_state_does_everything_once` |
| New cycle: a never-seen state after the milestone (boot's second state does not count) | 1327-1333 | `Cycle::on_state` | `a_fresh_state_after_the_milestone_starts_a_new_cycle` |
| Re-arm clears phase, milestone, OnBoot, key scan, OnLoad registration | 1399-1429 | `Cycle::on_state`, `PhaseTracker::rearm`, `Pump::cancel_onload` | `a_fresh_state_after_the_milestone_starts_a_new_cycle`, `a_cancelled_onload_does_not_run` |
| Watchdog paths A and B, 2 s period, reports spaced by half the threshold, 0 disables | 2502-2558 | `watchdog::Watchdog::check` | `path_a_fires_on_a_long_chunk_without_ticks`, `path_b_tells_a_pump_not_entered_from_one_that_does_not_drain`, `path_b_needs_work_recent_ticks_and_stale_progress` |
| Watchdog log lines | 2560-2567 | `watchdog::report` | `the_report_names_the_pattern_and_the_timestamps` |
| Start-up: config, loader directories, printf log, data file, REPL, sampler, watchdog | 5025-5157 | `win::start` | live (`ess-live-test`) |
| Folder run logs `Loading script`, `Script result`, feed line `[folder] rel -> result` | 2332-2406 | `win::Bridge::run_folder` | live |

### Differences

- **Hooks and the executor are m2-sdk.dll's.** The fingerprint, the RVA tables, prologue validation, the
  SecuROM poll, `LooksLikeLuaState`, the seen-state ring and `LuaDoString` (C 814-1429, 5059-5120) are
  replaced by `m2_lua_install`, `m2_lua_on_state`, `m2_lua_on_tick` and `m2_lua_exec`. Namespaces are
  registered by m2-sdk.dll on every state, so the per-batch re-registration (1131-1137, 2354-2357) is gone;
  the polyfill still runs on each reported state and before each batch and folder.
- **Re-arm needs the milestone, not OnLoad** (C 1329 tested `g_OnLoadExecuted`), so it also works with
  `loader_onload = 0`. The milestone is watched whenever OnLoad or re-arm is enabled.
- **The watchdog reports and does not reset.** The pump's gate is the queue's own pending count, so there
  is no hot-work flag to raise; the C reset's other targets (the captured state, the seen ring, the TLS
  exec flag) are m2-sdk.dll's or no longer exist.
- **`stack_diag` and `loader_delay_ms` are removed** (the stack diagnostics were part of the C executor; the
  delay was a no-op since 0.5.0), and an unknown key is a startup error.
