//! The Windows side: `DllMain`, start-up, the namespaces registered through m2-sdk.dll, and the game-thread
//! callbacks.
//!
//! Start-up runs on a thread started from `DllMain`, never under the loader lock:
//! 1. `lua_bridge.log` (m2-sdk.dll's per-module log) and the m2-sdk.dll version check;
//! 2. `lua_bridge.ini` (written with its defaults when missing), the loader directories,
//!    `lua_loader_printf.log` and `lua_loader_data.ini`;
//! 3. the REPL listener (and the WebSocket upgrade on the same port);
//! 4. the Lua host: `m2_lua_install`, the `Loader`, `Tcp` and `math` namespaces, the state and tick
//!    callbacks, and the log listener for load phases and the world-loaded milestone;
//! 5. the key-event sampler and the watchdog.
//!
//! Any failure in 1–4 is fatal: logged, shown in a message box, and the process exits; the game never runs
//! with a bridge that is half there.

use crate::config::{self, Config, EncodingMode};
use crate::encoding;
use crate::folders::{self, FOLDER_ON_BOOT, FOLDER_ON_LOAD, LoaderPaths, ProcessAnsi, Script, ScriptLoader, ScriptOutcome};
use crate::keys::{self, KeyEvents};
use crate::loader_ns::{self, LoaderHost, ScriptRun};
use mercs2_formats::mod_runtime::lua::game;
use crate::math_ns;
use crate::onkey::{self, OnKeyConfig, OnKeyEvent, OnKeyHooks, OnKeyRequest, OnKeyTable};
use crate::phase::PhaseTracker;
use crate::polyfill::POLYFILL;
use crate::pump::{Pump, TickHost};
use crate::queue::{LogFn, Normalize, Origin, Queue};
use crate::rearm::{Cycle, Flags};
use crate::repl;
use crate::store::{Store, Value};
use crate::tcp_ns;
use crate::watchdog::{self, Watchdog};
use crate::ws::WsHub;
use core::ffi::{c_char, c_int, c_void};
use mercs2_formats::mod_runtime::{self as rt, LuaReg, ffi};
use std::ffi::CString;
use std::fs::File;
use std::io::Write;
use std::os::windows::ffi::OsStringExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;
use windows_sys::Win32::Foundation::HMODULE;
use windows_sys::Win32::System::LibraryLoader::{DisableThreadLibraryCalls, GetModuleFileNameW};
use windows_sys::Win32::System::SystemInformation::GetTickCount;
use windows_sys::Win32::System::SystemServices::DLL_PROCESS_ATTACH;
use windows_sys::Win32::System::Threading::ExitProcess;
use windows_sys::Win32::UI::WindowsAndMessaging::{MB_ICONERROR, MB_OK, MessageBoxW};

/// The m2-sdk.dll release this ASI is built against (manifest.yaml requires `^0.4`).
const SDK_BUILT: &str = "0.4.0";
const TITLE: &str = "lua-bridge";
/// The longest result line a chunk produces.
const RESULT_CAP: usize = 16 * 1024;

static MODULE: AtomicUsize = AtomicUsize::new(0);
static BRIDGE: OnceLock<Bridge> = OnceLock::new();
static KEY_EVENTS: KeyEvents = KeyEvents::new();
static ONKEY: OnKeyTable = OnKeyTable::new();

struct Bridge {
    cfg: Config,
    flags: Flags,
    loader_paths: LoaderPaths,
    queue: Queue,
    ws: Option<WsHub>,
    store: Mutex<Store>,
    printf: Mutex<File>,
    phase: PhaseTracker,
    pump: Pump,
    cycle: Cycle,
    loader: ScriptLoader,
}

fn bridge() -> &'static Bridge {
    BRIDGE.get().expect("the game-thread callbacks are registered only after the bridge is built")
}

fn module() -> HMODULE {
    MODULE.load(Ordering::Acquire) as HMODULE
}

fn now() -> u32 {
    unsafe { GetTickCount() }
}

