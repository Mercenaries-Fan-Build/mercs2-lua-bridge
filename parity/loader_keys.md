# Parity: script loader folders, key names, OnKey

Source: `mods/lua-bridge-DEV/lua_bridge_DEV.c`. Rust: `client/src/folders.rs`, `client/src/keys.rs`,
`client/src/onkey.rs`. Tests are `cargo test` names in the module's `tests`.

| Behaviour | C lines | Rust item | Test |
|---|---|---|---|
| Create `scripts/`, `OnBoot`, `OnLoad`, `OnKey` | 1747-1767 | `folders::ensure_loader_directories`, `ScriptLoader::ensure_directories` | `ensure_directories_creates_all_four` |
| Recursive scan, `.lua` suffix case-insensitive, name longer than 4 | 1779-1845, 1826-1827 | `folders::discover`, `classify_entry` | `discover_recursive_sorted_backslash_keys`, `classify_rules` |
| Depth cap: folder is depth 0, `depth > 8` not scanned | 1772, 1784-1788 | `MAX_DEPTH`, `ScanReport::depth_capped` | `discover_depth_cap_is_eight_levels_below_the_folder` |
| Reparse points (files and dirs) skipped before anything else | 1802-1806 | `classify_entry` → `SkipReparsePoint`; Windows `FILE_ATTRIBUTE_REPARSE_POINT`, host symlinks | `classify_rules`, `discover_skips_symlinks` |
| `_`-prefixed directories skipped (files are not) | 1821 | `EntryAction::SkipLibraryDir` | `classify_rules`, `discover_recursive_sorted_backslash_keys` |
| `rel_path` keyed with `\` separators | 1809-1813, 1823 | `ScriptFile::rel_path` | `discover_recursive_sorted_backslash_keys` |
| 128-file cap, overflow counted | 1728, 1828-1839, 2264-2268 | `MAX_SCRIPTS`, `ScanReport::dropped` | `discover_caps_at_max_scripts_and_counts_the_rest` |
| Alphabetical order: `_stricmp` (lowercase fold) | 1737-1739 | `cmp_ignore_ascii_case` | `discover_recursive_sorted_backslash_keys` |
| Load order: order then `_stricmp` | 1741-1745, 2304 | `folders::register` sort | `register_assigns_writes_back_and_orders` |
| ini header written only if the file is absent (raw bytes, as `fprintf` wrote them) | 1847-1864 | `ensure_loader_ini`, `LOADER_INI_HEADER` | `ini_header_written_once`, `ini_set_appends_section_with_file_line_endings` |
| ini read and written in the process ANSI code page, as the `*PrivateProfile*A` APIs did | 2282-2299, 2786-2792 | `AnsiCodec`, `ProcessAnsi` (Windows: `MultiByteToWideChar(CP_ACP, MB_ERR_INVALID_CHARS)` / `WideCharToMultiByte(CP_ACP, WC_NO_BEST_FIT_CHARS)`), `Cp1252` (host tests), `LoaderIni::load`/`save` | `cp1252_every_byte_round_trips_and_matches_the_windows_table`, `cp1252_encode_is_exact_not_best_fit`, `cp1252_path_keys_round_trip_through_the_ini`, `path_without_an_ansi_form_is_a_hard_error` |
| `off`/`no`/`disabled` (case-insensitive) disables | 2282-2292 | `is_disable_value`, `Order::Disabled` | `register_disable_values_and_invalid_orders` |
| Missing order (or `-1`, or empty) → `(i+1)*10`, i = alphabetical index over all scripts, written back | 2294-2300 | `folders::register`, `LoaderIni::set` | `register_assigns_writes_back_and_orders` |
| Explicit `0` still runs | 2278-2281 | `Order::Run(0)` | `register_disable_values_and_invalid_orders` |
| Empty folder: ini untouched beyond header | 2270 | `ScriptLoader::register_locked` | `empty_folder_leaves_ini_header_only` |
| Register-only pass (OnLoad at launch) | 2311-2330 | `ScriptLoader::register_folder` | `register_folder_runs_nothing_and_writes_ini` |
| Re-entrancy refused | 2228, 2314-2319, 2338-2343 | `ScriptLoader` busy guard | `nested_pass_is_refused` |
| Libs re-registered once before a non-empty batch | 2352-2357 | `run_folder` `prepare` closure | `run_folder_in_order_with_outcomes`, `run_folder_without_scripts_skips_prepare` |
| Run each enabled script in order; bytes via normaliser; result per script | 2359-2394 | `run_folder` `normalize`/`execute`, `ScriptOutcome` | `run_folder_in_order_with_outcomes`, `run_folder_execute_failure_is_reported` |
| Feed line `[folder] rel -> result` | 2387-2392 | `folders::feed_line`, `onkey::result_feed_line` | `feed_line_format`, `queued_event_reaches_the_sink` |
| KEYVAL default: first 10 fgets pieces, `"` preferred over `'`, < 64 bytes, else `unassigned` | 1866-1896 | `keys::extract_default_key` | `keyval_*` (8 tests) |
| Key-name table: letters, digits, numpad, F1-F24, 60 named | 1906-1994 | `resolve_key_name`, `NAMED_KEYS`, `NUMPAD_NAMED_KEYS` | `single_letters_and_digits`, `numpad_rules_never_fall_through`, `function_keys_one_to_twenty_four`, `named_keys_case_insensitive`, `every_table_entry_resolves_to_itself`, `table_sizes` |
| F-key names strict: `F` + 1..=24, no leading zero, nothing after (C used `atoi`) | 1930-1933 | `resolve_key_name` | `function_keys_one_to_twenty_four`, `function_key_names_are_strict`, `lenient_key_names_are_reported_unrecognized` |
| VK codes | 1921-1991 | `keys::vk` (compile-time asserted against `windows-sys` on Windows) | `cargo check --target i686-pc-windows-gnu` |
| Spec parsing: trim, modifier tokens, stop at empty/non-modifier token | 2002-2041 | `resolve_key_spec` | `spec_*` (4 tests) |
| Ring: 128 slots, drop oldest, pop all, clear | 385, 2454-2479, 3101-3111 | `KeyEventRing`, `KeyEvents` | `ring_push_pop_in_order`, `ring_full_drops_oldest`, `ring_clear` |
| Sampler: all 256 VKs, state tracked unfocused, push gated on focus, 16 ms | 2584-2607 | `KeyEventSampler::step`, `keys::win::spawn_key_event_sampler` | `sampler_pushes_edges_only_while_focused` |
| `GetAsyncKeyState & 0x8000` | 2599, 2620-2622, 2650 | `async_state_is_down` | `async_state_high_bit` |
| Focus = foreground window's PID is ours | 2484-2491 | `focus_matches`, `keys::win::game_focused` | `focus_is_a_pid_match` |
| Exact vs loose modifier match | 2618-2625 | `onkey::modifiers_match` | `exact_and_loose_modifier_matching` |
| Poller: focus gate clears edges | 2638-2644 | `OnKeyTable::poll` | `unfocused_clears_edges_and_does_not_fire` |
| Wrong modifiers treated as released | 2651-2656 | `OnKeyTable::poll` | `wrong_modifiers_then_right_modifiers_refires` |
| One fire per press | 2657-2659, 2722-2724 | `OnKeyTable::poll` | `fires_once_per_press_and_respects_exact_modifiers` |
| Per-script cooldown, wrapping clock, first throttle flagged | 2670-2687 | `fire`, `OnKeyEvent::Throttled { first }` | `cooldown_throttles_and_reports_first_only_flag` |
| Missing / directory file reported, not queued | 2695-2701 | `OnKeyEvent::MissingFile` | `missing_file_reported_and_not_queued` |
| Cooldown stamp set before load | 2703 | `fire` | `load_and_enqueue_failures_reach_the_sink` |
| Re-read and normalise on every press, then queue | 2710-2720 | `OnKeyHooks::normalize`, `OnKeyHooks::enqueue` | `fires_once_per_press_and_respects_exact_modifiers` |
| OnKey scan: ini value, else KEYVAL default written back | 2786-2793 | `scan_key_scripts` | `scan_extracts_defaults_writes_back_and_reports` |
| OnKey disable values | 2795-2801 | `KeyScan::disabled` | `scan_extracts_defaults_writes_back_and_reports` |
| Unrecognized names reported (not `unassigned`), kept, never fire | 2803-2814 | `KeyScan::unrecognized` | `scan_extracts_defaults_writes_back_and_reports`, `unassigned_bindings_never_fire` |
| Shared bindings reported | 2842-2856 | `KeyScan::shared` | `scan_extracts_defaults_writes_back_and_reports` |
| Table swapped whole; state reset | 2825-2829, 2858-2861 | `OnKeyTable::install` | `install_resets_edge_and_cooldown_state` |
| Rescan repeatable; poller spawned once | 2733-2741, 2863-2870 | `OnKeyTable::rescan`, `OnKeyTable::start_polling` (Windows) | `rescan_picks_up_new_scripts_with_no_other_precondition` |

