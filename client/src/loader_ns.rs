//! The `Loader` table: logging, keyboard input, persisted variables, the world-load phase, and
//! loading shared script files.
//!
//! | Function | Returns |
//! |---|---|
//! | `Printf(...)` | nothing; the string arguments, tab-joined, become one line of `lua_loader_printf.log` and a `log` broadcast |
//! | `WsSend(...)` | nothing; the string arguments, tab-joined, become a `ws` broadcast |
//! | `GetKeyboardState()` | a 256-byte string, byte `vk` = `0x80` while the key is held, else `0x00` |
//! | `IsKeyDown(vk)` | a boolean |
//! | `PopKeyEvents()` | a string of the pressed virtual-key codes in press order, `""` when none |
//! | `ClearKeyEvents()` | nothing; empties the event ring |
//! | `IsGameFocused()` | a boolean: the foreground window belongs to the game process |
//! | `SaveVar(key, value)` | nothing; persists a number, string or boolean |
//! | `LoadVar(key)` | the saved value with its type, or `nil` |
//! | `GetLoadPhase()` | the phase index and name, `-1, "none"` before a load is observed |
//! | `LoadFile(relPath)` | `true` when the file ran, `false` when it does not exist |
//!
//! Every string goes back to Lua through `push_string`, so it is an interned Lua string and
//! compares with `==` like any other.

use mercs2_formats::mod_runtime::lua::{Lua, Ret, TBOOLEAN, TNUMBER, TSTRING, type_name};
use crate::store::Value;
use std::cell::Cell;

/// The `Loader` functions, in registration order.
pub const LOADER_FUNCTIONS: &[&str] = &[
    "Printf",
    "WsSend",
    "GetKeyboardState",
    "IsKeyDown",
    "PopKeyEvents",
    "ClearKeyEvents",
    "IsGameFocused",
    "SaveVar",
    "LoadVar",
    "GetLoadPhase",
    "LoadFile",
];

/// The `out_max` the `Printf` join is given; a line holds at most `PRINTF_JOIN_MAX - 1` bytes.
pub const PRINTF_JOIN_MAX: usize = 2048 - 32;
/// The `out_max` the `WsSend` join is given.
pub const WS_SEND_JOIN_MAX: usize = 2048 - 2;

/// What a `LoadFile` run did.
#[derive(Debug, Clone, PartialEq)]
pub enum ScriptRun {
    /// `scripts/<rel>` does not exist.
    NotFound,
    /// The file was read and executed; this is its result text (a runtime error inside the file
    /// is reported here, not raised).
    Ran(Vec<u8>),
}

/// The bridge state the `Loader` functions read and drive.
pub trait LoaderHost {
    /// Append `line` plus CRLF to `lua_loader_printf.log` and broadcast it as `{type:"log"}`.
    fn printf_line(&self, line: &[u8]) -> Result<(), String>;
    /// Broadcast `line` as `{type:"ws"}` to the connected WebSocket clients (none connected is not
    /// an error).
    fn ws_send_line(&self, line: &[u8]) -> Result<(), String>;
    /// Whether virtual key `vk` is physically held right now.
    fn key_down(&self, vk: u8) -> bool;
    /// Drain the key-event ring, oldest first.
    fn pop_key_events(&self) -> Vec<u8>;
    /// Empty the key-event ring.
    fn clear_key_events(&self);
    /// Whether the foreground window belongs to the game process.
    fn is_game_focused(&self) -> bool;
    /// Set one variable in `lua_loader_data.ini` and persist the file ([`crate::store::Store::set`]
    /// then [`crate::store::Store::persist`]); a key or value the file cannot hold is an `Err`.
    fn save_var(&self, key: &[u8], value: Value) -> Result<(), String>;
    /// Read one variable, `None` when the key is not set.
    fn load_var(&self, key: &[u8]) -> Result<Option<Value>, String>;
    /// The highest load-ladder phase reached in the current load cycle and its name, or `None`
    /// before one is observed.
    fn load_phase(&self) -> Option<(i32, Vec<u8>)>;
    /// Read `<game dir>/scripts/<rel>` and run it on the calling Lua state. `rel` has already been
    /// checked by [`check_script_path`]. Any failure other than the file not existing is an `Err`.
    fn run_script(&self, rel: &[u8]) -> Result<ScriptRun, String>;
}