/// One line in `lua_bridge.log`.
fn log(line: &str) {
    let c = CString::new(line.replace('\0', " ")).expect("NULs replaced");
    unsafe { ffi::m2_logf_mod(module(), c"%s".as_ptr(), c.as_ptr()) };
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Log, show the reason, end the process.
fn fatal(msg: &str) -> ! {
    log(&format!("[!] lua_bridge: FATAL: {msg}"));
    let text = wide(&format!("{msg}\n\nThe game will now exit."));
    let title = wide(TITLE);
    unsafe {
        MessageBoxW(std::ptr::null_mut(), text.as_ptr(), title.as_ptr(), MB_OK | MB_ICONERROR);
        ExitProcess(1)
    }
}

fn module_file(m: HMODULE) -> Result<PathBuf, String> {
    let mut buf = vec![0u16; 1024];
    loop {
        let n = unsafe { GetModuleFileNameW(m, buf.as_mut_ptr(), buf.len() as u32) } as usize;
        if n == 0 {
            return Err(format!("GetModuleFileNameW failed: {}", std::io::Error::last_os_error()));
        }
        if n < buf.len() {
            return Ok(PathBuf::from(std::ffi::OsString::from_wide(&buf[..n])));
        }
        buf.resize(buf.len() * 2, 0);
    }
}

fn parent(p: &Path) -> Result<PathBuf, String> {
    p.parent().map(Path::to_path_buf).ok_or_else(|| format!("{} has no parent directory", p.display()))
}

/// The source-encoding step every chunk and script goes through, its log lines written as it runs.
fn normalize_bytes(mode: EncodingMode, what: &str, source: &str, bytes: &[u8]) -> Result<Vec<u8>, String> {
    match encoding::normalize(bytes, mode) {
        Ok(n) => {
            for line in n.log_lines(what, source) {
                log(&line);
            }
            Ok(n.bytes)
        }
        Err(e) => {
            log(&e.log_line(what, source));
            Err(e.to_string())
        }
    }
}

impl Bridge {
    /// One line on the `Loader.Printf` channels: `lua_loader_printf.log` and the WebSocket log feed.
    fn printf_line(&self, line: &[u8]) -> Result<(), String> {
        {
            let mut f = self.printf.lock().map_err(|_| "the printf log lock is poisoned".to_string())?;
            f.write_all(line)
                .and_then(|()| f.write_all(b"\r\n"))
                .map_err(|e| format!("writing lua_loader_printf.log: {e}"))?;
        }
        self.broadcast("log", line);
        Ok(())
    }

    fn broadcast(&self, kind: &str, line: &[u8]) {
        if let Some(ws) = &self.ws {
            ws.broadcast(kind, line);
        }
    }

    /// A loader line for both the module log and the feed; a feed failure is logged.
    fn feed(&self, line: &str) {
        if let Err(e) = self.printf_line(line.as_bytes()) {
            log(&format!("[!] lua_bridge: {e}"));
        }
    }

    /// Run a loader folder on `l`, logging and mirroring each script's outcome.
    fn run_folder(&self, l: *mut c_void, folder: &str) {
        let mode = self.cfg.loader_source_encoding;
        let mut prepare = || {
        polyfill(l);
        Ok(())
    };
        let mut normalize = |path: &Path, bytes: Vec<u8>| normalize_bytes(mode, folder, &path.display().to_string(), &bytes);
        let mut execute = |script: &Script, code: &[u8]| {
            log(&format!("[*] lua_bridge: Loading script ({folder}): {} ({} bytes)", script.rel_path, code.len()));
            let r = unsafe { rt::lua_exec(l, code, RESULT_CAP) };
            Ok(String::from_utf8_lossy(&r.text).into_owned())
        };
        let run = self.pump.timed(now(), || self.loader.run_folder(folder, &mut prepare, &mut normalize, &mut execute));
        let run = match run {
            Ok(run) => run,
            Err(e) => {
                log(&format!("[!] lua_bridge: scripts/{folder}/: {e}"));
                self.feed(&format!("[{folder}] {e}"));
                return;
            }
        };
        if run.reports.is_empty() {
            log(&format!("[*] lua_bridge: No scripts found in scripts/{folder}/"));
        }
        for r in run.reports {
            match r.outcome {
                ScriptOutcome::Disabled => {
                    log(&format!("[*] lua_bridge: {folder} script disabled by ini: {}", r.rel_path));
                }
                ScriptOutcome::Ran { result, .. } => {
                    log(&format!("[+] lua_bridge: Script result: {result}"));
                    self.feed(&folders::feed_line(folder, &r.rel_path, &result));
                }
                ScriptOutcome::InvalidOrder(e) | ScriptOutcome::LoadFailed(e) | ScriptOutcome::ExecuteFailed(e) => {
                    log(&format!("[!] lua_bridge: {folder} {}: {e}", r.rel_path));
                    self.feed(&folders::feed_line(folder, &r.rel_path, &e));
                }
            }
        }
    }

    fn register_onload(&self) {
        match self.loader.register_folder(FOLDER_ON_LOAD) {
            Ok(reg) if reg.scripts.is_empty() => {
                log(&format!("[*] lua_bridge: No scripts found in scripts/{FOLDER_ON_LOAD}/ (registration pass)"));
            }
            Ok(reg) => log(&format!(
                "[*] lua_bridge: Registered {} script(s) in scripts/{FOLDER_ON_LOAD}/ into lua_loader.ini — execution \
                 deferred",
                reg.scripts.len()
            )),
            Err(e) => log(&format!("[!] lua_bridge: registering scripts/{FOLDER_ON_LOAD}/: {e}")),
        }
    }

    /// Scan `scripts/OnKey/` into the binding table, and start the poller the first time.
    fn init_keys(&'static self) {
        match ONKEY.rescan(&self.loader_paths.scripts_root, &self.loader_paths.ini_path, &ProcessAnsi) {
            Ok(scan) => {
                log(&format!("[*] lua_bridge: OnKey: {} binding(s)", scan.bindings.len()));
                for (rel, key) in &scan.unrecognized {
                    log(&format!("[!] lua_bridge: OnKey {rel}: unrecognized key {key:?}"));
                }
                for s in &scan.shared {
                    log(&format!("[!] lua_bridge: OnKey key {} is bound by both {} and {}", s.key_name, s.first, s.second));
                }
                for (rel, e) in &scan.failed {
                    log(&format!("[!] lua_bridge: OnKey {rel}: {e}"));
                }
            }
            Err(e) => {
                log(&format!("[!] lua_bridge: scanning scripts/OnKey/: {e}"));
                return;
            }
        }
        let cfg = OnKeyConfig {
            cooldown_ms: self.cfg.loader_onkey_cooldown_ms,
            require_focus: self.cfg.loader_onkey_require_focus,
            exact_modifiers: self.cfg.loader_onkey_exact_modifiers,
        };
        let mode = self.cfg.loader_source_encoding;
        let hooks = OnKeyHooks {
            normalize: Box::new(move |path, bytes| normalize_bytes(mode, "OnKey", &path.display().to_string(), &bytes)),
            enqueue: Box::new(|req: OnKeyRequest| {
                self.queue.push(req.code, Origin::Loader(req.rel_path));
                Ok(())
            }),
            sink: Box::new(|ev: OnKeyEvent| onkey_event(self, ev)),
        };
        match ONKEY.start_polling(cfg, hooks) {
            Ok(true) => log("[*] lua_bridge: OnKey poller started"),
            Ok(false) => {}
            Err(e) => log(&format!("[!] lua_bridge: {e}")),
        }
    }
}

fn onkey_event(b: &Bridge, ev: OnKeyEvent) {
    let line = match ev {
        OnKeyEvent::Queued { key_name, rel_path, bytes } => {
            format!("[*] lua_bridge: OnKey {key_name}: queued {rel_path} ({bytes} bytes)")
        }
        OnKeyEvent::Throttled { key_name, rel_path, elapsed_ms, cooldown_ms, first } => {
            if !first {
                return;
            }
            format!("[*] lua_bridge: OnKey {key_name}: {rel_path} throttled ({elapsed_ms} ms of a {cooldown_ms} ms cooldown)")
        }
        OnKeyEvent::MissingFile { key_name, rel_path } => {
            format!("[!] lua_bridge: OnKey {key_name}: {rel_path} no longer exists")
        }
        OnKeyEvent::LoadFailed { key_name, rel_path, error } | OnKeyEvent::EnqueueFailed { key_name, rel_path, error } => {
            let line = format!("[!] lua_bridge: OnKey {key_name}: {rel_path}: {error}");
            b.feed(&onkey::result_feed_line(&rel_path, &error));
            line
        }
    };
    log(&line);
}

/// The `Loader` host for one call on the game thread.
struct GameLoader<'a> {
    b: &'a Bridge,
    l: *mut c_void,
}

