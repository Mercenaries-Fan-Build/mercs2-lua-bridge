//! `lua_bridge.ini`: the bridge's settings file.
//!
//! The file is read with `mercs2_formats::ini::parse` (section-blind, `;`/`#` comments, first `=` splits).
//! Keys match case-insensitively and a repeated key takes its last value. Keys this module does not know
//! are ignored, so the file may carry settings for other tools. Every known key's value is checked: a value
//! that does not parse is an error naming the key and the value, never a silent default.

use std::fmt;
use std::fs;
use std::io::ErrorKind;
use std::net::Ipv4Addr;
use std::path::Path;

use mercs2_formats::ini;

/// The file written next to the ASI when no settings file exists.
pub const DEFAULT_INI: &str = "; lua-bridge configuration.
; Drop this next to lua_bridge.asi in your game folder.

[repl]
; Host to bind the REPL listener on. 127.0.0.1 = localhost-only,
; which is what you want unless you're routing through a tunnel.
host = 127.0.0.1

; Port for the REPL listener. Matches the default the upstream
; tools/lua_repl.py and tools/lua_console.py expect.
port = 27050

[loader]
; Enable or disable the native Lua Loader (1 = enabled, 0 = disabled)
loader_enabled = 1

; Load scripts in scripts/OnBoot/ once captured (1 = enabled, 0 = disabled)
loader_onboot = 1

; Load scripts in scripts/OnLoad/ once game enters world (1 = enabled, 0 = disabled)
loader_onload = 1

; DEPRECATED in v0.5.0 and now a no-op. ExecuteLuaFolder runs ON the game
; thread, so sleeping between scripts could never let the engine settle
; the way this name suggests -- the engine IS the thread being slept. All
; it bought was a delay * (scripts-1) freeze on top of every load. Still
; parsed so existing .ini files stay valid.
loader_delay_ms = 50

; Minimum time (in milliseconds) between the SAME OnKey script re-firing.
; Prevents human hammer-tapping the hotkey from queueing multiple back-to-back
; runs of the same script, which can crash non-reentrant scripts (menus,
; state-mutating cheats, etc). First throttle per script is logged so users
; can see it kick in; subsequent throttles are silent to avoid log spam.
; Set to 0 to disable the cooldown entirely.
loader_onkey_cooldown_ms = 250

; Watchdog: if the queue has pending chunks and no pump progress happens
; for this many milliseconds (while the game is actively running detours),
; a background thread force-resets the bridge's stuck-state candidates
; (hotWork, t_inBridgeExec, g_LuaState, seen-set) and logs a comprehensive
; diagnostic line. Set to 0 to disable the watchdog entirely.
watchdog_stuck_ms = 8000