/// The string arguments, in order and tab-separated, skipping every other type; and how many
/// string arguments there were. The result holds at most `out_max - 1` bytes, cut where the
/// space runs out.
pub fn join_string_args<L: Lua>(l: &L, out_max: usize) -> (Vec<u8>, usize) {
    let mut out = Vec::new();
    let mut count = 0;
    let mut i = 0;
    while l.arg_type(i) != -1 && out.len() + 2 < out_max {
        if l.arg_type(i) == TSTRING
            && let Some(bytes) = l.arg_string(i)
        {
            if count > 0 && out.len() + 1 < out_max {
                out.push(b'\t');
            }
            let room = out_max - out.len();
            if room > 1 {
                let take = bytes.len().min(room - 1);
                out.extend_from_slice(&bytes[..take]);
            }
            count += 1;
        }
        i += 1;
    }
    (out, count)
}

fn host_err(fname: &str, e: String) -> Ret {
    Ret::Raise(format!("Loader.{fname}: {e}"))
}

/// `Loader.Printf(...)`. With no string arguments the line is
/// `[Loader.Printf: no string args (joined=0)]`.
pub fn printf<L: Lua, H: LoaderHost>(l: &mut L, host: &H) -> Ret {
    let (mut line, count) = join_string_args(l, PRINTF_JOIN_MAX);
    if count == 0 {
        line = b"[Loader.Printf: no string args (joined=0)]".to_vec();
    }
    match host.printf_line(&line) {
        Ok(()) => Ret::Values(0),
        Err(e) => host_err("Printf", e),
    }
}

/// `Loader.WsSend(...)`. Raises when no argument is a string.
pub fn ws_send<L: Lua, H: LoaderHost>(l: &mut L, host: &H) -> Ret {
    let (line, count) = join_string_args(l, WS_SEND_JOIN_MAX);
    if count == 0 {
        return Ret::Raise("Loader.WsSend: no string arguments".to_string());
    }
    match host.ws_send_line(&line) {
        Ok(()) => Ret::Values(0),
        Err(e) => host_err("WsSend", e),
    }
}

/// `Loader.GetKeyboardState()`.
pub fn get_keyboard_state<L: Lua, H: LoaderHost>(l: &mut L, host: &H) -> Ret {
    let state: Vec<u8> = (0..=255u8)
        .map(|vk| if host.key_down(vk) { 0x80 } else { 0x00 })
        .collect();
    l.push_string(&state);
    Ret::Values(1)
}

/// `Loader.IsKeyDown(vk)`: `vk` is truncated toward zero and must be in 0..=255.
pub fn is_key_down<L: Lua, H: LoaderHost>(l: &mut L, host: &H) -> Ret {
    let tag = l.arg_type(0);
    if tag != TNUMBER {
        return Ret::Raise(format!(
            "bad argument #1 to 'Loader.IsKeyDown' (number expected, got {})",
            type_name(tag)
        ));
    }
    let Some(n) = l.arg_number(0) else {
        return Ret::Raise("bad argument #1 to 'Loader.IsKeyDown' (unreadable number)".to_string());
    };
    let vk = (n as f32).trunc();
    if !(0.0..=255.0).contains(&vk) {
        return Ret::Raise(format!(
            "bad argument #1 to 'Loader.IsKeyDown' (virtual key {n} is outside 0..=255)"
        ));
    }
    let down = host.key_down(vk as u8);
    l.push_bool(down);
    Ret::Values(1)
}

/// `Loader.PopKeyEvents()`.
pub fn pop_key_events<L: Lua, H: LoaderHost>(l: &mut L, host: &H) -> Ret {
    let events = host.pop_key_events();
    l.push_string(&events);
    Ret::Values(1)
}

/// `Loader.ClearKeyEvents()`.
pub fn clear_key_events<L: Lua, H: LoaderHost>(_l: &mut L, host: &H) -> Ret {
    host.clear_key_events();
    Ret::Values(0)
}