impl LoaderHost for GameLoader<'_> {
    fn printf_line(&self, line: &[u8]) -> Result<(), String> {
        self.b.printf_line(line)
    }
    fn ws_send_line(&self, line: &[u8]) -> Result<(), String> {
        self.b.broadcast("ws", line);
        Ok(())
    }
    fn key_down(&self, vk: u8) -> bool {
        keys::win::key_down(vk)
    }
    fn pop_key_events(&self) -> Vec<u8> {
        KEY_EVENTS.pop_all()
    }
    fn clear_key_events(&self) {
        KEY_EVENTS.clear()
    }
    fn is_game_focused(&self) -> bool {
        keys::win::game_focused()
    }
    fn save_var(&self, key: &[u8], value: Value) -> Result<(), String> {
        let mut store = self.b.store.lock().map_err(|_| "the variable store lock is poisoned".to_string())?;
        store.set(key, value)?;
        store.persist()
    }
    fn delete_var(&self, key: &[u8]) -> Result<(), String> {
        let mut store = self.b.store.lock().map_err(|_| "the variable store lock is poisoned".to_string())?;
        if store.remove(key) {
            store.persist()?;
        }
        Ok(())
    }
    fn load_var(&self, key: &[u8]) -> Result<Option<Value>, String> {
        let store = self.b.store.lock().map_err(|_| "the variable store lock is poisoned".to_string())?;
        Ok(store.get(key).cloned())
    }
    fn load_phase(&self) -> Option<(i32, Vec<u8>)> {
        let (p, name) = self.b.phase.current();
        (p >= 0).then(|| (p, name.as_bytes().to_vec()))
    }
    fn run_script(&self, rel: &[u8]) -> Result<ScriptRun, String> {
        let rel = std::str::from_utf8(rel).map_err(|_| "the path is not UTF-8".to_string())?;
        let path = self.b.loader_paths.scripts_root.join(rel);
        let bytes = match std::fs::read(&path) {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(ScriptRun::NotFound),
            Err(e) => return Err(format!("reading {}: {e}", path.display())),
        };
        let code = normalize_bytes(self.b.cfg.loader_source_encoding, "LoadFile", &path.display().to_string(), &bytes)?;
        let r = unsafe { rt::lua_exec(self.l, &code, RESULT_CAP) };
        Ok(ScriptRun::Ran(r.text))
    }
}

