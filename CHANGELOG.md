# Changelog

## 1.0.0

The bridge is rewritten in Rust and ships as the `lua-bridge` Shipment. Its file is now
`scripts/lua_bridge.asi` (the old `lua_bridge_DEV.asi` must be removed; `qm build` refuses while it is
installed), its configuration `lua_bridge.ini` and its log `lua_bridge.log`. `lua_loader.ini`,
`lua_loader_data.ini` and `lua_loader_printf.log` keep their names and formats, so existing installs keep
their script order, hotkeys and saved variables.

### Built on m2-sdk.dll

The bridge requires the `m2-sdk` Shipment `^0.4` and hooks nothing itself. m2-sdk.dll finds every game
function it needs by signature (no per-exe address tables), captures the Lua states, registers the
`Loader`, `Tcp` and `math` namespaces on every state, runs chunks, and calls the bridge on the game thread.
Several mods can now use Lua together without hooking the same functions twice.

- An exe on which any of those functions cannot be found stops the game with a message naming it,
  instead of starting a bridge with some hooks missing.
- The load-phase scan listens to the game's log only while a load is in progress, and costs nothing
  during gameplay.
- `loader_delay_ms` (a no-op since 0.5.0) and `stack_diag` (a diagnostic of the old executor) are removed.
  Delete them from an existing `lua_bridge.ini`: a key the bridge does not know now stops the game with a
  message naming it.

### Fixes

**Lua functions**
- `math.random()` with no arguments returns a number in [0, 1); it could return 1.
- Strings returned to Lua (`GetKeyboardState`, `PopKeyEvents`, `GetLoadPhase`'s name, `LoadVar`) are
  ordinary interned Lua strings, so `==` compares them correctly; `LoadVar` no longer leaks.
- `Tcp.Send` checks that `port` is a number, accepts `localhost`, and gives up on a connect after 500 ms
  as well as on a send.
- `Loader.SaveVar(key, "")` stores the empty string; it was ignored.
- `Loader.SaveVar(key, nil)` deletes the key; it was ignored, so the old value stayed.

**Hotkeys (OnKey)**
- A binding to a bare `shift`, `ctrl` or `alt` fires; with exact modifiers it never could.
- OnKey script results go to the live log feed like OnBoot and OnLoad results.
- A key name such as `F5x`, `F05` or `F25` is reported as unrecognized; it was read as F5.
- An unreadable script, an empty script, or an order value in `lua_loader.ini` that is neither a number
  nor `off`/`no`/`disabled` is reported instead of being skipped or run at an arbitrary position.
- A script whose path cannot be written in the system code page is reported; the ini used to receive a
  substituted name that never matched the file again.

**Loader**
- OnBoot and OnKey re-arm after a main-menu trip even with `loader_onload = 0`.
- A script that hangs inside OnBoot or OnLoad is reported by the watchdog.
- Script order is decided the same way on every machine (directories are listed in name order before
  the 128-script cap applies).

**Saved variables (`lua_loader_data.ini`)**
- The file keeps the order variables were first saved in; it reversed on every save.
- Numbers are written with 9 significant digits, so they read back exactly; 6-digit values written by
  earlier versions, and their `1.#INF`-style infinities, still read.
- A line the bridge cannot read, or a save that fails, is an error instead of silently losing data.

**REPL and WebSocket**
- The WebSocket ack escapes the request `id`, so an id with quotes produces valid JSON, and a non-ASCII
  id comes back unchanged.
- Chunks sent over the REPL or WebSocket go through the same text-encoding step as script files.
- The game thread never waits on a socket. A WebSocket client that stops reading is disconnected once
  4 MiB is queued for it, without slowing the others.
- A REPL client that stays connected no longer blocks WebSocket clients from connecting; a connection
  that sends nothing for 5 s is closed.
- The 17th WebSocket client gets `503 Service Unavailable` instead of an accepted connection that is then
  closed.
- `[queued]` always arrives before its chunk's result. A result for a client that has disconnected is
  dropped instead of being handed to the next client.
- A chunk over 1 MiB, or a line over 64 KiB without a newline, is answered with an error; the chunk used
  to be cut short and run.

**Configuration (`lua_bridge.ini`)**
- An unreadable `host`, a port outside 1–65535, a negative time, or a switch other than
  `0/1/true/false/on/off` stops the game with a message naming the key; the bridge used to start with a
  different value (a bad `host` bound every interface). So does a key it does not know, which it used to
  ignore (a typo silently kept the default).

### Changed behaviour

These were silent before and now raise a Lua error (catch it with `pcall` where a script expects to
continue):
- a missing or wrongly typed argument to any `Loader`, `Tcp` or `math` function;
- `Loader.SaveVar` with no value or one that is not a number, string, boolean or `nil`, or a key or value
  the file cannot hold;
- `Loader.IsKeyDown` with a key outside 0–255;
- `Loader.LoadFile` with a path outside `scripts/` or from inside another `LoadFile` (it returned
  `false`);
- `Loader.WsSend` with no string arguments;
- `Tcp.Send` to a non-loopback address, or a connect or send that fails;
- `math.random` with an empty interval or more than two arguments, `math.frexp` of infinity or NaN.

A UTF-16 script file is an error (it used to reach the Lua compiler and fail there).