/// `Loader.IsGameFocused()`.
pub fn is_game_focused<L: Lua, H: LoaderHost>(l: &mut L, host: &H) -> Ret {
    let focused = host.is_game_focused();
    l.push_bool(focused);
    Ret::Values(1)
}

fn key_arg<L: Lua>(l: &L, fname: &str) -> Result<Vec<u8>, String> {
    let tag = l.arg_type(0);
    match (tag == TSTRING).then(|| l.arg_string(0)).flatten() {
        Some(k) => Ok(k),
        None => Err(format!(
            "bad argument #1 to 'Loader.{fname}' (string expected, got {})",
            type_name(tag)
        )),
    }
}

/// `Loader.SaveVar(key, value)`: `value` is a number, string or boolean.
pub fn save_var<L: Lua, H: LoaderHost>(l: &mut L, host: &H) -> Ret {
    let key = match key_arg(l, "SaveVar") {
        Ok(k) => k,
        Err(e) => return Ret::Raise(e),
    };
    let tag = l.arg_type(1);
    let value = match tag {
        TNUMBER => l.arg_number(1).map(|n| Value::Number(f64::from(n as f32))),
        TBOOLEAN => l.arg_bool(1).map(Value::Bool),
        TSTRING => l.arg_string(1).map(Value::Str),
        _ => None,
    };
    let Some(value) = value else {
        return Ret::Raise(format!(
            "bad argument #2 to 'Loader.SaveVar' (number, string or boolean expected, got {})",
            type_name(tag)
        ));
    };
    match host.save_var(&key, value) {
        Ok(()) => Ret::Values(0),
        Err(e) => host_err("SaveVar", e),
    }
}

/// `Loader.LoadVar(key)`.
pub fn load_var<L: Lua, H: LoaderHost>(l: &mut L, host: &H) -> Ret {
    let key = match key_arg(l, "LoadVar") {
        Ok(k) => k,
        Err(e) => return Ret::Raise(e),
    };
    match host.load_var(&key) {
        Ok(None) => l.push_nil(),
        Ok(Some(Value::Number(n))) => l.push_number(n),
        Ok(Some(Value::Bool(b))) => l.push_bool(b),
        Ok(Some(Value::Str(s))) => l.push_string(&s),
        Err(e) => return host_err("LoadVar", e),
    }
    Ret::Values(1)
}

/// `Loader.GetLoadPhase()`.
pub fn get_load_phase<L: Lua, H: LoaderHost>(l: &mut L, host: &H) -> Ret {
    match host.load_phase() {
        Some((index, name)) => {
            l.push_number(f64::from(index));
            l.push_string(&name);
        }
        None => {
            l.push_number(-1.0);
            l.push_string(b"none");
        }
    }
    Ret::Values(2)
}

/// Refuse a `LoadFile` path that is not relative to `scripts/`: empty, a drive (`X:`), rooted at
/// `\` or `/`, or containing `..` anywhere.
pub fn check_script_path(rel: &[u8]) -> Result<(), String> {
    let refused = rel.is_empty()
        || rel.get(1) == Some(&b':')
        || rel[0] == b'\\'
        || rel[0] == b'/'
        || rel.windows(2).any(|w| w == b"..");
    if refused {
        Err(format!(
            "'{}' refused: must be a path relative to scripts/ with no '..'",
            String::from_utf8_lossy(rel)
        ))
    } else {
        Ok(())
    }
}

thread_local! {
    static LOAD_FILE_DEPTH: Cell<u32> = const { Cell::new(0) };
}

struct DepthGuard;

impl DepthGuard {
    fn enter() -> Option<DepthGuard> {
        LOAD_FILE_DEPTH.with(|d| {
            if d.get() > 0 {
                None
            } else {
                d.set(1);
                Some(DepthGuard)
            }
        })
    }
}

impl Drop for DepthGuard {
    fn drop(&mut self) {
        LOAD_FILE_DEPTH.with(|d| d.set(0));
    }
}