; WebSocket transport (1 = enabled, 0 = disabled). When enabled, the same
; listener speaks either raw TCP (existing lua_repl.py / lua_console.py
; protocol) or WebSocket \u{2014} auto-detected from the first bytes. Browser
; clients (tools/ess-bridge.js) connect via WS and receive structured
; JSON messages: {type:\"ack\"}, {type:\"log\"} (mirrors Loader.Printf),
; and {type:\"ws\"} (the hidden Loader.WsSend channel). Disable this if
; you want the bridge locked to raw-TCP only.
websocket_enabled = 1

; --- added in v0.5.0 ---

; Drop OnKey hotkey fires while the game is NOT the foreground window.
; Before 0.5.0 hotkeys fired regardless of focus, so pressing F5 in a
; browser (refresh) or F11 (fullscreen) silently ran your game scripts in
; the background. Loader.PopKeyEvents has always been focus-gated; this
; makes the .lua-firing path agree with it. 0 = old behavior.
loader_onkey_require_focus = 1

; Require the held modifier set to match a binding EXACTLY, so you can
; bind `F5` and `ctrl+F5` to two different scripts. Side effect: a plain
; `F5` binding no longer fires while ctrl/shift/alt is held. 0 = ignore
; modifiers entirely (pre-0.5.0 matching).
loader_onkey_exact_modifiers = 1

; Re-run OnBoot/OnLoad after a main-menu round-trip. The engine wipes _G
; when you exit to the menu and builds fresh Lua VMs on the next load, so
; without this every OnLoad-installed framework stays dead until you
; restart the game. Detected by watching for a world-load-start marker
; after the world had already finished loading. 0 = once per process.
loader_rearm_on_menu = 1

; Track the world-load phase ladder (0..20) so Loader.GetLoadPhase()
; works and the re-arm above can see a load restart. Costs a substring
; scan per captured game log line on the detour path. 0 removes that cost
; but disables GetLoadPhase and the menu re-arm.
loader_track_phase = 1

; Source-encoding normalization for .lua files and WS-submitted chunks.
; The engine's text renderers consume single-byte CP1252, so a UTF-8
; source file renders \"Mobius\" (o-umlaut) as \"MA-bius\". auto strips BOMs
; and transcodes UTF-8 -> CP1252 only when the bytes are valid UTF-8 AND
; contain a byte >= 0x80, so pure-ASCII and already-ANSI files are never
; touched. off = raw bytes through (BOMs are still stripped, because a
; BOM is an unconditional Lua syntax error).
loader_source_encoding = auto

; One-shot Lua stack-layout dump on the first chunk execution. This build
; of Lua leaves a table where stock pcall leaves its boolean status, and
; the status is not in the window the bridge reads \u{2014} this dump exists to
; find where it went, and to confirm it is NOT being written into the
; engine's own stack frame. Costs one burst of log lines per process.
stack_diag = 1
";

/// How `.lua` source bytes are prepared before the game's Lua compiles them (see `crate::encoding`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EncodingMode {
    /// Strip a UTF-8 BOM, and transcode valid UTF-8 that contains a byte >= 0x80 to CP1252.
    Auto,
    /// Strip a UTF-8 BOM only; every other byte goes through unchanged.
    Raw,
}

/// Every setting in `lua_bridge.ini`, typed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    /// `host`: the IPv4 address the REPL listener binds.
    pub host: Ipv4Addr,
    /// `port`: the REPL listener's TCP port, 1..=65535.
    pub port: u16,
    /// `loader_enabled`: the script loader as a whole.
    pub loader_enabled: bool,
    /// `loader_onboot`: run `scripts/OnBoot/` once the Lua state is captured.
    pub loader_onboot: bool,
    /// `loader_onload`: run `scripts/OnLoad/` once the world has loaded.
    pub loader_onload: bool,
    /// `loader_delay_ms`: read and range-checked; it drives nothing.
    pub loader_delay_ms: u32,
    /// `loader_onkey_cooldown_ms`: minimum gap between two fires of the same OnKey script; 0 = no gap.
    pub loader_onkey_cooldown_ms: u32,
    /// `watchdog_stuck_ms`: how long the queue may sit without progress before the watchdog acts; 0 = off.
    pub watchdog_stuck_ms: u32,
    /// `websocket_enabled`: accept WebSocket clients on the REPL listener as well as raw TCP.
    pub websocket_enabled: bool,
    /// `loader_onkey_require_focus`: drop OnKey fires while the game is not the foreground window.
    pub loader_onkey_require_focus: bool,
    /// `loader_onkey_exact_modifiers`: a binding fires only when the held modifier set matches it exactly.
    pub loader_onkey_exact_modifiers: bool,
    /// `loader_rearm_on_menu`: run OnBoot/OnLoad again after a main-menu round-trip.
    pub loader_rearm_on_menu: bool,
    /// `loader_track_phase`: follow the world-load phase ladder from the game's log lines.
    pub loader_track_phase: bool,
    /// `loader_source_encoding`: `auto` or `raw`/`off`.
    pub loader_source_encoding: EncodingMode,
    /// `stack_diag`: the one-shot Lua stack-layout dump.
    pub stack_diag: bool,
}

impl Default for Config {
    /// The values `DEFAULT_INI` sets, which are also the values of any key a file leaves out.
    fn default() -> Self {
        Config {
            host: Ipv4Addr::new(127, 0, 0, 1),
            port: 27050,
            loader_enabled: true,
            loader_onboot: true,
            loader_onload: true,
            loader_delay_ms: 50,
            loader_onkey_cooldown_ms: 250,
            watchdog_stuck_ms: 8000,
            websocket_enabled: true,
            loader_onkey_require_focus: true,
            loader_onkey_exact_modifiers: true,
            loader_rearm_on_menu: true,
            loader_track_phase: true,
            loader_source_encoding: EncodingMode::Auto,
            stack_diag: true,
        }
    }
}

/// Why a settings file was rejected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigError {
    /// A known key carries a value that does not parse as that key's type.
    InvalidValue { key: String, value: String, expected: &'static str },
    /// The file is larger than the INI reader accepts (`mercs2_formats::ini::MAX_FILE`).
    TooLarge { len: usize },
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ConfigError::InvalidValue { key, value, expected } => {
                write!(f, "lua_bridge.ini: `{key}` = `{value}` is not valid; expected {expected}")
            }
            ConfigError::TooLarge { len } => {
                write!(f, "lua_bridge.ini: the file is {len} bytes; the INI reader accepts at most {} bytes", ini::MAX_FILE)
            }
        }
    }
}

