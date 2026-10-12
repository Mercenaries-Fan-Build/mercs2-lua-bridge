//! OnKey hotkey scripts: scanning `scripts/OnKey/` into bindings, and the poller that queues a
//! bound script each time its key goes down.
//!
//! A script's binding comes from `lua_loader.ini` `[OnKey]`; when the ini has none, the
//! script's own `KEYVAL = "..."` declaration (or `unassigned`) is used and written back. The
//! poller re-reads the file on every press, so editing a bound script takes effect on the next
//! press. Every fire, throttle and failure is handed to a caller-supplied sink so the
//! integration can put it on the same log and live feed as OnBoot and OnLoad results.

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::folders::{self, AnsiCodec, FOLDER_ON_KEY, LoaderIni, ScanReport};
use crate::keys::{self, KeySpec, Mods, UNASSIGNED};

/// Poller period in milliseconds (about 30 Hz).
pub const POLL_PERIOD_MS: u64 = 33;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyScript {
    pub path: PathBuf,
    /// `\`-separated path under `scripts/OnKey/`; the script's key in the ini.
    pub rel_path: String,
    /// The binding text as configured.
    pub key_name: String,
    pub spec: KeySpec,
}

/// Two scripts bound to the same key and modifiers; both run on every press.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SharedBinding {
    pub key_name: String,
    pub first: String,
    pub second: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct KeyScan {
    /// Every registered script, in ASCII-case-insensitive path order, including ones whose key is
    /// unassigned or unrecognized (they never fire).
    pub bindings: Vec<KeyScript>,
    /// Scripts whose binding is `off`, `no` or `disabled`.
    pub disabled: Vec<String>,
    /// `(rel_path, key_name)` for bindings that name no known key and are not `unassigned`.
    pub unrecognized: Vec<(String, String)>,
    pub shared: Vec<SharedBinding>,
    /// `(rel_path, key_name)` defaults written to the ini by this scan.
    pub defaults_written: Vec<(String, String)>,
    /// `(rel_path, error)` for scripts whose default could not be read; not registered.
    pub failed: Vec<(String, String)>,
    pub scan: ScanReport,
    pub ini_created: bool,
}

/// Scan `scripts_root/OnKey/` and resolve each script's binding, writing missing defaults to
/// the ini at `ini_path`.
pub fn scan_key_scripts(scripts_root: &Path, ini_path: &Path, codec: &dyn AnsiCodec) -> Result<KeyScan, String> {
    let ini_created = folders::ensure_loader_ini(ini_path)?;
    let discovery = folders::discover(scripts_root, FOLDER_ON_KEY)?;
    let mut out = KeyScan { scan: discovery.report, ini_created, ..KeyScan::default() };
    if discovery.files.is_empty() {
        return Ok(out);
    }
    let mut ini = LoaderIni::load(ini_path, codec)?;
    for f in discovery.files {
        let configured = ini.get(FOLDER_ON_KEY, &f.rel_path).unwrap_or("").to_string();
        let key_name = if configured.is_empty() {
            let default = std::fs::read(&f.path)
                .map_err(|e| format!("cannot read {}: {e}", f.path.display()))
                .and_then(|src| keys::extract_default_key(&src));
            match default {
                Ok(k) => {
                    ini.set(FOLDER_ON_KEY, &f.rel_path, &k);
                    out.defaults_written.push((f.rel_path.clone(), k.clone()));
                    k
                }
                Err(e) => {
                    out.failed.push((f.rel_path, e));
                    continue;
                }
            }
        } else {
            configured
        };
        if folders::is_disable_value(&key_name) {
            out.disabled.push(f.rel_path);
            continue;
        }
        let spec = keys::resolve_key_spec(&key_name);
        if spec.vk.is_none() && !key_name.eq_ignore_ascii_case(UNASSIGNED) {
            out.unrecognized.push((f.rel_path.clone(), key_name.clone()));
        }
        out.bindings.push(KeyScript { path: f.path, rel_path: f.rel_path, key_name, spec });
    }
    if ini.is_dirty() {
        ini.save(ini_path, codec)?;
    }
    for (i, a) in out.bindings.iter().enumerate() {
        for b in &out.bindings[i + 1..] {
            if a.spec.vk.is_some() && a.spec == b.spec {
                out.shared.push(SharedBinding {
                    key_name: a.key_name.clone(),
                    first: a.rel_path.clone(),
                    second: b.rel_path.clone(),
                });
            }
        }
    }
    Ok(out)
}