/// `Loader.LoadFile(relPath)`: run `scripts/<relPath>` on the calling state. A `LoadFile` from
/// inside a `LoadFile` raises. After a run, `[LoadFile] <relPath> -> <result>` goes out on the
/// `Printf` channels.
pub fn load_file<L: Lua, H: LoaderHost>(l: &mut L, host: &H) -> Ret {
    let tag = l.arg_type(0);
    let Some(rel) = (tag == TSTRING).then(|| l.arg_string(0)).flatten() else {
        return Ret::Raise(format!(
            "bad argument #1 to 'Loader.LoadFile' (string expected, got {})",
            type_name(tag)
        ));
    };
    if let Err(e) = check_script_path(&rel) {
        return Ret::Raise(format!("Loader.LoadFile: {e}"));
    }
    let Some(guard) = DepthGuard::enter() else {
        return Ret::Raise(format!(
            "Loader.LoadFile: '{}' refused: a LoadFile is already running (nesting is not supported)",
            String::from_utf8_lossy(&rel)
        ));
    };
    let run = host.run_script(&rel);
    drop(guard);
    match run {
        Ok(ScriptRun::NotFound) => {
            l.push_bool(false);
            Ret::Values(1)
        }
        Ok(ScriptRun::Ran(result)) => {
            let mut line = b"[LoadFile] ".to_vec();
            line.extend_from_slice(&rel);
            line.extend_from_slice(b" -> ");
            line.extend_from_slice(&result);
            if let Err(e) = host.printf_line(&line) {
                return host_err("LoadFile", e);
            }
            l.push_bool(true);
            Ret::Values(1)
        }
        Err(e) => host_err("LoadFile", e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mercs2_formats::mod_runtime::lua::fake::{FakeLua, Val, s};
    use crate::store::{KEY_MAX, STR_MAX, Store};
    use std::cell::RefCell;

    #[derive(Default)]
    struct FakeHost {
        lines: RefCell<Vec<Vec<u8>>>,
        ws: RefCell<Vec<Vec<u8>>>,
        held: Vec<u8>,
        events: RefCell<Vec<u8>>,
        focused: bool,
        vars: RefCell<Option<Store>>,
        phase: Option<(i32, Vec<u8>)>,
        scripts: Vec<(Vec<u8>, Vec<u8>)>,
        ran: RefCell<Vec<Vec<u8>>>,
        nested: Cell<Option<Ret>>,
        log_broken: bool,
    }

    impl FakeHost {
        /// An in-memory store: its path is never written, because the fake never persists.
        fn with_store<T>(&self, f: impl FnOnce(&mut Store) -> Result<T, String>) -> Result<T, String> {
            let mut slot = self.vars.borrow_mut();
            if slot.is_none() {
                let path = std::env::temp_dir().join(format!("loader_ns_fake_{}_absent.ini", std::process::id()));
                *slot = Some(Store::load(path)?);
            }
            f(slot.as_mut().expect("store loaded above"))
        }
    }

    impl LoaderHost for FakeHost {
        fn printf_line(&self, line: &[u8]) -> Result<(), String> {
            if self.log_broken {
                return Err("lua_loader_printf.log is not open".to_string());
            }
            self.lines.borrow_mut().push(line.to_vec());
            Ok(())
        }
        fn ws_send_line(&self, line: &[u8]) -> Result<(), String> {
            self.ws.borrow_mut().push(line.to_vec());
            Ok(())
        }
        fn key_down(&self, vk: u8) -> bool {
            self.held.contains(&vk)
        }
        fn pop_key_events(&self) -> Vec<u8> {
            std::mem::take(&mut *self.events.borrow_mut())
        }
        fn clear_key_events(&self) {
            self.events.borrow_mut().clear();
        }
        fn is_game_focused(&self) -> bool {
            self.focused
        }
        fn save_var(&self, key: &[u8], value: Value) -> Result<(), String> {
            self.with_store(|st| st.set(key, value))
        }
        fn load_var(&self, key: &[u8]) -> Result<Option<Value>, String> {
            self.with_store(|st| Ok(st.get(key).cloned()))
        }
        fn load_phase(&self) -> Option<(i32, Vec<u8>)> {
            self.phase.clone()
        }
        fn run_script(&self, rel: &[u8]) -> Result<ScriptRun, String> {
            self.ran.borrow_mut().push(rel.to_vec());
            if rel == b"OnLoad/_lib/nest.lua" {
                let mut inner = FakeLua::with(vec![s("OnLoad/_lib/util.lua")]);
                self.nested.set(Some(load_file(&mut inner, self)));
            }
            match self.scripts.iter().find(|(name, _)| name == rel) {
                Some((_, result)) => Ok(ScriptRun::Ran(result.clone())),
                None if rel == b"broken.lua" => Err("broken.lua is 9000000 bytes, over the chunk cap".to_string()),
                None => Ok(ScriptRun::NotFound),
            }
        }
    }

    type Binding = fn(&mut FakeLua, &FakeHost) -> Ret;

    fn call(f: Binding, host: &FakeHost, args: Vec<Val>) -> (Ret, Vec<Val>) {
        let mut l = FakeLua::with(args);
        let r = f(&mut l, host);
        (r, l.pushed)
    }

    fn raise_msg(r: (Ret, Vec<Val>)) -> String {
        match r {
            (Ret::Raise(m), pushed) => {
                assert!(pushed.is_empty());
                m
            }
            other => panic!("expected a raise, got {other:?}"),
        }
    }

    #[test]
    fn function_list_is_the_registered_set() {
        assert_eq!(LOADER_FUNCTIONS.len(), 11);
    }

    #[test]
    fn printf_joins_string_args_with_tabs_and_skips_others() {
        let host = FakeHost::default();
        let r = call(
            printf,
            &host,
            vec![s("tag"), Val::Num(5.0), s("value"), Val::Nil, s("x")],
        );
        assert_eq!(r, (Ret::Values(0), vec![]));
        assert_eq!(host.lines.borrow().as_slice(), [b"tag\tvalue\tx".to_vec()]);
    }

    #[test]
    fn printf_without_string_args_writes_the_marker_line() {
        let host = FakeHost::default();
        assert_eq!(call(printf, &host, vec![Val::Num(1.0)]).0, Ret::Values(0));
        assert_eq!(host.lines.borrow()[0], b"[Loader.Printf: no string args (joined=0)]");
    }

    #[test]
    fn printf_caps_the_line_at_2015_bytes() {
        let host = FakeHost::default();
        let long = "a".repeat(5000);
        call(printf, &host, vec![s(&long), s("tail")]);
        assert_eq!(host.lines.borrow()[0].len(), PRINTF_JOIN_MAX - 1);
    }

    #[test]
    fn printf_raises_when_the_log_cannot_be_written() {
        let host = FakeHost {
            log_broken: true,
            ..FakeHost::default()
        };
        let m = raise_msg(call(printf, &host, vec![s("x")]));
        assert_eq!(m, "Loader.Printf: lua_loader_printf.log is not open");
    }

    #[test]
    fn join_matches_the_byte_budget() {
        let l = FakeLua::with(vec![s("abc"), s("def")]);
        assert_eq!(join_string_args(&l, 6), (b"abc\td".to_vec(), 2));
        assert_eq!(join_string_args(&l, 5), (b"abc".to_vec(), 1));
        assert_eq!(join_string_args(&l, 100), (b"abc\tdef".to_vec(), 2));
    }

    #[test]
    fn ws_send_broadcasts_the_joined_line_and_raises_without_strings() {
        let host = FakeHost::default();
        assert_eq!(call(ws_send, &host, vec![s("a"), s("b")]).0, Ret::Values(0));
        assert_eq!(host.ws.borrow().as_slice(), [b"a\tb".to_vec()]);
        assert!(host.lines.borrow().is_empty());
        assert_eq!(
            raise_msg(call(ws_send, &host, vec![Val::Num(1.0)])),
            "Loader.WsSend: no string arguments"
        );
    }

    #[test]
    fn keyboard_state_is_a_256_byte_plain_string() {
        let host = FakeHost {
            held: vec![0x10, 0xff, 0],
            ..FakeHost::default()
        };
        let (r, pushed) = call(get_keyboard_state, &host, vec![]);
        assert_eq!(r, Ret::Values(1));
        let Val::Str(state) = &pushed[0] else {
            panic!("{pushed:?}")
        };
        assert_eq!(state.len(), 256);
        for (vk, &b) in state.iter().enumerate() {
            let want = if [0x10, 0xff, 0].contains(&vk) { 0x80 } else { 0 };
            assert_eq!(b, want, "vk {vk}");
        }
    }

    #[test]
    fn is_key_down_returns_a_boolean() {
        let host = FakeHost {
            held: vec![0x10],
            ..FakeHost::default()
        };
        assert_eq!(
            call(is_key_down, &host, vec![Val::Num(16.0)]),
            (Ret::Values(1), vec![Val::Bool(true)])
        );
        assert_eq!(
            call(is_key_down, &host, vec![Val::Num(16.9)]),
            (Ret::Values(1), vec![Val::Bool(true)])
        );
        assert_eq!(
            call(is_key_down, &host, vec![Val::Num(17.0)]),
            (Ret::Values(1), vec![Val::Bool(false)])
        );
    }

    #[test]
    fn is_key_down_rejects_bad_keys() {
        let host = FakeHost::default();
        assert!(raise_msg(call(is_key_down, &host, vec![])).contains("number expected, got no value"));
        assert!(raise_msg(call(is_key_down, &host, vec![s("16")])).contains("got string"));
        assert!(raise_msg(call(is_key_down, &host, vec![Val::Num(256.0)])).contains("outside 0..=255"));
        assert!(raise_msg(call(is_key_down, &host, vec![Val::Num(-1.0)])).contains("outside 0..=255"));
        assert!(raise_msg(call(is_key_down, &host, vec![Val::Num(f64::NAN)])).contains("outside 0..=255"));
    }

    #[test]
    fn pop_key_events_returns_a_plain_string_and_empty_when_drained() {
        let host = FakeHost {
            events: RefCell::new(vec![0x41, 0x0d]),
            ..FakeHost::default()
        };
        assert_eq!(
            call(pop_key_events, &host, vec![]),
            (Ret::Values(1), vec![Val::Str(vec![0x41, 0x0d])])
        );
        assert_eq!(call(pop_key_events, &host, vec![]), (Ret::Values(1), vec![s("")]));
    }

    #[test]
    fn clear_key_events_empties_the_ring_and_returns_nothing() {
        let host = FakeHost {
            events: RefCell::new(vec![1, 2, 3]),
            ..FakeHost::default()
        };
        assert_eq!(call(clear_key_events, &host, vec![]), (Ret::Values(0), vec![]));
        assert_eq!(call(pop_key_events, &host, vec![]).1, vec![s("")]);
    }

    #[test]
    fn is_game_focused_returns_a_boolean() {
        let host = FakeHost {
            focused: true,
            ..FakeHost::default()
        };
        assert_eq!(
            call(is_game_focused, &host, vec![]),
            (Ret::Values(1), vec![Val::Bool(true)])
        );
        let host = FakeHost::default();
        assert_eq!(
            call(is_game_focused, &host, vec![]),
            (Ret::Values(1), vec![Val::Bool(false)])
        );
    }

    #[test]
    fn save_and_load_var_round_trip_each_type() {
        let host = FakeHost::default();
        for (key, value) in [
            ("n", Val::Num(18.5)),
            ("s", s("hardcore")),
            ("empty", s("")),
            ("t", Val::Bool(true)),
            ("f", Val::Bool(false)),
        ] {
            assert_eq!(
                call(save_var, &host, vec![s(key), value.clone()]),
                (Ret::Values(0), vec![])
            );
            assert_eq!(call(load_var, &host, vec![s(key)]), (Ret::Values(1), vec![value]));
        }
        assert_eq!(
            call(load_var, &host, vec![s("missing")]),
            (Ret::Values(1), vec![Val::Nil])
        );
        call(save_var, &host, vec![s("n"), s("now a string")]);
        assert_eq!(call(load_var, &host, vec![s("n")]).1, vec![s("now a string")]);
    }

    #[test]
    fn save_var_rejects_unsupported_values_and_keys() {
        let host = FakeHost::default();
        assert!(raise_msg(call(save_var, &host, vec![s("k"), Val::Table])).contains("got table"));
        assert!(raise_msg(call(save_var, &host, vec![s("k"), Val::Nil])).contains("got nil"));
        assert!(raise_msg(call(save_var, &host, vec![s("k")])).contains("got no value"));
        assert!(raise_msg(call(save_var, &host, vec![Val::Num(1.0), Val::Num(1.0)])).contains("string expected"));
        for bad in ["", "a=b", " lead", "trail\t", ";c", "#c", "new\nline"] {
            let m = raise_msg(call(save_var, &host, vec![s(bad), Val::Num(1.0)]));
            assert!(m.starts_with("Loader.SaveVar: "), "{bad:?}: {m}");
        }
        let long_key = "k".repeat(KEY_MAX + 1);
        assert!(raise_msg(call(save_var, &host, vec![s(&long_key), Val::Num(1.0)])).contains("128 bytes"));
        let long_val = "v".repeat(STR_MAX + 1);
        assert!(raise_msg(call(save_var, &host, vec![s("k"), s(&long_val)])).contains("2049 bytes"));
        assert!(raise_msg(call(load_var, &host, vec![])).contains("string expected, got no value"));
    }

    #[test]
    fn get_load_phase_returns_index_and_plain_string_name() {
        let host = FakeHost::default();
        assert_eq!(
            call(get_load_phase, &host, vec![]),
            (Ret::Values(2), vec![Val::Num(-1.0), s("none")])
        );
        let host = FakeHost {
            phase: Some((11, b"Player spawn".to_vec())),
            ..FakeHost::default()
        };
        assert_eq!(
            call(get_load_phase, &host, vec![]),
            (Ret::Values(2), vec![Val::Num(11.0), s("Player spawn")])
        );
    }

    #[test]
    fn load_file_runs_found_files_and_reports_the_result() {
        let host = FakeHost {
            scripts: vec![(b"OnLoad/_lib/util.lua".to_vec(), b"ok".to_vec())],
            ..FakeHost::default()
        };
        assert_eq!(
            call(load_file, &host, vec![s("OnLoad/_lib/util.lua")]),
            (Ret::Values(1), vec![Val::Bool(true)])
        );
        assert_eq!(
            host.lines.borrow().as_slice(),
            [b"[LoadFile] OnLoad/_lib/util.lua -> ok".to_vec()]
        );
        assert_eq!(
            call(load_file, &host, vec![s("nope.lua")]),
            (Ret::Values(1), vec![Val::Bool(false)])
        );
        assert_eq!(
            raise_msg(call(load_file, &host, vec![s("broken.lua")])),
            "Loader.LoadFile: broken.lua is 9000000 bytes, over the chunk cap"
        );
    }

    #[test]
    fn load_file_refuses_paths_outside_scripts() {
        let host = FakeHost::default();
        for bad in [
            "",
            "C:\\x.lua",
            "\\x.lua",
            "/x.lua",
            "../x.lua",
            "a/../../x.lua",
            "a..b.lua",
        ] {
            let m = raise_msg(call(load_file, &host, vec![s(bad)]));
            assert!(m.contains("refused"), "{bad:?}: {m}");
        }
        assert!(raise_msg(call(load_file, &host, vec![Val::Num(1.0)])).contains("string expected, got number"));
        assert!(host.ran.borrow().is_empty());
    }

    #[test]
    fn load_file_inside_load_file_raises_and_the_gate_reopens() {
        let host = FakeHost {
            scripts: vec![
                (b"OnLoad/_lib/nest.lua".to_vec(), b"done".to_vec()),
                (b"OnLoad/_lib/util.lua".to_vec(), b"ok".to_vec()),
            ],
            ..FakeHost::default()
        };
        assert_eq!(
            call(load_file, &host, vec![s("OnLoad/_lib/nest.lua")]).0,
            Ret::Values(1)
        );
        match host.nested.take() {
            Some(Ret::Raise(m)) => assert!(m.contains("already running"), "{m}"),
            other => panic!("{other:?}"),
        }
        assert_eq!(host.ran.borrow().len(), 1);
        assert_eq!(
            call(load_file, &host, vec![s("OnLoad/_lib/util.lua")]).0,
            Ret::Values(1)
        );
    }
}
