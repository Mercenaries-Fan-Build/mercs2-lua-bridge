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
; Read from the folder lua_bridge.asi is in. A key the bridge does not know, or a value it
; cannot read, stops the game with a message naming the key.

[repl]
; Address the REPL listener binds. 127.0.0.1 = this machine only.
host = 127.0.0.1

; Port of the REPL listener (raw TCP and WebSocket share it).
port = 27050

[loader]
; The script loader as a whole (1 = on, 0 = off).
loader_enabled = 1

; Run scripts/OnBoot/ when the game's first Lua state is captured.
loader_onboot = 1

; Run scripts/OnLoad/ when the world has finished loading.
loader_onload = 1

; Minimum time in milliseconds between two fires of the SAME OnKey script, so a
; hammered hotkey cannot queue back-to-back runs of a script that is not reentrant.
; The first throttle per script is logged. 0 = no cooldown.
loader_onkey_cooldown_ms = 250

; Watchdog: report in lua_bridge.log when a chunk has run for longer than this many
; milliseconds, or when chunks are queued but nothing runs them. 0 = off.
watchdog_stuck_ms = 8000

; Accept WebSocket clients on the REPL port as well as raw TCP (told apart by the
; first bytes). WebSocket clients get {type:\"ack\"}, {type:\"log\"} (Loader.Printf
; and loader results) and {type:\"ws\"} (Loader.WsSend). 0 = raw TCP only.
websocket_enabled = 1

; Ignore OnKey hotkeys while the game is not the foreground window, so F5 in a
; browser does not run your scripts. Loader.PopKeyEvents is always focus-gated.
loader_onkey_require_focus = 1

; A binding fires only when the held ctrl/shift/alt set matches it exactly, so F5
; and ctrl+F5 can run different scripts. 0 = ignore which modifiers are held.
loader_onkey_exact_modifiers = 1

; Re-run OnBoot and OnLoad (and rescan OnKey) after a main-menu round-trip: the
; game wipes _G there and builds fresh Lua states for the next load. 0 = once per
; game session.
loader_rearm_on_menu = 1

; Follow the world-load phase ladder (0..20) for Loader.GetLoadPhase(). The game's
; log lines are only read while a load is in progress. 0 = GetLoadPhase reports none.
loader_track_phase = 1

; Source encoding of .lua files and REPL/WebSocket chunks. The game draws text as
; single-byte CP1252: auto strips a UTF-8 byte-order mark and converts UTF-8 text with
; non-ASCII characters to CP1252, leaving ASCII and CP1252 files alone. off = only
; the byte-order mark is stripped. A UTF-16 file is refused either way.
loader_source_encoding = auto

; Engine Lua modules to import before any OnBoot/OnLoad/OnKey script or REPL chunk
; runs, comma-separated, in order. A library Shipment ships its Lua as a module that
; nothing imports on its own, so each consumer would otherwise need a loose stub whose
; whole body is import(\"<module>\"). Listing the modules here runs those imports first,
; so the globals they export are there for every script and no stub is needed. For the
; Ess library this is: preimport = ess, ess_names
; A module in the level's script block cannot be imported in the front end, so each
; name's outcome is logged once per Lua state. Empty = import nothing.
preimport =
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
    /// `preimport`: engine Lua modules imported before any script or chunk runs, in order.
    ///
    /// Empty by default: a library Shipment declares the bridge as its dependency, so the bridge
    /// names no library of its own accord (see [`crate::preimport`]).
    pub preimport: Vec<String>,
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
            loader_onkey_cooldown_ms: 250,
            watchdog_stuck_ms: 8000,
            websocket_enabled: true,
            loader_onkey_require_focus: true,
            loader_onkey_exact_modifiers: true,
            loader_rearm_on_menu: true,
            loader_track_phase: true,
            loader_source_encoding: EncodingMode::Auto,
            preimport: Vec::new(),
        }
    }
}