/// The tick's executor on one state.
struct GameTick {
    l: *mut c_void,
}

impl TickHost for GameTick {
    fn now(&self) -> u32 {
        now()
    }
    fn usable(&self) -> bool {
        rt::lua_state_ok(self.l)
    }
    fn begin_batch(&mut self) {
        polyfill(self.l);
    }
    fn exec(&mut self, code: &[u8]) -> Vec<u8> {
        unsafe { rt::lua_exec(self.l, code, RESULT_CAP) }.text
    }
    fn run_onload(&mut self) {
        log("[*] lua_bridge: running OnLoad scripts");
        bridge().run_folder(self.l, FOLDER_ON_LOAD);
    }
    fn feed_loader(&self, rel_path: &str, result: &[u8]) {
        bridge().feed(&onkey::result_feed_line(rel_path, &String::from_utf8_lossy(result)));
    }
    fn log(&self, line: &str) {
        log(line);
    }
}

/// Fill in `math.pi`, `math.huge` and `assert` where `_G` lacks them. Run on each reported state and before
/// each batch and folder, since the game can rebuild `_G` after a state was captured.
fn polyfill(l: *mut c_void) {
    let r = unsafe { rt::lua_exec(l, POLYFILL.as_bytes(), RESULT_CAP) };
    if r.status != rt::EXEC_OK {
        log(&format!("[!] lua_bridge: the polyfill failed on L={l:p}: {}", String::from_utf8_lossy(&r.text)));
    }
}

