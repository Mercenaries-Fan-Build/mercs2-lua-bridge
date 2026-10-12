# Namespaces parity: `Loader`, `Tcp`, `math`, polyfill

Source: `mods/lua-bridge-DEV/lua_bridge_DEV.c` (v0.5.4) and `sdk/m2/m2_luastack.c`. Rust: `client/src/loader_ns.rs`,
`tcp_ns.rs`, `math_ns.rs`, `polyfill.rs`. Tests are `cargo test` names inside each module's `tests`.

## Loader

| Behaviour | C lines | Rust item | Test |
|---|---|---|---|
| Table holds 11 functions: Printf, WsSend, GetKeyboardState, IsKeyDown, PopKeyEvents, ClearKeyEvents, IsGameFocused, SaveVar, LoadVar, GetLoadPhase, LoadFile | 3601-3614 | `loader_ns::LOADER_FUNCTIONS` | `function_list_is_the_registered_set` |
| String args joined with `\t`, non-strings skipped, byte budget `out_max` | m2_luastack.c 131-145 | `join_string_args` | `join_matches_the_byte_budget`, `printf_joins_string_args_with_tabs_and_skips_others` |
| Printf: budget 2048-32 (line <= 2015 bytes), appended + CRLF to `lua_loader_printf.log`, broadcast `log` | 2924-2967 | `printf`, `LoaderHost::printf_line` | `printf_caps_the_line_at_2015_bytes` |
| Printf with no string args writes `[Loader.Printf: no string args (joined=0)]` | 2937-2940 | `printf` | `printf_without_string_args_writes_the_marker_line` |
| WsSend: budget 2048-2, broadcast `ws`, never the log file | 4563-4569 | `ws_send`, `LoaderHost::ws_send_line` | `ws_send_broadcasts_the_joined_line_and_raises_without_strings` |
| GetKeyboardState: 256 bytes, `0x80` when `GetAsyncKeyState & 0x8000` | 3009-3024 | `get_keyboard_state`, `LoaderHost::key_down` | `keyboard_state_is_a_256_byte_plain_string` |
| IsKeyDown: number arg, `(int)` truncation, 0..=255, boolean | 3035-3063 | `is_key_down` | `is_key_down_returns_a_boolean`, `is_key_down_rejects_bad_keys` |
| PopKeyEvents: drains ring in press order, `""` when empty | 3080-3093, 2468-2479 | `pop_key_events` | `pop_key_events_returns_a_plain_string_and_empty_when_drained` |
| ClearKeyEvents: empties ring, returns nothing | 3102-3111 | `clear_key_events` | `clear_key_events_empties_the_ring_and_returns_nothing` |
| IsGameFocused: foreground window's PID is ours | 3122-3131, 2484-2491 | `is_game_focused` | `is_game_focused_returns_a_boolean` |
| SaveVar: number/boolean/string with type kept, persisted at once | 3394-3433 | `save_var` -> `LoaderHost::save_var` (`store::Store`) | `save_and_load_var_round_trip_each_type` |
| SaveVar(key, nil) deletes the key and persists (the C ignored nil, 3426-3428) | 3426-3428 | `save_var` -> `LoaderHost::delete_var` (`Store::remove`) | `save_var_nil_deletes_the_key` |
| LoadVar: saved type back, nil when missing | 3437-3479 | `load_var` | `save_and_load_var_round_trip_each_type` |
| GetLoadPhase: index + name, `-1, "none"` before a load | 3497-3518 | `get_load_phase`, `LoaderHost::load_phase` | `get_load_phase_returns_index_and_plain_string_name` |
| LoadFile: path refused if empty, `X:`, leading `\` or `/`, or containing `..` | 3557-3560 | `check_script_path` | `load_file_refuses_paths_outside_scripts` |
| LoadFile: nesting refused | 3554-3556 | `load_file` (thread-local depth gate) | `load_file_inside_load_file_raises_and_the_gate_reopens` |
| LoadFile: runs `scripts/<rel>`, logs `[LoadFile] rel -> result` on the Printf channels, true; false when not found | 3561-3598 | `load_file`, `LoaderHost::run_script`, `ScriptRun` | `load_file_runs_found_files_and_reports_the_result` |
| Returned strings are interned (`push_string`) | 3020, 3089, 3467, 3513 | every string push | `keyboard_state_...`, `pop_key_events_...`, `save_and_load_var_...`, `get_load_phase_...` |

## Tcp

| Behaviour | C lines | Rust item | Test |
|---|---|---|---|
| `Tcp.Send(host, port, msg)` writes msg to a TCP connection, then closes | 1666-1718, 1720-1723 | `tcp_ns::send` | `sends_the_message_to_a_loopback_listener` |
| Loopback 127.0.0.0/8 only | 1697-1700 | `parse_loopback_host` | `non_loopback_and_unparsable_hosts_raise` |
| 500 ms timeout | 1709-1710 (send only) | `TIMEOUT` on connect and write | `refused_connection_raises_promptly` |
| Port from arg 2, `(int)` truncation | 1675-1678 | `port_arg` | `non_number_port_raises`, `out_of_range_port_raises` |

## math

| Behaviour | C lines | Rust item | Test |
|---|---|---|---|
| 20 registered names | 3825-3847 | `math_ns::MATH_FUNCTIONS` | `function_names_are_the_registered_set` |
| sin cos tan asin acos atan sinh cosh tanh sqrt log log10 in f32; number-tag check | 3655-3700 | `sin`..`log10` (`one_arg`) | `one_arg_functions_compute_in_f32`, `non_number_arguments_raise` |
| FreedomUnits = m * 1.09361f | 3703-3704 | `freedom_units` | `freedom_units_is_metres_times_1_09361` |
| atan2, fmod (fmodf) | 3671-3687, 3706-3707 | `atan2`, `fmod` | `two_arg_functions` |
| ldexp(x, (int)e) | 3710-3727 | `ldexp`, `ldexp_f32` | `ldexp_truncates_the_exponent_and_rounds_once` |
| modf -> ipart, fpart | 3730-3748 | `modf`, `modf_f32` | `modf_returns_integer_then_fraction_with_sign` |
| frexp -> mantissa, exponent | 3751-3769 | `frexp`, `frexp_f32` | `frexp_returns_mantissa_then_exponent` |
| CRT `rand`/`srand` (msvcrt LCG 214013/2531011, RAND_MAX 32767, per thread, seed 1) | 3786-3821 | `crt_rand`, `crt_srand` | `crt_generator_matches_the_c_runtime_sequence` |
| random(n) = 1 + rand()%n; random(m,n) = m + rand()%(n-m+1) | 3787-3803 | `random` | `random_with_bounds_uses_modulo_of_rand`, `random_full_i32_span_does_not_overflow` |
| randomseed((unsigned)x) | 3814-3823 | `randomseed` | `randomseed_seeds_the_generator` |

## Polyfill

| Behaviour | C lines | Rust item | Test |
|---|---|---|---|
| Chunk text byte-identical (260 bytes, checked against the C literal compiled with `cc`) | 3887-3900 | `polyfill::POLYFILL` | `polyfill_text_is_exact` |
| Ran on mercs2_luac's float VM: math.pi, math.huge = inf, assert returns v, level-2 location, default message, idempotent | 3887-3900 | `POLYFILL` | one-off check outside the crate (not in `cargo test`) |

## Fixes

- **#1** `math.random()` returns `(rand() % 32767) / 32767`, in [0,1) (stock Lua 5.1 formula). C (3786) used `rand()/RAND_MAX`, which reaches 1.0.
- **#8** Every returned string is pushed with `push_string` (interned). C pushed hand-built TStrings, reused buffers (3020, 3089, 3513).
- **#11** Tcp connect has a 500 ms timeout. C's `SO_SNDTIMEO` (1710) covered only the send.
- **#12** Tcp port must be a Lua number (C read the slot unchecked, 1677); `localhost` maps to 127.0.0.1 (C's `inet_addr` refused it, 1682).
- Hard errors where C returned nothing without a message:
  - wrong or missing args on every Loader/math/Tcp function (3051-3055, 3399-3400, 3442-3443, 3551-3552, 3661-3662, 1671-1683);
  - IsKeyDown outside 0..=255;
  - SaveVar with an unsupported value type (3426-3428), or a key or value the file cannot hold (store.rs);
  - LoadFile with a refused path or a nested call (3554-3560; C logged these and returned false);
  - Tcp to a non-loopback host, an unparsable host, a port outside 1..=65535, a refused or failed connect, or a failed write (1698-1714);
  - random with an empty interval (3792, 3801) or 3+ args;
  - a truncated integer argument that does not fit an i32/u32;
  - frexp of inf/NaN;
  - WsSend with no string args (4566);
  - a Printf/LoadFile log line the host cannot write (2930).
- SaveVar of `""` is stored. C skipped it (3418).
- `random(m, n)` computes the span in i64. C's `hi - lo + 1` could overflow (3802).