/// Why a settings file was rejected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigError {
    /// A known key carries a value that does not parse as that key's type.
    InvalidValue { key: String, value: String, expected: &'static str },
    /// A key the bridge does not read: a typo, or a key an older version had.
    UnknownKey { key: String },
    /// The file is larger than the INI reader accepts (`mercs2_formats::ini::MAX_FILE`).
    TooLarge { len: usize },
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ConfigError::InvalidValue { key, value, expected } => {
                write!(f, "lua_bridge.ini: `{key}` = `{value}` is not valid; expected {expected}")
            }
            ConfigError::UnknownKey { key } => write!(
                f,
                "lua_bridge.ini: `{key}` is not a setting this version reads; remove it (the default file the \
                 bridge writes when lua_bridge.ini is missing lists every setting)"
            ),
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

/// `preimport`: a comma-separated module list. A name the generated `import()` chunk could not hold
/// is an error naming it, so a typo is reported rather than quietly dropped from the list.
fn parse_preimport(key: &[u8], value: &[u8]) -> Result<Vec<String>, ConfigError> {
    let text = std::str::from_utf8(value).map_err(|_| ConfigError::InvalidValue {
        key: String::from_utf8_lossy(key).into_owned(),
        value: String::from_utf8_lossy(value).into_owned(),
        expected: "a comma-separated list of module names (letters, digits and `_`)",
    })?;
    crate::preimport::parse_list(text).map_err(|_| ConfigError::InvalidValue {
        key: String::from_utf8_lossy(key).into_owned(),
        value: text.to_string(),
        expected: "a comma-separated list of module names (letters, digits and `_`)",
    })
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
    } else if is("preimport") {
        cfg.preimport = parse_preimport(key, value)?;
    } else {
        return Err(ConfigError::UnknownKey { key: String::from_utf8_lossy(key).into_owned() });
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
                "loader_onkey_cooldown_ms",
                "watchdog_stuck_ms",
                "websocket_enabled",
                "loader_onkey_require_focus",
                "loader_onkey_exact_modifiers",
                "loader_rearm_on_menu",
                "loader_track_phase",
                "loader_source_encoding",
                "preimport",
            ]
        );
    }

    #[test]
    fn default_ini_is_ascii() {
        assert!(DEFAULT_INI.is_ascii());
        assert!(DEFAULT_INI.starts_with("; lua-bridge configuration.\n"));
        assert!(DEFAULT_INI.ends_with("preimport =\n"));
    }

    #[test]
    fn every_key_is_read() {
        let text = "host=0.0.0.0\nport=1\nloader_enabled=0\nloader_onboot=off\nloader_onload=FALSE\n\
                    loader_onkey_cooldown_ms=7\nwatchdog_stuck_ms=2147483647\n\
                    websocket_enabled=Off\nloader_onkey_require_focus=0\nloader_onkey_exact_modifiers=0\n\
                    loader_rearm_on_menu=0\nloader_track_phase=0\nloader_source_encoding=raw\n\
                    preimport=ess, ess_names\n";
        let cfg = parse(text.as_bytes()).unwrap();
        assert_eq!(
            cfg,
            Config {
                host: Ipv4Addr::new(0, 0, 0, 0),
                port: 1,
                loader_enabled: false,
                loader_onboot: false,
                loader_onload: false,
                loader_onkey_cooldown_ms: 7,
                watchdog_stuck_ms: 2_147_483_647,
                websocket_enabled: false,
                loader_onkey_require_focus: false,
                loader_onkey_exact_modifiers: false,
                loader_rearm_on_menu: false,
                loader_track_phase: false,
                loader_source_encoding: EncodingMode::Raw,
                preimport: vec!["ess".to_string(), "ess_names".to_string()],
            }
        );
    }

    #[test]
    fn preimport_reads_a_list_and_refuses_a_name_a_chunk_cannot_hold() {
        assert_eq!(parse(b"preimport = ess, ess_names").unwrap().preimport, ["ess", "ess_names"]);
        assert_eq!(parse(b"preimport =").unwrap().preimport, Vec::<String>::new());
        assert!(parse(b"").unwrap().preimport.is_empty(), "absent means import nothing");
        // A typo must name itself rather than silently shorten the list.
        for bad in ["preimport = ess names", "preimport = ess, a\"b", "preimport = ess.names"] {
            assert!(matches!(err_of(bad), ConfigError::InvalidValue { .. }), "{bad}");
        }
    }

    #[test]
    fn keys_match_case_insensitively_and_last_value_wins() {
        let cfg = parse(b"[a]\nPORT = 1000\n[b]\nPort=2000\n").unwrap();
        assert_eq!(cfg.port, 2000);
    }

    #[test]
    fn unknown_keys_are_errors() {
        for key in ["some_other_tool", "stack_diag", "loader_delay_ms", "prot"] {
            assert_eq!(
                err_of(&format!("{key} = 1\n")),
                ConfigError::UnknownKey { key: key.into() },
                "{key}"
            );
        }
        assert!(err_of("stack_diag = 1").to_string().contains("`stack_diag` is not a setting this version reads"));
    }

    #[test]
    fn crlf_file_parses() {
        let crlf = DEFAULT_INI.replace('\n', "\r\n");
        assert_eq!(parse(crlf.as_bytes()).unwrap(), Config::default());
    }

    #[test]
    fn bool_keys_accept_exact_words_only() {
        for (v, want) in [("1", true), ("TRUE", true), ("On", true), ("0", false), ("False", false), ("OFF", false)] {
            assert_eq!(parse(format!("websocket_enabled={v}").as_bytes()).unwrap().websocket_enabled, want, "{v}");
        }
        for v in ["2", "yes", "no", "", "-1", "01", "1x", "enabled"] {
            let e = err_of(&format!("websocket_enabled={v}"));
            assert_eq!(
                e,
                ConfigError::InvalidValue { key: "websocket_enabled".into(), value: v.into(), expected: BOOL_EXPECTED },
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
            ("loader_onkey_cooldown_ms", "-5"),
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