unsafe extern "C" fn on_tick(l: *mut c_void, _ud: *mut c_void) {
    let b = bridge();
    b.pump.tick(&b.queue, now(), || GameTick { l });
}

unsafe extern "C" fn on_state(l: *mut c_void, is_new_cycle: c_int, _ud: *mut c_void) {
    let b = bridge();
    log(&format!("[+] lua_bridge: Lua VM captured: L={l:p}"));
    polyfill(l);
    let actions = b.cycle.on_state(is_new_cycle != 0, b.phase.milestone_seen(), b.flags);
    if actions.rearm {
        let (was, listening) = b.phase.rearm();
        b.pump.cancel_onload();
        log(&format!(
            "[*] lua_bridge: new load cycle detected (fresh Lua VM after a completed load) — re-arming loader (was at \
             phase {was})"
        ));
        if listening && let Err(e) = rt::loghook_set_active(on_log_line, std::ptr::null_mut(), true) {
            log(&format!("[!] lua_bridge: resuming the load-phase listener: {e}"));
        }
    }
    if actions.init_keys {
        b.init_keys();
    }
    if actions.run_onboot {
        b.run_folder(l, FOLDER_ON_BOOT);
    }
    if actions.register_onload {
        b.register_onload();
    }
}

unsafe extern "C" fn on_log_line(msg: *const c_char, _ud: *mut c_void) {
    let b = bridge();
    let line = unsafe { core::ffi::CStr::from_ptr(msg) }.to_bytes();
    let out = b.phase.on_line(line, now());
    if out.milestone && b.cfg.loader_enabled && b.cfg.loader_onload {
        log("[*] lua_bridge: OnLoad milestone reached (GlobalExit - Complete). Queuing OnLoad scripts.");
        b.pump.schedule_onload();
    }
    if let Some(msg) = out.gave_up {
        log(&format!("[!] lua_bridge: {msg}"));
    }
    if !out.keep_listening && let Err(e) = rt::loghook_set_active(on_log_line, std::ptr::null_mut(), false) {
        log(&format!("[!] lua_bridge: pausing the load-phase listener: {e}"));
    }
}

/// A `lua_CFunction` for a binding of the `Loader` namespace.
macro_rules! loader_fn {
    ($name:ident, $f:path) => {
        unsafe extern "C" fn $name(l: *mut c_void) -> c_int {
            let b = bridge();
            unsafe { game::call(l, |g| $f(g, &GameLoader { b, l })) }
        }
    };
}

/// A `lua_CFunction` for a binding that needs only the stack.
macro_rules! plain_fn {
    ($name:ident, $f:path) => {
        unsafe extern "C" fn $name(l: *mut c_void) -> c_int {
            unsafe { game::call(l, |g| $f(g)) }
        }
    };
}

loader_fn!(l_printf, loader_ns::printf);
loader_fn!(l_ws_send, loader_ns::ws_send);
loader_fn!(l_get_keyboard_state, loader_ns::get_keyboard_state);
loader_fn!(l_is_key_down, loader_ns::is_key_down);
loader_fn!(l_pop_key_events, loader_ns::pop_key_events);
loader_fn!(l_clear_key_events, loader_ns::clear_key_events);
loader_fn!(l_is_game_focused, loader_ns::is_game_focused);
loader_fn!(l_save_var, loader_ns::save_var);
loader_fn!(l_load_var, loader_ns::load_var);
loader_fn!(l_get_load_phase, loader_ns::get_load_phase);
loader_fn!(l_load_file, loader_ns::load_file);
plain_fn!(t_send, tcp_ns::send);
plain_fn!(m_sin, math_ns::sin);
plain_fn!(m_cos, math_ns::cos);
plain_fn!(m_tan, math_ns::tan);
plain_fn!(m_asin, math_ns::asin);
plain_fn!(m_acos, math_ns::acos);
plain_fn!(m_atan, math_ns::atan);
plain_fn!(m_atan2, math_ns::atan2);
plain_fn!(m_sinh, math_ns::sinh);
plain_fn!(m_cosh, math_ns::cosh);
plain_fn!(m_tanh, math_ns::tanh);
plain_fn!(m_sqrt, math_ns::sqrt);
plain_fn!(m_log, math_ns::log);
plain_fn!(m_log10, math_ns::log10);
plain_fn!(m_fmod, math_ns::fmod);
plain_fn!(m_ldexp, math_ns::ldexp);
plain_fn!(m_modf, math_ns::modf);
plain_fn!(m_frexp, math_ns::frexp);
plain_fn!(m_random, math_ns::random);
plain_fn!(m_randomseed, math_ns::randomseed);
plain_fn!(m_freedom_units, math_ns::freedom_units);