impl std::error::Error for ConfigError {}

const BOOL_EXPECTED: &str = "one of 0, 1, true, false, on, off";
const MS_EXPECTED: &str = "a whole number of milliseconds from 0 to 2147483647";
const PORT_EXPECTED: &str = "a port number from 1 to 65535";
const HOST_EXPECTED: &str = "a dotted-quad IPv4 address such as 127.0.0.1";
const ENCODING_EXPECTED: &str = "one of auto, raw, off, on, true, false, 1, 0";

fn invalid(key: &[u8], value: &[u8], expected: &'static str) -> ConfigError {
    ConfigError::InvalidValue {
        key: String::from_utf8_lossy(key).into_owned(),
        value: String::from_utf8_lossy(value).into_owned(),
        expected,
    }
}

fn parse_bool(key: &[u8], value: &[u8]) -> Result<bool, ConfigError> {
    let is = |word: &str| value.eq_ignore_ascii_case(word.as_bytes());
    if is("1") || is("true") || is("on") {
        Ok(true)
    } else if is("0") || is("false") || is("off") {
        Ok(false)
    } else {
        Err(invalid(key, value, BOOL_EXPECTED))
    }
}

/// Decimal digits only, the whole value, within the range of a signed 32-bit int.
fn parse_digits(value: &[u8]) -> Option<u32> {
    if value.is_empty() || !value.iter().all(u8::is_ascii_digit) {
        return None;
    }
    let mut n: u32 = 0;
    for &b in value {
        n = n.checked_mul(10)?.checked_add(u32::from(b - b'0'))?;
        if n > i32::MAX as u32 {
            return None;
        }
    }
    Some(n)
}

fn parse_ms(key: &[u8], value: &[u8]) -> Result<u32, ConfigError> {
    parse_digits(value).ok_or_else(|| invalid(key, value, MS_EXPECTED))
}

fn parse_port(key: &[u8], value: &[u8]) -> Result<u16, ConfigError> {
    parse_digits(value)
        .and_then(|n| u16::try_from(n).ok())
        .filter(|&p| p != 0)
        .ok_or_else(|| invalid(key, value, PORT_EXPECTED))
}

fn parse_host(key: &[u8], value: &[u8]) -> Result<Ipv4Addr, ConfigError> {
    std::str::from_utf8(value)
        .ok()
        .and_then(|s| s.parse::<Ipv4Addr>().ok())
        .ok_or_else(|| invalid(key, value, HOST_EXPECTED))
}

fn parse_encoding(key: &[u8], value: &[u8]) -> Result<EncodingMode, ConfigError> {
    let is = |word: &str| value.eq_ignore_ascii_case(word.as_bytes());
    if is("auto") || is("1") || is("on") || is("true") {
        Ok(EncodingMode::Auto)
    } else if is("raw") || is("off") || is("0") || is("false") {
        Ok(EncodingMode::Raw)
    } else {
        Err(invalid(key, value, ENCODING_EXPECTED))
    }
}

/// Read settings from the bytes of a `lua_bridge.ini`. Keys the file leaves out keep their default.
pub fn parse(text: &[u8]) -> Result<Config, ConfigError> {
    if text.len() > ini::MAX_FILE {
        return Err(ConfigError::TooLarge { len: text.len() });
    }
    let mut cfg = Config::default();
    let mut first_error: Option<ConfigError> = None;
    ini::parse(text, |key, value| {
        if first_error.is_some() {
            return;
        }
        if let Err(e) = apply(&mut cfg, key, value) {
            first_error = Some(e);
        }
    });
    match first_error {
        Some(e) => Err(e),
        None => Ok(cfg),
    }
}