/// Whether the held modifiers satisfy a binding.
///
/// With `exact`, the held set must equal the required set, so `F5` and `ctrl+F5` can be bound to
/// different scripts; otherwise the held set need only include it. When the bound key is itself
/// a modifier (`shift`, `lctrl`, `ctrl+alt`, ...), holding that key necessarily sets its own
/// modifier, so that modifier is left out of the comparison on both sides.
pub fn modifiers_match(held: Mods, spec: &KeySpec, exact: bool) -> bool {
    let own = spec.vk.map_or(Mods::NONE, Mods::of_key);
    let held = held.without(own);
    let required = spec.mods.without(own);
    if exact { held == required } else { held.contains(required) }
}

/// Live key state the poller reads.
pub trait KeyInput {
    /// Whether the game window has focus.
    fn focused(&self) -> bool;
    fn is_down(&self, vk: u8) -> bool;
}

/// The modifiers currently held, either side.
pub fn held_modifiers(input: &dyn KeyInput) -> Mods {
    let mut held = Mods::NONE;
    if input.is_down(keys::vk::CONTROL) {
        held = held.union(Mods::CTRL);
    }
    if input.is_down(keys::vk::SHIFT) {
        held = held.union(Mods::SHIFT);
    }
    if input.is_down(keys::vk::MENU) {
        held = held.union(Mods::ALT);
    }
    held
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OnKeyConfig {
    /// A script pressed again within this many ms of its last fire is throttled; 0 disables.
    pub cooldown_ms: u32,
    /// Ignore keys while the game is not the foreground window.
    pub require_focus: bool,
    /// See [`modifiers_match`].
    pub exact_modifiers: bool,
}

/// A script to run, handed to the enqueue hook.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OnKeyRequest {
    pub key_name: String,
    pub rel_path: String,
    pub code: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OnKeyEvent {
    Queued { key_name: String, rel_path: String, bytes: usize },
    /// `first` is true for the first throttle of this script since its binding was installed.
    Throttled { key_name: String, rel_path: String, elapsed_ms: u32, cooldown_ms: u32, first: bool },
    MissingFile { key_name: String, rel_path: String },
    LoadFailed { key_name: String, rel_path: String, error: String },
    EnqueueFailed { key_name: String, rel_path: String, error: String },
}

/// The line mirrored to the live log feed when a queued OnKey script's result comes back.
pub fn result_feed_line(rel_path: &str, result: &str) -> String {
    folders::feed_line(FOLDER_ON_KEY, rel_path, result)
}

/// [`folders::Normalize`] that can move to the polling thread.
pub type SendNormalize = dyn FnMut(&Path, Vec<u8>) -> Result<Vec<u8>, String> + Send;

/// What the poller calls out to.
pub struct OnKeyHooks {
    /// Encoding normalisation of a script's bytes (see [`folders::load_script_bytes`]).
    pub normalize: Box<SendNormalize>,
    /// Queue a script for the game's Lua executor.
    pub enqueue: Box<dyn FnMut(OnKeyRequest) -> Result<(), String> + Send>,
    /// Receives every poller event.
    pub sink: Box<dyn FnMut(OnKeyEvent) + Send>,
}

#[derive(Debug)]
struct Entry {
    script: KeyScript,
    was_down: bool,
    last_fired_ms: Option<u32>,
    throttle_reported: bool,
}

/// The installed bindings and their edge/cooldown state.
#[derive(Debug, Default)]
pub struct OnKeyTable {
    entries: Mutex<Vec<Entry>>,
    poller_started: AtomicBool,
}

impl OnKeyTable {
    pub const fn new() -> Self {
        OnKeyTable { entries: Mutex::new(Vec::new()), poller_started: AtomicBool::new(false) }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Vec<Entry>> {
        self.entries.lock().expect("OnKey table mutex poisoned: a thread panicked while holding it")
    }

    /// Replace every binding; edge, cooldown and throttle state start fresh.
    pub fn install(&self, bindings: Vec<KeyScript>) {
        let fresh = bindings
            .into_iter()
            .map(|script| Entry { script, was_down: false, last_fired_ms: None, throttle_reported: false })
            .collect();
        *self.lock() = fresh;
    }

    /// Scan and install. Safe to call any number of times (each load cycle re-arms with it); it
    /// depends on nothing but the OnKey folder and the ini.
    pub fn rescan(&self, scripts_root: &Path, ini_path: &Path, codec: &dyn AnsiCodec) -> Result<KeyScan, String> {
        let scan = scan_key_scripts(scripts_root, ini_path, codec)?;
        self.install(scan.bindings.clone());
        Ok(scan)
    }

    /// Whether the polling thread has been started.
    pub fn poller_started(&self) -> bool {
        self.poller_started.load(Ordering::Acquire)
    }

    pub fn binding_count(&self) -> usize {
        self.lock().len()
    }

    /// One poll: queue each script whose key went down with matching modifiers.
    ///
    /// Without focus (when required) every key is treated as released, so refocusing with a key
    /// held does not fire. A key down with the wrong modifiers is also treated as released, so
    /// fixing the modifiers and pressing again fires. `now_ms` is a wrapping millisecond clock.
    pub fn poll(&self, cfg: &OnKeyConfig, input: &dyn KeyInput, now_ms: u32, hooks: &mut OnKeyHooks) {
        let mut entries = self.lock();
        if cfg.require_focus && !input.focused() {
            for e in entries.iter_mut() {
                e.was_down = false;
            }
            return;
        }
        for e in entries.iter_mut() {
            let Some(vk) = e.script.spec.vk else { continue };
            let down = input.is_down(vk);
            if !down {
                e.was_down = false;
                continue;
            }
            if !modifiers_match(held_modifiers(input), &e.script.spec, cfg.exact_modifiers) {
                e.was_down = false;
                continue;
            }
            if e.was_down {
                continue;
            }
            e.was_down = true;
            fire(e, cfg, now_ms, hooks);
        }
    }
}

fn fire(e: &mut Entry, cfg: &OnKeyConfig, now_ms: u32, hooks: &mut OnKeyHooks) {
    let key_name = || e.script.key_name.clone();
    let rel_path = || e.script.rel_path.clone();
    if cfg.cooldown_ms > 0
        && let Some(last) = e.last_fired_ms
    {
        let elapsed_ms = now_ms.wrapping_sub(last);
        if elapsed_ms < cfg.cooldown_ms {
            let first = !e.throttle_reported;
            e.throttle_reported = true;
            (hooks.sink)(OnKeyEvent::Throttled {
                key_name: key_name(),
                rel_path: rel_path(),
                elapsed_ms,
                cooldown_ms: cfg.cooldown_ms,
                first,
            });
            return;
        }
    }
    if !std::fs::metadata(&e.script.path).is_ok_and(|m| !m.is_dir()) {
        (hooks.sink)(OnKeyEvent::MissingFile { key_name: key_name(), rel_path: rel_path() });
        return;
    }
    e.last_fired_ms = Some(now_ms);
    let event = match folders::load_script_bytes(&e.script.path, &mut *hooks.normalize) {
        Err(error) => OnKeyEvent::LoadFailed { key_name: key_name(), rel_path: rel_path(), error },
        Ok(code) => {
            let bytes = code.len();
            match (hooks.enqueue)(OnKeyRequest { key_name: key_name(), rel_path: rel_path(), code }) {
                Ok(()) => OnKeyEvent::Queued { key_name: key_name(), rel_path: rel_path(), bytes },
                Err(error) => OnKeyEvent::EnqueueFailed { key_name: key_name(), rel_path: rel_path(), error },
            }
        }
    };
    (hooks.sink)(event);
}

#[cfg(windows)]
mod win {
    use super::*;
    use std::time::{Duration, Instant};

    struct Win32Keys;

    impl KeyInput for Win32Keys {
        fn focused(&self) -> bool {
            keys::win::game_focused()
        }
        fn is_down(&self, vk: u8) -> bool {
            keys::win::key_down(vk)
        }
    }

    impl OnKeyTable {
        /// Start the polling thread once for the life of the process. Returns `Ok(false)` when it
        /// is already running (later rescans only swap the table it reads).
        pub fn start_polling(&'static self, cfg: OnKeyConfig, mut hooks: OnKeyHooks) -> Result<bool, String> {
            if self.poller_started.swap(true, Ordering::AcqRel) {
                return Ok(false);
            }
            std::thread::Builder::new()
                .name("lua-bridge OnKey poller".into())
                .spawn(move || {
                    let start = Instant::now();
                    loop {
                        // A wrapping millisecond clock; cooldown arithmetic uses wrapping_sub.
                        let now_ms = start.elapsed().as_millis() as u32;
                        self.poll(&cfg, &Win32Keys, now_ms, &mut hooks);
                        std::thread::sleep(Duration::from_millis(POLL_PERIOD_MS));
                    }
                })
                .map_err(|e| {
                    self.poller_started.store(false, Ordering::Release);
                    format!("failed to spawn the OnKey polling thread: {e}")
                })?;
            Ok(true)
        }
    }
}


#[cfg(test)]
mod tests {
    use super::*;
    use crate::folders::Cp1252;
    use crate::folders::testdir::TestDir;
    use crate::keys::vk;
    use std::cell::RefCell;
    use std::collections::HashSet;
    use std::sync::Arc;

    struct FakeKeys {
        focused: bool,
        down: RefCell<HashSet<u8>>,
    }

    impl FakeKeys {
        fn new() -> Self {
            FakeKeys { focused: true, down: RefCell::new(HashSet::new()) }
        }
        fn set(&self, keys: &[u8]) {
            *self.down.borrow_mut() = keys.iter().copied().collect();
        }
    }

    impl KeyInput for FakeKeys {
        fn focused(&self) -> bool {
            self.focused
        }
        fn is_down(&self, vk: u8) -> bool {
            self.down.borrow().contains(&vk)
        }
    }

    type Log<T> = Arc<Mutex<Vec<T>>>;

    fn hooks() -> (OnKeyHooks, Log<OnKeyRequest>, Log<OnKeyEvent>) {
        let queued: Log<OnKeyRequest> = Arc::default();
        let events: Log<OnKeyEvent> = Arc::default();
        let (q, ev) = (queued.clone(), events.clone());
        let h = OnKeyHooks {
            normalize: Box::new(|_, b| Ok(b)),
            enqueue: Box::new(move |r| {
                q.lock().unwrap().push(r);
                Ok(())
            }),
            sink: Box::new(move |e| ev.lock().unwrap().push(e)),
        };
        (h, queued, events)
    }

    const CFG: OnKeyConfig = OnKeyConfig { cooldown_ms: 250, require_focus: true, exact_modifiers: true };

    fn table_with(t: &TestDir, bindings: &[(&str, &str)]) -> OnKeyTable {
        let table = OnKeyTable::new();
        let scripts = bindings
            .iter()
            .map(|(rel, key)| KeyScript {
                path: t.file(&format!("OnKey/{rel}"), "print(1)"),
                rel_path: rel.to_string(),
                key_name: key.to_string(),
                spec: keys::resolve_key_spec(key),
            })
            .collect();
        table.install(scripts);
        table
    }

    #[test]
    fn exact_and_loose_modifier_matching() {
        let f5 = keys::resolve_key_spec("F5");
        let ctrl_f5 = keys::resolve_key_spec("ctrl+F5");
        assert!(modifiers_match(Mods::NONE, &f5, true));
        assert!(!modifiers_match(Mods::CTRL, &f5, true));
        assert!(modifiers_match(Mods::CTRL, &f5, false));
        assert!(modifiers_match(Mods::CTRL, &ctrl_f5, true));
        assert!(!modifiers_match(Mods::CTRL.union(Mods::ALT), &ctrl_f5, true));
        assert!(modifiers_match(Mods::CTRL.union(Mods::ALT), &ctrl_f5, false));
        assert!(!modifiers_match(Mods::NONE, &ctrl_f5, false));
    }

    #[test]
    fn bare_modifier_bindings_match_while_held() {
        for (name, held) in [
            ("shift", Mods::SHIFT),
            ("ctrl", Mods::CTRL),
            ("alt", Mods::ALT),
            ("lshift", Mods::SHIFT),
            ("rctrl", Mods::CTRL),
        ] {
            let spec = keys::resolve_key_spec(name);
            assert!(modifiers_match(held, &spec, true), "{name}");
            let extra = if held == Mods::ALT { Mods::CTRL } else { Mods::ALT };
            assert!(!modifiers_match(held.union(extra), &spec, true), "{name} with another modifier held");
            assert!(modifiers_match(held.union(extra), &spec, false), "{name} loose");
        }
        let ctrl_shift = keys::resolve_key_spec("ctrl+shift");
        assert!(modifiers_match(Mods::CTRL.union(Mods::SHIFT), &ctrl_shift, true));
        assert!(!modifiers_match(Mods::SHIFT, &ctrl_shift, true));
    }

    #[test]
    fn bare_shift_binding_fires_through_the_poller() {
        let t = TestDir::new("bareshift");
        let table = table_with(&t, &[("s.lua", "shift")]);
        let (mut h, queued, _) = hooks();
        let keys = FakeKeys::new();
        keys.set(&[vk::SHIFT, vk::LSHIFT]);
        table.poll(&CFG, &keys, 1000, &mut h);
        assert_eq!(queued.lock().unwrap().len(), 1);
    }

    #[test]
    fn fires_once_per_press_and_respects_exact_modifiers() {
        let t = TestDir::new("edge");
        let table = table_with(&t, &[("a.lua", "F5"), ("b.lua", "ctrl+F5")]);
        let (mut h, queued, _) = hooks();
        let keys = FakeKeys::new();
        keys.set(&[vk::F1 + 4]);
        table.poll(&CFG, &keys, 1000, &mut h);
        table.poll(&CFG, &keys, 1033, &mut h);
        keys.set(&[]);
        table.poll(&CFG, &keys, 2000, &mut h);
        keys.set(&[vk::F1 + 4, vk::CONTROL]);
        table.poll(&CFG, &keys, 3000, &mut h);
        let q = queued.lock().unwrap();
        let names: Vec<_> = q.iter().map(|r| r.rel_path.as_str()).collect();
        assert_eq!(names, vec!["a.lua", "b.lua"]);
        assert_eq!(q[0].code, b"print(1)");
        assert_eq!(q[0].key_name, "F5");
    }

    #[test]
    fn wrong_modifiers_then_right_modifiers_refires() {
        let t = TestDir::new("rearm");
        let table = table_with(&t, &[("a.lua", "F5")]);
        let (mut h, queued, _) = hooks();
        let keys = FakeKeys::new();
        keys.set(&[vk::F1 + 4, vk::SHIFT]);
        table.poll(&CFG, &keys, 1000, &mut h);
        assert!(queued.lock().unwrap().is_empty());
        keys.set(&[vk::F1 + 4]);
        table.poll(&CFG, &keys, 1033, &mut h);
        assert_eq!(queued.lock().unwrap().len(), 1);
    }

    #[test]
    fn unfocused_clears_edges_and_does_not_fire() {
        let t = TestDir::new("focus");
        let table = table_with(&t, &[("a.lua", "insert")]);
        let (mut h, queued, _) = hooks();
        let mut keys = FakeKeys::new();
        keys.set(&[vk::INSERT]);
        table.poll(&CFG, &keys, 1000, &mut h);
        keys.focused = false;
        table.poll(&CFG, &keys, 2000, &mut h);
        keys.focused = true;
        table.poll(&CFG, &keys, 3000, &mut h);
        assert_eq!(queued.lock().unwrap().len(), 2, "refocus with key held is a fresh edge after the clear");
        let loose = OnKeyConfig { require_focus: false, ..CFG };
        keys.focused = false;
        keys.set(&[]);
        table.poll(&loose, &keys, 4000, &mut h);
        keys.set(&[vk::INSERT]);
        table.poll(&loose, &keys, 5000, &mut h);
        assert_eq!(queued.lock().unwrap().len(), 3, "focus not required");
    }

    #[test]
    fn cooldown_throttles_and_reports_first_only_flag() {
        let t = TestDir::new("cool");
        let table = table_with(&t, &[("a.lua", "k")]);
        let (mut h, queued, events) = hooks();
        let keys = FakeKeys::new();
        for now in [u32::MAX - 50, 100, 200, 400] {
            keys.set(&[0x4B]);
            table.poll(&CFG, &keys, now, &mut h);
            keys.set(&[]);
            table.poll(&CFG, &keys, now, &mut h);
        }
        assert_eq!(queued.lock().unwrap().len(), 2);
        let throttles: Vec<_> = events
            .lock()
            .unwrap()
            .iter()
            .filter_map(|e| match e {
                OnKeyEvent::Throttled { elapsed_ms, first, .. } => Some((*elapsed_ms, *first)),
                _ => None,
            })
            .collect();
        assert_eq!(throttles, vec![(151, true), (200, false)]);
        let zero = OnKeyConfig { cooldown_ms: 0, ..CFG };
        keys.set(&[0x4B]);
        table.poll(&zero, &keys, 401, &mut h);
        assert_eq!(queued.lock().unwrap().len(), 3);
    }

    #[test]
    fn missing_file_reported_and_not_queued() {
        let t = TestDir::new("gone");
        let table = table_with(&t, &[("a.lua", "k")]);
        std::fs::remove_file(t.path().join("OnKey/a.lua")).unwrap();
        let (mut h, queued, events) = hooks();
        let keys = FakeKeys::new();
        keys.set(&[0x4B]);
        table.poll(&CFG, &keys, 1000, &mut h);
        assert!(queued.lock().unwrap().is_empty());
        assert_eq!(
            events.lock().unwrap()[0],
            OnKeyEvent::MissingFile { key_name: "k".into(), rel_path: "a.lua".into() }
        );
    }

    #[test]
    fn load_and_enqueue_failures_reach_the_sink() {
        let t = TestDir::new("fail");
        let table = table_with(&t, &[("a.lua", "k")]);
        let (mut h, _, events) = hooks();
        h.normalize = Box::new(|_, _| Err("UTF-16".into()));
        let keys = FakeKeys::new();
        keys.set(&[0x4B]);
        table.poll(&CFG, &keys, 1000, &mut h);
        keys.set(&[]);
        table.poll(&CFG, &keys, 1000, &mut h);
        h.normalize = Box::new(|_, b| Ok(b));
        h.enqueue = Box::new(|_| Err("queue closed".into()));
        keys.set(&[0x4B]);
        table.poll(&CFG, &keys, 2000, &mut h);
        let ev = events.lock().unwrap();
        assert!(matches!(&ev[0], OnKeyEvent::LoadFailed { error, .. } if error == "UTF-16"));
        assert!(matches!(&ev[1], OnKeyEvent::EnqueueFailed { error, .. } if error == "queue closed"));
    }

    #[test]
    fn queued_event_reaches_the_sink() {
        let t = TestDir::new("sink");
        let table = table_with(&t, &[("a.lua", "k")]);
        let (mut h, _, events) = hooks();
        let keys = FakeKeys::new();
        keys.set(&[0x4B]);
        table.poll(&CFG, &keys, 1000, &mut h);
        assert_eq!(
            events.lock().unwrap()[0],
            OnKeyEvent::Queued { key_name: "k".into(), rel_path: "a.lua".into(), bytes: 8 }
        );
        assert_eq!(result_feed_line("a.lua", "nil"), "[OnKey] a.lua -> nil");
    }

    #[test]
    fn unassigned_bindings_never_fire() {
        let t = TestDir::new("unassigned");
        let table = table_with(&t, &[("a.lua", "unassigned"), ("b.lua", "pgup")]);
        let (mut h, queued, _) = hooks();
        let keys = FakeKeys::new();
        keys.set(&(0..=255).collect::<Vec<u8>>());
        table.poll(&CFG, &keys, 1000, &mut h);
        assert!(queued.lock().unwrap().is_empty());
    }

    #[test]
    fn scan_extracts_defaults_writes_back_and_reports() {
        let t = TestDir::new("scan");
        let root = t.path().join("scripts");
        let ini = t.path().join("lua_loader.ini");
        t.file("scripts/OnKey/a.lua", "-- KEYVAL = \"F5\"\n");
        t.file("scripts/OnKey/b.lua", "print(1)\n");
        t.file("scripts/OnKey/c.lua", "-- KEYVAL = \"F5\"\n");
        t.file("scripts/OnKey/d.lua", "-- KEYVAL = \"F6\"\n");
        t.file("scripts/OnKey/e.lua", "-- KEYVAL = \"F7\"\n");
        t.file("scripts/OnKey/sub/f.lua", "");
        std::fs::write(&ini, "[OnKey]\nd.lua=off\ne.lua=pgup\nsub\\f.lua=ctrl+F8\n").unwrap();
        let scan = scan_key_scripts(&root, &ini, &Cp1252).unwrap();
        let got: Vec<_> = scan.bindings.iter().map(|b| (b.rel_path.as_str(), b.key_name.as_str())).collect();
        assert_eq!(
            got,
            vec![("a.lua", "F5"), ("b.lua", "unassigned"), ("c.lua", "F5"), ("e.lua", "pgup"), ("sub\\f.lua", "ctrl+F8")]
        );
        assert_eq!(scan.disabled, vec!["d.lua".to_string()]);
        assert_eq!(scan.unrecognized, vec![("e.lua".into(), "pgup".into())]);
        assert_eq!(
            scan.shared,
            vec![SharedBinding { key_name: "F5".into(), first: "a.lua".into(), second: "c.lua".into() }]
        );
        assert_eq!(
            scan.defaults_written,
            vec![("a.lua".into(), "F5".into()), ("b.lua".into(), "unassigned".into()), ("c.lua".into(), "F5".into())]
        );
        let text = std::fs::read_to_string(&ini).unwrap();
        assert_eq!(text, "[OnKey]\nd.lua=off\ne.lua=pgup\nsub\\f.lua=ctrl+F8\na.lua=F5\nb.lua=unassigned\nc.lua=F5\n");
        assert_eq!(scan.bindings[4].spec, KeySpec { vk: Some(vk::F1 + 7), mods: Mods::CTRL });
    }

    #[test]
    fn lenient_key_names_are_reported_unrecognized() {
        let t = TestDir::new("strict");
        let root = t.path().join("scripts");
        let ini = t.path().join("lua_loader.ini");
        for (i, name) in ["F5x", "F0", "F25", "5a", "F05"].iter().enumerate() {
            t.file(&format!("scripts/OnKey/s{i}.lua"), &format!("-- KEYVAL = \"{name}\"\n"));
        }
        t.file("scripts/OnKey/ok.lua", "-- KEYVAL = \"F5\"\n");
        let scan = scan_key_scripts(&root, &ini, &Cp1252).unwrap();
        let names: Vec<_> = scan.unrecognized.iter().map(|(_, k)| k.as_str()).collect();
        assert_eq!(names, vec!["F5x", "F0", "F25", "5a", "F05"]);
        assert_eq!(scan.bindings.len(), 6);
        assert_eq!(scan.bindings[0].spec.vk, Some(vk::F1 + 4));
    }

    #[test]
    fn scan_of_empty_folder_creates_header_only() {
        let t = TestDir::new("scanempty");
        let root = t.path().join("scripts");
        crate::folders::ensure_loader_directories(&root).unwrap();
        let ini = t.path().join("lua_loader.ini");
        let scan = scan_key_scripts(&root, &ini, &Cp1252).unwrap();
        assert!(scan.ini_created);
        assert!(scan.bindings.is_empty());
        assert_eq!(std::fs::read(&ini).unwrap(), crate::folders::LOADER_INI_HEADER);
    }

    #[test]
    fn rescan_picks_up_new_scripts_with_no_other_precondition() {
        let t = TestDir::new("rescan");
        let root = t.path().join("scripts");
        let ini = t.path().join("lua_loader.ini");
        crate::folders::ensure_loader_directories(&root).unwrap();
        let table = OnKeyTable::new();
        t.file("scripts/OnKey/a.lua", "-- KEYVAL = \"F1\"\n");
        assert_eq!(table.rescan(&root, &ini, &Cp1252).unwrap().bindings.len(), 1);
        t.file("scripts/OnKey/b.lua", "-- KEYVAL = \"F2\"\n");
        assert_eq!(table.rescan(&root, &ini, &Cp1252).unwrap().bindings.len(), 2);
        assert_eq!(table.binding_count(), 2);
        assert!(!table.poller_started());
    }

    #[test]
    fn install_resets_edge_and_cooldown_state() {
        let t = TestDir::new("reset");
        let table = table_with(&t, &[("a.lua", "k")]);
        let (mut h, queued, _) = hooks();
        let keys = FakeKeys::new();
        keys.set(&[0x4B]);
        table.poll(&CFG, &keys, 1000, &mut h);
        let path = t.path().join("OnKey/a.lua");
        table.install(vec![KeyScript {
            path,
            rel_path: "a.lua".into(),
            key_name: "k".into(),
            spec: keys::resolve_key_spec("k"),
        }]);
        table.poll(&CFG, &keys, 1010, &mut h);
        assert_eq!(queued.lock().unwrap().len(), 2);
    }
}