const fn reg(name: &'static core::ffi::CStr, f: rt::LuaCFunction) -> LuaReg {
    LuaReg { name: name.as_ptr(), func: Some(f) }
}
const END: LuaReg = LuaReg { name: core::ptr::null(), func: None };

static LOADER_REG: [LuaReg; 12] = [
    reg(c"Printf", l_printf),
    reg(c"WsSend", l_ws_send),
    reg(c"GetKeyboardState", l_get_keyboard_state),
    reg(c"IsKeyDown", l_is_key_down),
    reg(c"PopKeyEvents", l_pop_key_events),
    reg(c"ClearKeyEvents", l_clear_key_events),
    reg(c"IsGameFocused", l_is_game_focused),
    reg(c"SaveVar", l_save_var),
    reg(c"LoadVar", l_load_var),
    reg(c"GetLoadPhase", l_get_load_phase),
    reg(c"LoadFile", l_load_file),
    END,
];
static TCP_REG: [LuaReg; 2] = [reg(c"Send", t_send), END];
static MATH_REG: [LuaReg; 21] = [
    reg(c"sin", m_sin),
    reg(c"cos", m_cos),
    reg(c"tan", m_tan),
    reg(c"asin", m_asin),
    reg(c"acos", m_acos),
    reg(c"atan", m_atan),
    reg(c"atan2", m_atan2),
    reg(c"sinh", m_sinh),
    reg(c"cosh", m_cosh),
    reg(c"tanh", m_tanh),
    reg(c"sqrt", m_sqrt),
    reg(c"log", m_log),
    reg(c"log10", m_log10),
    reg(c"fmod", m_fmod),
    reg(c"ldexp", m_ldexp),
    reg(c"modf", m_modf),
    reg(c"frexp", m_frexp),
    reg(c"random", m_random),
    reg(c"randomseed", m_randomseed),
    reg(c"FreedomUnits", m_freedom_units),
    END,
];

/// Each registration table names exactly the functions its module documents, in order.
fn check_tables() -> Result<(), String> {
    let names = |t: &[LuaReg]| -> Vec<String> {
        t.iter()
            .take_while(|r| !r.name.is_null())
            .map(|r| unsafe { core::ffi::CStr::from_ptr(r.name) }.to_string_lossy().into_owned())
            .collect()
    };
    for (ns, table, want) in
        [("Loader", &LOADER_REG[..], loader_ns::LOADER_FUNCTIONS), ("math", &MATH_REG[..], math_ns::MATH_FUNCTIONS)]
    {
        if names(table) != want {
            return Err(format!("the {ns} registration table does not match its function list"));
        }
    }
    Ok(())
}