fn apply(cfg: &mut Config, key: &[u8], value: &[u8]) -> Result<(), ConfigError> {
    let is = |name: &str| key.eq_ignore_ascii_case(name.as_bytes());
    if is("host") {
        cfg.host = parse_host(key, value)?;
    } else if is("port") {
        cfg.port = parse_port(key, value)?;
    } else if is("loader_enabled") {
        cfg.loader_enabled = parse_bool(key, value)?;
    } else if is("loader_onboot") {
        cfg.loader_onboot = parse_bool(key, value)?;
    } else if is("loader_onload") {
        cfg.loader_onload = parse_bool(key, value)?;
    } else if is("loader_delay_ms") {
        cfg.loader_delay_ms = parse_ms(key, value)?;
    } else if is("loader_onkey_cooldown_ms") {
        cfg.loader_onkey_cooldown_ms = parse_ms(key, value)?;
    } else if is("watchdog_stuck_ms") {
        cfg.watchdog_stuck_ms = parse_ms(key, value)?;
    } else if is("websocket_enabled") {
        cfg.websocket_enabled = parse_bool(key, value)?;
    } else if is("loader_onkey_require_focus") {
        cfg.loader_onkey_require_focus = parse_bool(key, value)?;
    } else if is("loader_onkey_exact_modifiers") {
        cfg.loader_onkey_exact_modifiers = parse_bool(key, value)?;
    } else if is("loader_rearm_on_menu") {
        cfg.loader_rearm_on_menu = parse_bool(key, value)?;
    } else if is("loader_track_phase") {
        cfg.loader_track_phase = parse_bool(key, value)?;
    } else if is("loader_source_encoding") {
        cfg.loader_source_encoding = parse_encoding(key, value)?;
    } else if is("stack_diag") {
        cfg.stack_diag = parse_bool(key, value)?;
    }
    Ok(())
}

/// Write `DEFAULT_INI` to `path` when nothing exists there. Returns whether the file was written.
/// An existing file is never touched.
pub fn ensure_default(path: &Path) -> Result<bool, String> {
    match fs::metadata(path) {
        Ok(_) => Ok(false),
        Err(e) if e.kind() == ErrorKind::NotFound => {
            fs::write(path, DEFAULT_INI).map_err(|e| format!("writing default {}: {e}", path.display()))?;
            Ok(true)
        }
        Err(e) => Err(format!("checking {}: {e}", path.display())),
    }
}

