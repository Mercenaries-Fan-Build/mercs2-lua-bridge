# lua-bridge

A live line into *Mercenaries 2: World in Flames*' Lua: a REPL and a WebSocket on `127.0.0.1:27050` that
run Lua chunks on the game thread, a script loader for boot-time, level-load and hotkey scripts, and the
`Loader`, `Tcp` and `math` namespaces those scripts use.

Originally written by **loganw234** (Wally) as `lua_bridge_DEV`, and maintained as a Shipment by
Mercenaries Fan Build. Version 0.6.0 is a Rust rewrite on the shared m2-sdk runtime; see the
[CHANGELOG](CHANGELOG.md) for what changed.

> [!TIP]
> Script samples, function references and tutorials are on the wiki: [wiki.mercs2.tools](https://wiki.mercs2.tools).
> The Shipment ships only the bridge, never sample scripts, so an update cannot touch your own scripts.

## Install

Install the `lua-bridge` Shipment with Modkit (or `qm`); it requires the `m2-sdk` Shipment `^0.4`, which
places `m2-sdk.dll` in the game folder. The bridge is `scripts/lua_bridge.asi`.

If you installed the old `lua_bridge_DEV.asi` by hand, delete it from `scripts/` first: two bridges would
both try to listen on the port, and `qm build` refuses while the old file is there.

## The REPL

Connect to `127.0.0.1:27050` and send a chunk followed by a line holding only `<<<RUN>>>`:

```text
client → return 1 + 1
client → <<<RUN>>>
bridge ← [queued]
bridge ← [ok]	2
bridge ← <<<END>>>
```

Lines end at `\n` (a trailing `\r` is dropped). The chunk runs on the game's next frame, on the game
thread. The result line is `[ok]` followed by the chunk's return values, or `[compile] …` / `[runtime] …`
with the error. A chunk that cannot be accepted (over 1 MiB, or text the encoding step refuses) is
answered with `[bridge] …` and `<<<END>>>`, without `[queued]`. One REPL client is served at a time; a
second waits until the first disconnects.

Clients: `mercs2_repl --code 'return 1+1'` (from the mercs2-wad-simulator workspace) or any program that
speaks the framing above.

## The WebSocket

A connection to the same port that starts with `GET ` is a WebSocket (up to 16 clients). Send
`{"id":"…","code":"…"}`; the bridge answers `{"type":"ack","id":"…","status":"queued"}` (or
`"status":"rejected"` with an `"error"`) and runs the chunk. Every client receives the live feed:

- `{"type":"log","line":"…"}` — each `Loader.Printf` line, and each OnBoot/OnLoad/OnKey script's result;
- `{"type":"ws","line":"…"}` — each `Loader.WsSend` line.

A chunk's own result is not sent back over the WebSocket; a script that wants to answer calls
`Loader.WsSend`. Set `websocket_enabled = 0` to accept raw REPL connections only.

## Lua namespaces

### `Loader`

| Function | Returns |
|---|---|
| `Loader.Printf(...)` | Appends its string arguments, tab-separated, as one line to `scripts/lua_loader_printf.log`, and sends it on the WebSocket `log` feed. |
| `Loader.WsSend(...)` | Sends its string arguments, tab-separated, on the WebSocket `ws` feed. |
| `Loader.GetKeyboardState()` | A 256-byte string; byte `vk + 1` is `0x80` while that key is held. |
| `Loader.IsKeyDown(vk)` | Whether the key is held. |
| `Loader.PopKeyEvents()` | Every key pressed since the last call, one byte per press, oldest first (`""` when none). Filled by a 60 Hz sampler into a 128-press ring; presses are ignored while the game is not the foreground window. |
| `Loader.ClearKeyEvents()` | Empties the ring. |
| `Loader.IsGameFocused()` | Whether the foreground window belongs to the game. |
| `Loader.SaveVar(key, value)` | Saves a number, string or boolean under `key` in `scripts/lua_loader_data.ini`, at once; `nil` deletes the key. Keys are one flat namespace: prefix them with your script's name. |
| `Loader.LoadVar(key)` | The saved value, with its type, or `nil`. |
| `Loader.GetLoadPhase()` | `index, name` on the 0–20 world-load ladder, `-1, "none"` before a load is seen; `11 "Player spawn"` and `20 "World fully loaded (GlobalExit)"` are the useful checkpoints. Starts again on each load. Needs `loader_track_phase = 1`. |
| `Loader.LoadFile(relPath)` | Runs `scripts/<relPath>` on the current state and returns `true`, or `false` when the file does not exist. Errors inside the file are logged and sent on the feed, not raised. The path must stay inside `scripts/`, and `LoadFile` cannot be nested. |

Wrong arguments raise a Lua error; use `pcall` where a script must carry on.

> [!IMPORTANT]
> Do not call `Loader.Printf` or `Tcp.Send` in a per-frame loop. Each `Printf` is a file write (about
> 5 ms with antivirus scanning) and each `Tcp.Send` a TCP connection. Occasional use is fine.

### `Tcp`

`Tcp.Send(host, port, msg)` connects to `host:port`, writes `msg` and closes. Only `127.0.0.0/8` and
`localhost` are allowed. The connect and the write each give up after 500 ms.

### `math`

The game's Lua is built without most of `math`; the bridge adds `sin`, `cos`, `tan`, `asin`, `acos`,
`atan`, `atan2`, `sinh`, `cosh`, `tanh`, `sqrt`, `log`, `log10`, `fmod`, `ldexp`, `modf`, `frexp`,
`random`, `randomseed`, `FreedomUnits`, and the constants `math.pi` and `math.huge`. `assert` is added
when the game lacks it.

## The script loader

Scripts go in three folders under `<game>/scripts/`, created on first run:

- **`OnBoot/`** — run when the game's first Lua state is captured.
- **`OnLoad/`** — run when the world has finished loading (the `GlobalExit - Complete` milestone).
- **`OnKey/`** — run when their hotkey is pressed. A thread polls the keys at 30 Hz and reads the file on
  each press, so editing a bound script takes effect on the next press.

Folders are searched recursively (8 levels, 128 scripts). A directory whose name starts with `_` is not
searched, so shared code can live in, say, `scripts/OnLoad/_lib/util.lua` and be pulled in with
`Loader.LoadFile("OnLoad/_lib/util.lua")`.

`scripts/lua_loader.ini` lists every script found. Under `[OnBoot]` and `[OnLoad]` the value is the run
order (lowest first; new scripts are given one); `off`, `no` or `disabled` skips a script. Under
`[OnKey]` the value is the hotkey. A script can declare its default with `local KEYVAL = "ctrl+F5"` in
its first 10 lines.

```ini
[OnBoot]
print_boot.lua = 10

[OnLoad]
print_load.lua = 10

[OnKey]
test_key.lua = insert
```

**Hotkeys.** Letters, digits, `F1`–`F24`, `numpad0`–`numpad9`, `numpadplus`/`minus`/`multiply`/`divide`/
`decimal`, `insert`, `delete`, `home`, `end`, `pageup`, `pagedown`, `space`, `enter`, `escape`,
`backspace`, `tab`, the arrows, `shift`/`ctrl`/`alt` (and `lshift`, `rshift`, `lctrl`, `rctrl`, `lalt`,
`ralt`), `capslock`, `numlock`, `scrolllock`, `pause`, `printscreen`, `tilde`, `minus`, `plus`,
`lbracket`, `rbracket`, `backslash`, `semicolon`, `quote`, `comma`, `period`, `slash`, `mouse4`,
`mouse5`, with any of the prefixes `ctrl+`, `shift+`, `alt+`. Modifiers must match exactly, so `F5` and
`ctrl+F5` can run different scripts (`loader_onkey_exact_modifiers = 0` matches any modifiers). Keys are
ignored while the game is not focused (`loader_onkey_require_focus = 0` turns that off). The same script
does not fire again within 250 ms (`loader_onkey_cooldown_ms`). Unrecognized names and keys bound by two
scripts are logged.

**Main-menu trips.** Leaving to the main menu wipes the game's Lua globals, and the next load builds new
Lua states. When a state appears that the game has never had before, after a load has completed, the
loader starts over: OnBoot runs again, OnLoad runs again at the next milestone, and `OnKey/` is scanned
again, so scripts added since start-up are picked up. Moving between levels does not trigger it.
`loader_rearm_on_menu = 0` runs each folder once per game session.

## Text encoding

The game draws text as single-byte CP1252. The loader therefore removes a UTF-8 byte-order mark (Lua
5.1 cannot compile one) and converts a UTF-8 file that contains non-ASCII text to CP1252, so `Möbius`
does not become `MÃ¶bius`. ASCII and already-CP1252 files are left alone; a UTF-16 file is refused. The
same step applies to REPL and WebSocket chunks. `loader_source_encoding = off` turns it off.

> [!IMPORTANT]
> In the game's Lua, `"\ddd"` string escapes are **octal**: `ö` is `"\366"`, not `"\246"`.

## Configuration (`scripts/lua_bridge.ini`)

Written with these defaults when missing. A key the bridge does not know, or a value it cannot read, stops
the game with a message naming the key.

```ini
[repl]
host = 127.0.0.1
port = 27050

[loader]
loader_enabled = 1
loader_onboot = 1
loader_onload = 1
loader_onkey_cooldown_ms = 250
loader_onkey_require_focus = 1
loader_onkey_exact_modifiers = 1
loader_rearm_on_menu = 1
loader_track_phase = 1
loader_source_encoding = auto        ; auto | off
watchdog_stuck_ms = 8000             ; 0 turns the watchdog off
websocket_enabled = 1
```

`loader_delay_ms` and `stack_diag` are gone: the first did nothing since 0.5.0, the second drove a
diagnostic of the old executor. Delete them from an existing `lua_bridge.ini`.

The watchdog checks every 2 s and writes to `lua_bridge.log` when a chunk has been running for longer
than `watchdog_stuck_ms`, or when chunks are queued but nothing runs them.

## Building

```sh
cd client
bash scripts/fetch_dw2_libgcc.sh   # once per Rust version
bash scripts/build_asi.sh          # → src/lua_bridge.asi
cargo test --lib                   # host tests
```

Needs the Rust toolchain `client/rust-toolchain.toml` pins and `i686-w64-mingw32-gcc`.

## Acknowledgements

- **loganw234** (Wally) wrote the bridge and its protocol.
- **u/Kunster_** on r/MercenariesGames described the Lua registration-table technique.
- **Tsuda Kageyu** for MinHook, used through m2-sdk.dll.

## License

MIT — see [LICENSE](LICENSE).