fn start() -> Result<(), String> {
    let m = module();
    unsafe { ffi::m2_log_init(m) };
    log("==========================================");
    log(&format!("[*] lua_bridge {} loading", env!("CARGO_PKG_VERSION")));
    log("==========================================");
    rt::compatible(rt::loaded_version(), SDK_BUILT)?;
    check_tables()?;

    let module_dir = parent(&module_file(m)?)?;
    let exe_dir = parent(&module_file(std::ptr::null_mut())?)?;
    let cfg = config::load(&module_dir.join("lua_bridge.ini"))?;
    let loader_paths =
        LoaderPaths { scripts_root: exe_dir.join("scripts"), ini_path: module_dir.join("lua_loader.ini") };
    let loader = ScriptLoader::new(loader_paths.clone(), Box::new(ProcessAnsi));
    if cfg.loader_enabled {
        loader.ensure_directories()?;
    }
    let printf_path = module_dir.join("lua_loader_printf.log");
    let printf = File::create(&printf_path).map_err(|e| format!("creating {}: {e}", printf_path.display()))?;
    log(&format!("[*] lua_bridge: Loader.Printf log opened at {}", printf_path.display()));
    let store = Store::load(module_dir.join("lua_loader_data.ini"))?;
    log(&format!("[*] lua_bridge: Loaded {} persisted variable(s) from lua_loader_data.ini", store.entries().len()));

    let log_fn: LogFn = Arc::new(log);
    let queue = Queue::new(log_fn);
    let mode = cfg.loader_source_encoding;
    let normalize: Normalize = Arc::new(move |what: &str, bytes: &[u8]| normalize_bytes(mode, what, "chunk", bytes));
    let ws = cfg.websocket_enabled.then(|| WsHub::new(queue.clone(), normalize.clone()));
    repl::start(cfg.host, cfg.port, queue.clone(), ws.clone(), normalize)
        .map_err(|e| format!("listening on {}:{}: {e}", cfg.host, cfg.port))?;

    let flags = Flags {
        loader_enabled: cfg.loader_enabled,
        rearm_on_menu: cfg.loader_rearm_on_menu,
        onboot: cfg.loader_onboot,
        onload: cfg.loader_onload,
    };
    let phase = PhaseTracker::new(cfg.loader_track_phase, cfg.loader_enabled && (cfg.loader_onload || cfg.loader_rearm_on_menu));
    let listening = phase.listening();
    let stuck_ms = cfg.watchdog_stuck_ms;
    let built = Bridge {
        cfg,
        flags,
        loader_paths,
        queue,
        ws,
        store: Mutex::new(store),
        printf: Mutex::new(printf),
        phase,
        pump: Pump::new(),
        cycle: Cycle::new(),
        loader,
    };
    if BRIDGE.set(built).is_err() {
        return Err("the bridge was started twice".into());
    }

    rt::lua_install()?;
    log("[*] lua_bridge: Lua host installed (m2-sdk.dll)");
    rt::lua_add_namespace(c"Loader", &LOADER_REG)?;
    rt::lua_add_namespace(c"Tcp", &TCP_REG)?;
    rt::lua_add_namespace(c"math", &MATH_REG)?;
    unsafe {
        rt::lua_on_state(on_state, std::ptr::null_mut())?;
        rt::lua_on_tick(on_tick, std::ptr::null_mut())?;
        if listening {
            rt::loghook_listen(on_log_line, std::ptr::null_mut())?;
        }
    }

    keys::win::spawn_key_event_sampler(&KEY_EVENTS)?;
    log(&format!("[*] lua_bridge: key-event sampler armed (~60 Hz, {}-slot ring)", keys::KEY_EVENT_RING_SLOTS));
    if stuck_ms > 0 {
        std::thread::Builder::new()
            .name("lua_bridge watchdog".into())
            .spawn(move || watch(stuck_ms))
            .map_err(|e| format!("starting the watchdog: {e}"))?;
        log(&format!("[*] lua_bridge: watchdog armed (stuck threshold {stuck_ms} ms)"));
    } else {
        log("[*] lua_bridge: watchdog disabled (watchdog_stuck_ms = 0)");
    }
    log("[*] lua_bridge: ready");
    Ok(())
}

fn watch(stuck_ms: u32) {
    let b = bridge();
    let mut w = Watchdog::default();
    loop {
        std::thread::sleep(Duration::from_millis(u64::from(watchdog::PERIOD_MS)));
        let snap = b.pump.snapshot(&b.queue, now());
        if let Some(stall) = w.check(&snap, stuck_ms) {
            for line in watchdog::report(&stall, &snap) {
                log(&line);
            }
        }
    }
}

#[unsafe(no_mangle)]
pub extern "system" fn DllMain(h: HMODULE, reason: u32, _reserved: *mut c_void) -> i32 {
    if reason == DLL_PROCESS_ATTACH {
        MODULE.store(h as usize, Ordering::Release);
        unsafe { DisableThreadLibraryCalls(h) };
        if let Err(e) = std::thread::Builder::new().name("lua_bridge start".into()).spawn(|| {
            if let Err(e) = start() {
                fatal(&e);
            }
        }) {
            fatal(&format!("could not start the start-up thread: {e}"));
        }
    }
    1
}