## Fixes

- **Bare modifier bindings fire** (plan C.5 #2). C 2618-2625 with exact matching required
  `held == mods`; a `shift` binding is VK_SHIFT with `mods = 0`, but holding shift sets the SHIFT
  bit, so it could never fire. `modifiers_match` drops the bound key's own modifier from both
  sides. Tests: `bare_modifier_bindings_match_while_held`, `bare_shift_binding_fires_through_the_poller`.
- **OnKey results reach the feed** (#7). C 2717 queues with session 0 and the result is only
  `m2_logf`'d (C 2973-3000 path is not used for it). Every poller outcome goes to
  `OnKeyHooks::sink` as an `OnKeyEvent`, and `result_feed_line` gives the same
  `[OnKey] rel -> result` line OnBoot/OnLoad use; the integration mirrors both.
- **Re-arm does not depend on OnLoad** (#10). Nothing in these modules takes OnLoad state;
  `OnKeyTable::rescan` depends only on the folder and ini. The gate at C 1333 (`g_OnLoadExecuted`)
  is in the integration, not here.
- **Invalid order values are reported, not run as 0.** C 2294 used `GetPrivateProfileIntA`; a value
  like `1O` ran the script at an undefined order. `Order::Invalid` → `ScriptOutcome::InvalidOrder`.
- **Empty script files are reported.** C 2190 skipped a 0-byte file without a log line;
  `load_script_bytes` returns an error.
- **Unreadable KEYVAL source is reported.** C 1868-1869 silently fell back to `unassigned`;
  `KeyScan::failed`.
- **Ini write failures are errors.** C ignored `WritePrivateProfileStringA`'s result.
- **Focus-gate edge clear is under the table lock.** C 2641 wrote `was_down` outside the critical
  section.
- **"Never fired" cooldown sentinel.** C used `last_fired_tick != 0`; a fire at tick 0 disabled the
  next cooldown. Rust uses `Option`.
- **Deterministic enumeration.** Each directory is listed in case-insensitive name order, so which
  files survive the 128 cap does not depend on filesystem enumeration order.
- **Script paths with no ANSI form are a hard error.** The `A` profile APIs wrote such a path with
  best-fit or `?` substitutions, so the key never matched the file again. `LoaderIni::save` encodes
  with `WC_NO_BEST_FIT_CHARS` and refuses, naming the ini file and the offending line (which holds the
  script path); the ini is left unchanged.
- **Strict F-key names.** C 1930-1933 parsed the number with `atoi`, so `F5x` bound F5 and `F05` bound
  F5. Those names are now unrecognized and reported through `KeyScan::unrecognized`.

## Differences to know about

- Bytes in the ini that are invalid in the process code page (`MB_ERR_INVALID_CHARS`) are an error.
  Script/directory names that are not valid Unicode are reported in `ScanReport::unreadable_names`.
- Keys are matched ASCII-case-insensitively; the profile APIs fold non-ASCII letters by locale.
  A key that differs from the file name only in the case of a non-ASCII letter does not match.
- The ini reader/writer is our own (Windows profile-file conventions); new keys go after a section's
  last non-blank line, new sections at the end, using the file's line ending.
- The executor's chunk-size cap (C 2190) is enforced by the caller's `normalize` closure.
- The C value buffers truncate at 63 bytes (C 2003-2014, 2782-2787); Rust does not truncate, so an
  overlong spec is unrecognized rather than truncated.