/// Write the default file if `path` is missing, then read and parse it.
pub fn load(path: &Path) -> Result<Config, String> {
    ensure_default(path)?;
    let bytes = fs::read(path).map_err(|e| format!("reading {}: {e}", path.display()))?;
    parse(&bytes).map_err(|e| format!("{}: {e}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn err_of(text: &str) -> ConfigError {
        parse(text.as_bytes()).expect_err(text)
    }

    #[test]
    fn default_ini_parses_to_default() {
        assert_eq!(parse(DEFAULT_INI.as_bytes()).unwrap(), Config::default());
    }

    #[test]
    fn empty_file_is_all_defaults() {
        assert_eq!(parse(b"").unwrap(), Config::default());
    }

    #[test]
    fn default_ini_carries_every_key_once() {
        let mut keys = Vec::new();
        ini::parse(DEFAULT_INI.as_bytes(), |k, _| keys.push(String::from_utf8(k.to_vec()).unwrap()));
        assert_eq!(
            keys,
            [
                "host",
                "port",
                "loader_enabled",
                "loader_onboot",
                "loader_onload",
                "loader_delay_ms",
                "loader_onkey_cooldown_ms",
                "watchdog_stuck_ms",
                "websocket_enabled",
                "loader_onkey_require_focus",
                "loader_onkey_exact_modifiers",
                "loader_rearm_on_menu",
                "loader_track_phase",
                "loader_source_encoding",
                "stack_diag",
            ]
        );
    }

    #[test]
    fn default_ini_em_dashes_are_utf8_bytes() {
        let dash = "\u{2014}".as_bytes();
        assert_eq!(dash, [0xE2, 0x80, 0x94]);
        assert_eq!(DEFAULT_INI.as_bytes().windows(3).filter(|w| *w == dash).count(), 2);
        assert!(DEFAULT_INI.ends_with("stack_diag = 1\n"));
        assert!(DEFAULT_INI.starts_with("; lua-bridge configuration.\n"));
    }

    #[test]
    fn every_key_is_read() {
        let text = "host=0.0.0.0\nport=1\nloader_enabled=0\nloader_onboot=off\nloader_onload=FALSE\n\
                    loader_delay_ms=0\nloader_onkey_cooldown_ms=7\nwatchdog_stuck_ms=2147483647\n\
                    websocket_enabled=Off\nloader_onkey_require_focus=0\nloader_onkey_exact_modifiers=0\n\
                    loader_rearm_on_menu=0\nloader_track_phase=0\nloader_source_encoding=raw\nstack_diag=0\n";
        let cfg = parse(text.as_bytes()).unwrap();
        assert_eq!(
            cfg,
            Config {
                host: Ipv4Addr::new(0, 0, 0, 0),
                port: 1,
                loader_enabled: false,
                loader_onboot: false,
                loader_onload: false,
                loader_delay_ms: 0,
                loader_onkey_cooldown_ms: 7,
                watchdog_stuck_ms: 2_147_483_647,
                websocket_enabled: false,
                loader_onkey_require_focus: false,
                loader_onkey_exact_modifiers: false,
                loader_rearm_on_menu: false,
                loader_track_phase: false,
                loader_source_encoding: EncodingMode::Raw,
                stack_diag: false,
            }
        );
    }

    #[test]
    fn keys_match_case_insensitively_and_last_value_wins() {
        let cfg = parse(b"[a]\nPORT = 1000\n[b]\nPort=2000\n").unwrap();
        assert_eq!(cfg.port, 2000);
    }

    #[test]
    fn unknown_keys_are_ignored() {
        assert_eq!(parse(b"some_other_tool = banana\n").unwrap(), Config::default());
    }

    #[test]
    fn crlf_file_parses() {
        let crlf = DEFAULT_INI.replace('\n', "\r\n");
        assert_eq!(parse(crlf.as_bytes()).unwrap(), Config::default());
    }

    #[test]
    fn bool_keys_accept_exact_words_only() {
        for (v, want) in [("1", true), ("TRUE", true), ("On", true), ("0", false), ("False", false), ("OFF", false)] {
            assert_eq!(parse(format!("stack_diag={v}").as_bytes()).unwrap().stack_diag, want, "{v}");
        }
        for v in ["2", "yes", "no", "", "-1", "01", "1x", "enabled"] {
            let e = err_of(&format!("stack_diag={v}"));
            assert_eq!(
                e,
                ConfigError::InvalidValue { key: "stack_diag".into(), value: v.into(), expected: BOOL_EXPECTED },
                "{v:?}"
            );
        }
    }

    #[test]
    fn unparseable_host_is_an_error() {
        for v in ["localhost", "", "127.0.0", "256.0.0.1", "::1", "127.0.0.1:80"] {
            let e = err_of(&format!("host = {v}"));
            assert!(matches!(&e, ConfigError::InvalidValue { key, .. } if key == "host"), "{v:?}");
        }
        assert_eq!(parse(b"host=10.1.2.3").unwrap().host, Ipv4Addr::new(10, 1, 2, 3));
    }

    #[test]
    fn unparseable_or_out_of_range_numbers_are_errors() {
        for (k, v) in [
            ("port", "0"),
            ("port", "65536"),
            ("port", "abc"),
            ("port", "27050x"),
            ("port", "+27050"),
            ("port", "-1"),
            ("loader_delay_ms", "-5"),
            ("loader_onkey_cooldown_ms", "fast"),
            ("watchdog_stuck_ms", "2147483648"),
            ("watchdog_stuck_ms", ""),
            ("watchdog_stuck_ms", "8000.5"),
        ] {
            let e = err_of(&format!("{k}={v}"));
            assert!(matches!(&e, ConfigError::InvalidValue { key, value, .. } if key == k && value == v), "{k}={v}");
        }
        assert_eq!(parse(b"port=65535").unwrap().port, 65535);
    }

    #[test]
    fn encoding_words() {
        for (v, want) in [
            ("auto", EncodingMode::Auto),
            ("AUTO", EncodingMode::Auto),
            ("1", EncodingMode::Auto),
            ("raw", EncodingMode::Raw),
            ("Off", EncodingMode::Raw),
            ("0", EncodingMode::Raw),
        ] {
            assert_eq!(parse(format!("loader_source_encoding={v}").as_bytes()).unwrap().loader_source_encoding, want);
        }
        assert!(matches!(err_of("loader_source_encoding=utf8"), ConfigError::InvalidValue { .. }));
    }

    #[test]
    fn error_display_names_key_and_value() {
        let msg = err_of("port = banana").to_string();
        assert!(msg.contains("`port`"), "{msg}");
        assert!(msg.contains("`banana`"), "{msg}");
    }

    #[test]
    fn oversized_file_is_an_error() {
        let big = vec![b' '; ini::MAX_FILE + 1];
        assert_eq!(parse(&big), Err(ConfigError::TooLarge { len: ini::MAX_FILE + 1 }));
    }

    #[test]
    fn ensure_default_writes_once_and_load_reads_it() {
        let dir = std::env::temp_dir().join(format!("lb_cfg_test_{}", std::process::id()));
        if dir.exists() {
            fs::remove_dir_all(&dir).unwrap();
        }
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("lua_bridge.ini");
        assert!(ensure_default(&path).unwrap());
        assert_eq!(fs::read(&path).unwrap(), DEFAULT_INI.as_bytes());
        fs::write(&path, "port=1234\n").unwrap();
        assert!(!ensure_default(&path).unwrap());
        assert_eq!(load(&path).unwrap().port, 1234);
        fs::remove_dir_all(&dir).unwrap();
    }
}
