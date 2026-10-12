//! Script discovery under `scripts/<folder>/`, the `lua_loader.ini` that orders and disables
//! scripts, and running a folder's scripts through a caller-supplied executor.
//!
//! Script keys in the ini are paths relative to the folder with `\` separators
//! (`sub\name.lua`), whatever host the scan runs on, so one ini serves every build.

use std::cmp::Ordering;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};

pub const FOLDER_ON_BOOT: &str = "OnBoot";
pub const FOLDER_ON_LOAD: &str = "OnLoad";
pub const FOLDER_ON_KEY: &str = "OnKey";

/// Most `.lua` files one scan keeps; further files are counted in [`ScanReport::dropped`].
pub const MAX_SCRIPTS: usize = 128;
/// Deepest directory level scanned; the folder itself is level 0.
pub const MAX_DEPTH: usize = 8;
/// ini values (case-insensitive) that disable a script.
pub const DISABLE_VALUES: &[&str] = &["off", "no", "disabled"];

/// Header bytes written when `lua_loader.ini` does not exist yet. Written as-is, not through the
/// ANSI codec: the dash is the UTF-8 sequence `E2 80 94`, which sits in a comment line.
pub const LOADER_INI_HEADER: &[u8] = "; lua_loader.ini \u{2014} Lua Script Loader Configuration\r\n\
; Define execution order for [OnBoot] and [OnLoad] (lowest numbers load first)\r\n\
; Define hotkey triggers under [OnKey] (e.g. script.lua = F1 or script.lua = insert)\r\n\
;\r\n\
; Virtual Key codes reference: https://docs.microsoft.com/en-us/windows/win32/inputdev/virtual-key-codes\r\n\
; Common keys: insert, delete, home, end, pageup, pagedown, space, enter, escape, F1..F12, A..Z, 0..9\r\n\r\n"
    .as_bytes();

// ---------------------------------------------------------------------------------------------
// ANSI code page
// ---------------------------------------------------------------------------------------------

/// The text encoding `lua_loader.ini` is stored in: the process ANSI code page, which is what the
/// Windows `*PrivateProfile*A` functions read and write.
pub trait AnsiCodec {
    /// Decode the file's bytes. Bytes that are not valid in the code page are an error.
    fn decode(&self, bytes: &[u8]) -> Result<String, String>;
    /// Encode text exactly, with no best-fit substitution. `Err` carries the first character that
    /// has no exact form in the code page.
    fn encode(&self, text: &str) -> Result<Vec<u8>, char>;
}

/// CP1252 bytes 0x80..=0x9F as Windows decodes them (`bestfit1252.txt` MBTABLE); every other byte
/// decodes to the code point of the same value.
const CP1252_HIGH_BLOCK: [u16; 32] = [
    0x20AC, 0x0081, 0x201A, 0x0192, 0x201E, 0x2026, 0x2020, 0x2021, 0x02C6, 0x2030, 0x0160, 0x2039, 0x0152, 0x008D,
    0x017D, 0x008F, 0x0090, 0x2018, 0x2019, 0x201C, 0x201D, 0x2022, 0x2013, 0x2014, 0x02DC, 0x2122, 0x0161, 0x203A,
    0x0153, 0x009D, 0x017E, 0x0178,
];

/// Code page 1252, the ANSI code page of Western-European Windows installs.
#[derive(Debug, Clone, Copy, Default)]
pub struct Cp1252;

impl Cp1252 {
    pub fn decode_byte(b: u8) -> char {
        let unit = if (0x80..0xA0).contains(&b) { CP1252_HIGH_BLOCK[usize::from(b - 0x80)] } else { u16::from(b) };
        char::from_u32(u32::from(unit)).expect("CP1252 decodes to BMP scalar values")
    }
}

impl AnsiCodec for Cp1252 {
    fn decode(&self, bytes: &[u8]) -> Result<String, String> {
        Ok(bytes.iter().map(|&b| Cp1252::decode_byte(b)).collect())
    }

    fn encode(&self, text: &str) -> Result<Vec<u8>, char> {
        text.chars()
            .map(|c| match crate::encoding::to_cp1252(c) {
                // The table includes best-fit forms; only a byte that decodes back to `c` is exact.
                Some(b) if Cp1252::decode_byte(b) == c => Ok(b),
                _ => Err(c),
            })
            .collect()
    }
}

/// The running process's ANSI code page (`CP_ACP`).
#[cfg(windows)]
#[derive(Debug, Clone, Copy, Default)]
pub struct ProcessAnsi;

#[cfg(windows)]
impl AnsiCodec for ProcessAnsi {
    fn decode(&self, bytes: &[u8]) -> Result<String, String> {
        use windows_sys::Win32::Globalization::{CP_ACP, MB_ERR_INVALID_CHARS, MultiByteToWideChar};
        if bytes.is_empty() {
            return Ok(String::new());
        }
        let len = i32::try_from(bytes.len()).map_err(|_| format!("{} bytes is too large to decode", bytes.len()))?;
        let invalid = || "bytes are not valid in the process ANSI code page".to_string();
        let n = unsafe { MultiByteToWideChar(CP_ACP, MB_ERR_INVALID_CHARS, bytes.as_ptr(), len, std::ptr::null_mut(), 0) };
        if n <= 0 {
            return Err(invalid());
        }
        let mut wide = vec![0u16; n as usize];
        let got = unsafe { MultiByteToWideChar(CP_ACP, MB_ERR_INVALID_CHARS, bytes.as_ptr(), len, wide.as_mut_ptr(), n) };
        if got != n {
            return Err(invalid());
        }
        String::from_utf16(&wide).map_err(|e| format!("decoded text is not valid UTF-16: {e}"))
    }

    fn encode(&self, text: &str) -> Result<Vec<u8>, char> {
        let mut out = Vec::with_capacity(text.len());
        for c in text.chars() {
            out.extend_from_slice(&encode_char_acp(c).ok_or(c)?);
        }
        Ok(out)
    }
}

/// One character in `CP_ACP` with `WC_NO_BEST_FIT_CHARS`; `None` when it has no exact form.
#[cfg(windows)]
fn encode_char_acp(c: char) -> Option<Vec<u8>> {
    use windows_sys::Win32::Globalization::{CP_ACP, WC_NO_BEST_FIT_CHARS, WideCharToMultiByte};
    let mut units = [0u16; 2];
    let wide = c.encode_utf16(&mut units);
    let wlen = wide.len() as i32;
    let mut buf = [0u8; 8];
    let mut used_default = 0;
    let n = unsafe {
        WideCharToMultiByte(
            CP_ACP,
            WC_NO_BEST_FIT_CHARS,
            wide.as_ptr(),
            wlen,
            buf.as_mut_ptr(),
            buf.len() as i32,
            std::ptr::null(),
            &mut used_default,
        )
    };
    (n > 0 && used_default == 0).then(|| buf[..n as usize].to_vec())
}

/// ASCII-case-insensitive byte order (lowercase folding), used for every script ordering.
pub fn cmp_ignore_ascii_case(a: &str, b: &str) -> Ordering {
    a.bytes().map(|c| c.to_ascii_lowercase()).cmp(b.bytes().map(|c| c.to_ascii_lowercase()))
}

pub fn is_disable_value(value: &str) -> bool {
    DISABLE_VALUES.iter().any(|d| d.eq_ignore_ascii_case(value))
}

/// The line mirrored to the live log feed for a script result: `[<folder>] <rel> -> <result>`.
pub fn feed_line(folder: &str, rel_path: &str, result: &str) -> String {
    format!("[{folder}] {rel_path} -> {result}")
}

/// Create `scripts/` and its `OnBoot`, `OnLoad` and `OnKey` folders where missing.
pub fn ensure_loader_directories(scripts_root: &Path) -> Result<(), String> {
    for dir in [
        scripts_root.to_path_buf(),
        scripts_root.join(FOLDER_ON_BOOT),
        scripts_root.join(FOLDER_ON_LOAD),
        scripts_root.join(FOLDER_ON_KEY),
    ] {
        fs::create_dir_all(&dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    }
    Ok(())
}

/// Turns a script file's bytes into what the executor takes, or refuses them.
pub type Normalize<'a> = dyn FnMut(&Path, Vec<u8>) -> Result<Vec<u8>, String> + 'a;
/// Runs one script and returns its result text, or why it could not run.
pub type Execute<'a> = dyn FnMut(&Script, &[u8]) -> Result<String, String> + 'a;

/// Read a script and pass its bytes through `normalize` (encoding normalisation and the
/// executor's size limit live there). An empty file is an error: there is nothing to run.
pub fn load_script_bytes(
    path: &Path,
    normalize: &mut Normalize<'_>,
) -> Result<Vec<u8>, String> {
    let bytes = fs::read(path).map_err(|e| format!("failed to open {}: {e}", path.display()))?;
    if bytes.is_empty() {
        return Err(format!("{} is empty (0 bytes); nothing to run", path.display()));
    }
    normalize(path, bytes)
}

// ---------------------------------------------------------------------------------------------
// Discovery
// ---------------------------------------------------------------------------------------------

/// What a directory entry is, as far as the scan cares.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EntryAttrs {
    pub directory: bool,
    /// A junction, symlink or other reparse point. Never followed: a junction to an ancestor is
    /// an endless descent.
    pub reparse_point: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryAction {
    SkipReparsePoint,
    /// A directory whose name starts with `_`: library code that is never auto-run.
    SkipLibraryDir,
    Descend,
    Script,
    NotAScript,
}

/// Decide what the scan does with one entry. Reparse points are skipped before anything else,
/// files and directories alike.
pub fn classify_entry(name: &str, attrs: EntryAttrs) -> EntryAction {
    if attrs.reparse_point {
        EntryAction::SkipReparsePoint
    } else if attrs.directory {
        if name.starts_with('_') { EntryAction::SkipLibraryDir } else { EntryAction::Descend }
    } else if name.len() > 4 && name.as_bytes()[name.len() - 4..].eq_ignore_ascii_case(b".lua") {
        EntryAction::Script
    } else {
        EntryAction::NotAScript
    }
}

#[cfg(windows)]
fn entry_attrs(entry: &fs::DirEntry) -> io::Result<EntryAttrs> {
    use std::os::windows::fs::MetadataExt;
    use windows_sys::Win32::Storage::FileSystem::{FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_REPARSE_POINT};
    // DirEntry::metadata on Windows comes from the directory listing and does not follow links.
    let a = entry.metadata()?.file_attributes();
    Ok(EntryAttrs {
        directory: a & FILE_ATTRIBUTE_DIRECTORY != 0,
        reparse_point: a & FILE_ATTRIBUTE_REPARSE_POINT != 0,
    })
}

#[cfg(not(windows))]
fn entry_attrs(entry: &fs::DirEntry) -> io::Result<EntryAttrs> {
    let t = entry.file_type()?;
    Ok(EntryAttrs { directory: t.is_dir(), reparse_point: t.is_symlink() })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScriptFile {
    pub path: PathBuf,
    /// Path relative to the folder, `\`-separated; the script's key in `lua_loader.ini`.
    pub rel_path: String,
}

/// Everything a scan passed over, for the caller to log.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ScanReport {
    /// `.lua` files found after [`MAX_SCRIPTS`] were already kept.
    pub dropped: usize,
    /// Directories deeper than [`MAX_DEPTH`], not scanned.
    pub depth_capped: Vec<PathBuf>,
    pub reparse_skipped: Vec<PathBuf>,
    /// Scripts or directories whose names are not valid Unicode and so cannot be keyed in the ini.
    pub unreadable_names: Vec<PathBuf>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Discovery {
    /// Kept scripts, ASCII-case-insensitively sorted by `rel_path`.
    pub files: Vec<ScriptFile>,
    pub report: ScanReport,
}

/// Scan `scripts_root/<folder>/` recursively for `.lua` files.
///
/// Each directory is listed in ASCII-case-insensitive name order, so which files survive the
/// [`MAX_SCRIPTS`] cap is deterministic. A missing or unreadable directory is an error.
pub fn discover(scripts_root: &Path, folder: &str) -> Result<Discovery, String> {
    let base = scripts_root.join(folder);
    let mut d = Discovery::default();
    walk(&base, "", 0, &mut d)?;
    d.files.sort_by(|a, b| cmp_ignore_ascii_case(&a.rel_path, &b.rel_path));
    Ok(d)
}

fn walk(dir: &Path, sub: &str, depth: usize, d: &mut Discovery) -> Result<(), String> {
    if depth > MAX_DEPTH {
        d.report.depth_capped.push(dir.to_path_buf());
        return Ok(());
    }
    let read_err = |e: io::Error| format!("cannot list {}: {e}", dir.display());
    let mut entries = Vec::new();
    for entry in fs::read_dir(dir).map_err(read_err)? {
        let entry = entry.map_err(read_err)?;
        let attrs = entry_attrs(&entry).map_err(|e| format!("cannot stat {}: {e}", entry.path().display()))?;
        let lossy = entry.file_name().to_string_lossy().into_owned();
        entries.push((lossy, entry, attrs));
    }
    entries.sort_by(|a, b| cmp_ignore_ascii_case(&a.0, &b.0));

    for (lossy, entry, attrs) in entries {
        let action = classify_entry(&lossy, attrs);
        let name = match action {
            EntryAction::SkipReparsePoint => {
                d.report.reparse_skipped.push(entry.path());
                continue;
            }
            EntryAction::SkipLibraryDir | EntryAction::NotAScript => continue,
            EntryAction::Descend | EntryAction::Script => match entry.file_name().into_string() {
                Ok(name) => name,
                Err(_) => {
                    d.report.unreadable_names.push(entry.path());
                    continue;
                }
            },
        };
        let rel = format!("{sub}{name}");
        if action == EntryAction::Descend {
            walk(&entry.path(), &format!("{rel}\\"), depth + 1, d)?;
        } else if d.files.len() < MAX_SCRIPTS {
            d.files.push(ScriptFile { path: entry.path(), rel_path: rel });
        } else {
            d.report.dropped += 1;
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// lua_loader.ini
// ---------------------------------------------------------------------------------------------

/// `lua_loader.ini`, kept line by line so a write-back changes only the lines it must.
///
/// Reading follows the Windows profile-file conventions: `[Section]` headers, `key=value`
/// lines, section and key names matched ASCII-case-insensitively, the first match wins, key and
/// value trimmed, one pair of matching surrounding `"` or `'` quotes removed from a value, and
/// lines starting with `;` ignored. Existing lines are written back byte-for-byte; new lines use
/// the file's line ending (CRLF unless the file uses only LF).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoaderIni {
    lines: Vec<String>,
    crlf: bool,
    trailing_newline: bool,
    dirty: bool,
}

impl LoaderIni {
    pub fn parse(text: &str) -> Self {
        let crlf = text.contains("\r\n") || !text.contains('\n');
        let trailing_newline = text.ends_with('\n');
        let body = text.strip_suffix('\n').unwrap_or(text);
        let lines = if text.is_empty() {
            Vec::new()
        } else {
            body.split('\n').map(|l| l.strip_suffix('\r').unwrap_or(l).to_string()).collect()
        };
        LoaderIni { lines, crlf, trailing_newline, dirty: false }
    }

    /// Read an existing ini, decoding it with `codec`.
    pub fn load(path: &Path, codec: &dyn AnsiCodec) -> Result<Self, String> {
        let bytes = fs::read(path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
        let text = codec.decode(&bytes).map_err(|e| format!("cannot decode {}: {e}", path.display()))?;
        Ok(Self::parse(&text))
    }

    /// Whether [`set`](Self::set) has changed anything since parsing.
    pub fn is_dirty(&self) -> bool {
        self.dirty
    }

    fn section_name(line: &str) -> Option<&str> {
        let t = line.trim();
        let inner = t.strip_prefix('[')?;
        Some(inner.split(']').next().unwrap_or(inner).trim())
    }

    fn key_value(line: &str) -> Option<(&str, &str)> {
        if line.trim_start().starts_with(';') || Self::section_name(line).is_some() {
            return None;
        }
        let (k, v) = line.split_once('=')?;
        Some((k.trim(), v.trim()))
    }

    /// Line range of the first section with this name: (header index, end exclusive).
    fn section_range(&self, section: &str) -> Option<(usize, usize)> {
        let start = self.lines.iter().position(|l| Self::section_name(l).is_some_and(|s| s.eq_ignore_ascii_case(section)))?;
        let end = self.lines[start + 1..]
            .iter()
            .position(|l| Self::section_name(l).is_some())
            .map_or(self.lines.len(), |i| start + 1 + i);
        Some((start, end))
    }

    fn find_key(&self, section: &str, key: &str) -> Option<usize> {
        let (start, end) = self.section_range(section)?;
        (start + 1..end).find(|&i| Self::key_value(&self.lines[i]).is_some_and(|(k, _)| k.eq_ignore_ascii_case(key)))
    }

    /// The value for `key` in `section`, or `None` when absent. An empty value is `Some("")`.
    pub fn get(&self, section: &str, key: &str) -> Option<&str> {
        let i = self.find_key(section, key)?;
        let (_, v) = Self::key_value(&self.lines[i])?;
        let b = v.as_bytes();
        if b.len() >= 2 && (b[0] == b'"' || b[0] == b'\'') && b[b.len() - 1] == b[0] {
            Some(&v[1..v.len() - 1])
        } else {
            Some(v)
        }
    }

    /// Set `key=value` in `section`: replaces the first existing line for the key, else adds the
    /// key after the section's last non-blank line, else appends a new section at the end.
    pub fn set(&mut self, section: &str, key: &str, value: &str) {
        if let Some(i) = self.find_key(section, key) {
            let existing_key = Self::key_value(&self.lines[i]).map(|(k, _)| k.to_string()).unwrap_or_default();
            self.lines[i] = format!("{existing_key}={value}");
        } else if let Some((start, end)) = self.section_range(section) {
            let mut at = end;
            while at > start + 1 && self.lines[at - 1].trim().is_empty() {
                at -= 1;
            }
            self.lines.insert(at, format!("{key}={value}"));
        } else {
            self.lines.push(format!("[{section}]"));
            self.lines.push(format!("{key}={value}"));
            self.trailing_newline = true;
        }
        self.dirty = true;
    }

    pub fn render(&self) -> String {
        let eol = if self.crlf { "\r\n" } else { "\n" };
        let mut out = self.lines.join(eol);
        if self.trailing_newline && !self.lines.is_empty() {
            out.push_str(eol);
        }
        out
    }

    /// Encode with `codec` and write. A line holding a character the code page cannot represent
    /// exactly is an error naming the file and the line, since it could not be read back as written.
    pub fn save(&self, path: &Path, codec: &dyn AnsiCodec) -> Result<(), String> {
        let mut bytes = Vec::new();
        let eol: &[u8] = if self.crlf { b"\r\n" } else { b"\n" };
        for (i, line) in self.lines.iter().enumerate() {
            let encoded = codec.encode(line).map_err(|c| {
                format!(
                    "cannot write {}: the line {line:?} contains {c:?} (U+{:04X}), which has no form in the \
                     process ANSI code page; rename the script so its path is representable",
                    path.display(),
                    u32::from(c)
                )
            })?;
            bytes.extend_from_slice(&encoded);
            if i + 1 < self.lines.len() || self.trailing_newline {
                bytes.extend_from_slice(eol);
            }
        }
        fs::write(path, bytes).map_err(|e| format!("cannot write {}: {e}", path.display()))
    }
}

/// Create the ini with [`LOADER_INI_HEADER`] if it does not exist. Returns whether it was created.
pub fn ensure_loader_ini(path: &Path) -> Result<bool, String> {
    match fs::metadata(path) {
        Ok(_) => Ok(false),
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            fs::write(path, LOADER_INI_HEADER).map_err(|e| format!("cannot create {}: {e}", path.display()))?;
            Ok(true)
        }
        Err(e) => Err(format!("cannot stat {}: {e}", path.display())),
    }
}

// ---------------------------------------------------------------------------------------------
// Registration (ordering and disable state)
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Order {
    Run(i32),
    /// The ini value was `off`, `no` or `disabled`.
    Disabled,
    /// The ini value is neither a whole decimal number nor a disable value.
    Invalid(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Script {
    pub path: PathBuf,
    pub rel_path: String,
    pub order: Order,
}

/// Resolve each script's order from `ini` section `folder`, assigning `(index + 1) * 10` (index
/// in alphabetical order, counting every script) to any script with no value, an empty value or
/// `-1`, and recording that assignment in `ini`. Returns the scripts sorted by order then
/// ASCII-case-insensitive path (scripts that will not run sort as order -1), and the assignments.
pub fn register(ini: &mut LoaderIni, folder: &str, files: Vec<ScriptFile>) -> (Vec<Script>, Vec<(String, i32)>) {
    let mut assigned = Vec::new();
    let mut scripts: Vec<Script> = files
        .into_iter()
        .enumerate()
        .map(|(i, f)| {
            let value = ini.get(folder, &f.rel_path).unwrap_or("").to_string();
            let order = if is_disable_value(&value) {
                Order::Disabled
            } else {
                match parse_order(&value) {
                    Some(Ok(n)) => Order::Run(n),
                    Some(Err(())) => Order::Invalid(value),
                    None => {
                        let n = i32::try_from((i + 1) * 10).expect("at most MAX_SCRIPTS scripts");
                        ini.set(folder, &f.rel_path, &n.to_string());
                        assigned.push((f.rel_path.clone(), n));
                        Order::Run(n)
                    }
                }
            };
            Script { path: f.path, rel_path: f.rel_path, order }
        })
        .collect();
    let key = |s: &Script| match s.order {
        Order::Run(n) => n,
        Order::Disabled | Order::Invalid(_) => -1,
    };
    scripts.sort_by(|a, b| key(a).cmp(&key(b)).then_with(|| cmp_ignore_ascii_case(&a.rel_path, &b.rel_path)));
    (scripts, assigned)
}

/// `None` when the value means "unassigned" (empty or `-1`), `Some(Err)` when it is not a
/// whole decimal number with an optional sign.
fn parse_order(value: &str) -> Option<Result<i32, ()>> {
    if value.is_empty() {
        return None;
    }
    let digits = value.strip_prefix(['-', '+']).unwrap_or(value);
    if digits.is_empty() || !digits.bytes().all(|c| c.is_ascii_digit()) {
        return Some(Err(()));
    }
    match value.parse::<i32>() {
        Ok(-1) => None,
        Ok(n) => Some(Ok(n)),
        Err(_) => Some(Err(())),
    }
}

// ---------------------------------------------------------------------------------------------
// Folder passes
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoaderPaths {
    /// The `scripts/` directory next to the game executable.
    pub scripts_root: PathBuf,
    /// `lua_loader.ini`, next to the bridge module.
    pub ini_path: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FolderRegistration {
    pub folder: String,
    /// In run order.
    pub scripts: Vec<Script>,
    /// Orders newly written to the ini.
    pub assigned: Vec<(String, i32)>,
    pub scan: ScanReport,
    pub ini_created: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScriptOutcome {
    Disabled,
    InvalidOrder(String),
    LoadFailed(String),
    /// The executor could not run the script at all.
    ExecuteFailed(String),
    /// The executor ran it; `result` is its result text (which may describe a Lua error).
    Ran { bytes: usize, result: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScriptReport {
    pub rel_path: String,
    pub outcome: ScriptOutcome,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FolderRun {
    pub registration: FolderRegistration,
    /// One per script, in run order.
    pub reports: Vec<ScriptReport>,
}

/// Folder scans and runs, one at a time.
pub struct ScriptLoader {
    paths: LoaderPaths,
    codec: Box<dyn AnsiCodec + Send + Sync>,
    busy: AtomicBool,
}

struct BusyGuard<'a>(&'a AtomicBool);

impl Drop for BusyGuard<'_> {
    fn drop(&mut self) {
        self.0.store(false, AtomicOrdering::Release);
    }
}

impl ScriptLoader {
    /// `codec` is the ini's encoding; in the game that is the process ANSI code page.
    pub fn new(paths: LoaderPaths, codec: Box<dyn AnsiCodec + Send + Sync>) -> Self {
        ScriptLoader { paths, codec, busy: AtomicBool::new(false) }
    }

    pub fn paths(&self) -> &LoaderPaths {
        &self.paths
    }

    pub fn ensure_directories(&self) -> Result<(), String> {
        ensure_loader_directories(&self.paths.scripts_root)
    }

    /// A script run can re-enter the loader (through the game's own hooks); a second pass while
    /// one is in progress is refused.
    fn enter(&self, what: &str, folder: &str) -> Result<BusyGuard<'_>, String> {
        if self.busy.swap(true, AtomicOrdering::Acquire) {
            return Err(format!("{what}({folder}) refused: another folder pass is still running"));
        }
        Ok(BusyGuard(&self.busy))
    }

    fn register_locked(&self, folder: &str) -> Result<FolderRegistration, String> {
        let ini_created = ensure_loader_ini(&self.paths.ini_path)?;
        let discovery = discover(&self.paths.scripts_root, folder)?;
        let mut reg = FolderRegistration {
            folder: folder.to_string(),
            scripts: Vec::new(),
            assigned: Vec::new(),
            scan: discovery.report,
            ini_created,
        };
        if discovery.files.is_empty() {
            return Ok(reg);
        }
        let mut ini = LoaderIni::load(&self.paths.ini_path, &*self.codec)?;
        let (scripts, assigned) = register(&mut ini, folder, discovery.files);
        if ini.is_dirty() {
            ini.save(&self.paths.ini_path, &*self.codec)?;
        }
        reg.scripts = scripts;
        reg.assigned = assigned;
        Ok(reg)
    }

    /// Scan `folder` and bring `lua_loader.ini` up to date for it, running nothing.
    pub fn register_folder(&self, folder: &str) -> Result<FolderRegistration, String> {
        let _guard = self.enter("register_folder", folder)?;
        self.register_locked(folder)
    }

    /// Register `folder`, then run each enabled script in order.
    ///
    /// `prepare` runs once before the first script when the folder has any scripts.
    /// `normalize` turns a file's bytes into what the executor takes (see [`load_script_bytes`]).
    /// `execute` runs one script and returns its result text; it is the place to log the start of
    /// a script, since a hang inside it never returns here.
    pub fn run_folder(
        &self,
        folder: &str,
        prepare: &mut dyn FnMut() -> Result<(), String>,
        normalize: &mut Normalize<'_>,
        execute: &mut Execute<'_>,
    ) -> Result<FolderRun, String> {
        let _guard = self.enter("run_folder", folder)?;
        let registration = self.register_locked(folder)?;
        let mut reports = Vec::with_capacity(registration.scripts.len());
        if !registration.scripts.is_empty() {
            prepare().map_err(|e| format!("preparing to run scripts/{folder}/ failed: {e}"))?;
        }
        for script in &registration.scripts {
            let outcome = match &script.order {
                Order::Disabled => ScriptOutcome::Disabled,
                Order::Invalid(v) => ScriptOutcome::InvalidOrder(format!(
                    "lua_loader.ini [{folder}] {} = {v:?} is neither a whole number nor off/no/disabled",
                    script.rel_path
                )),
                Order::Run(_) => match load_script_bytes(&script.path, normalize) {
                    Err(e) => ScriptOutcome::LoadFailed(e),
                    Ok(code) => match execute(script, &code) {
                        Ok(result) => ScriptOutcome::Ran { bytes: code.len(), result },
                        Err(e) => ScriptOutcome::ExecuteFailed(e),
                    },
                },
            };
            reports.push(ScriptReport { rel_path: script.rel_path.clone(), outcome });
        }
        Ok(FolderRun { registration, reports })
    }
}

#[cfg(test)]
pub(crate) mod testdir {
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// A fresh directory under the system temp dir, removed on drop.
    pub struct TestDir(pub PathBuf);

    impl TestDir {
        pub fn new(tag: &str) -> Self {
            static N: AtomicUsize = AtomicUsize::new(0);
            let p = std::env::temp_dir().join(format!(
                "lua_bridge_test_{}_{}_{}",
                std::process::id(),
                tag,
                N.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir_all(&p).unwrap();
            TestDir(p)
        }
        pub fn path(&self) -> &Path {
            &self.0
        }
        pub fn file(&self, rel: &str, contents: &str) -> PathBuf {
            let p = self.0.join(rel);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(&p, contents).unwrap();
            p
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            std::fs::remove_dir_all(&self.0).unwrap();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::testdir::TestDir;
    use super::*;

    fn rels(d: &Discovery) -> Vec<&str> {
        d.files.iter().map(|f| f.rel_path.as_str()).collect()
    }

    #[test]
    fn classify_rules() {
        let file = EntryAttrs { directory: false, reparse_point: false };
        let dir = EntryAttrs { directory: true, reparse_point: false };
        let link = EntryAttrs { directory: true, reparse_point: true };
        assert_eq!(classify_entry("a.lua", file), EntryAction::Script);
        assert_eq!(classify_entry("A.LUA", file), EntryAction::Script);
        assert_eq!(classify_entry(".lua", file), EntryAction::NotAScript);
        assert_eq!(classify_entry("a.luac", file), EntryAction::NotAScript);
        assert_eq!(classify_entry("_x.lua", file), EntryAction::Script);
        assert_eq!(classify_entry("_lib", dir), EntryAction::SkipLibraryDir);
        assert_eq!(classify_entry("lib", dir), EntryAction::Descend);
        assert_eq!(classify_entry("lib", link), EntryAction::SkipReparsePoint);
        assert_eq!(classify_entry("a.lua", EntryAttrs { directory: false, reparse_point: true }), EntryAction::SkipReparsePoint);
    }

    #[test]
    fn discover_recursive_sorted_backslash_keys() {
        let t = TestDir::new("disc");
        t.file("OnLoad/b.lua", "");
        t.file("OnLoad/A.lua", "");
        t.file("OnLoad/sub/c.lua", "");
        t.file("OnLoad/_lib/hidden.lua", "");
        t.file("OnLoad/notes.txt", "");
        let d = discover(t.path(), "OnLoad").unwrap();
        assert_eq!(rels(&d), vec!["A.lua", "b.lua", "sub\\c.lua"]);
        assert_eq!(d.files[2].path, t.path().join("OnLoad").join("sub").join("c.lua"));
        assert_eq!(d.report, ScanReport::default());
    }

    #[test]
    fn discover_depth_cap_is_eight_levels_below_the_folder() {
        let t = TestDir::new("depth");
        let eight = (1..=8).map(|i| format!("d{i}")).collect::<Vec<_>>().join("/");
        t.file(&format!("OnBoot/{eight}/deep.lua"), "");
        t.file(&format!("OnBoot/{eight}/d9/too_deep.lua"), "");
        let d = discover(t.path(), "OnBoot").unwrap();
        assert_eq!(rels(&d), vec!["d1\\d2\\d3\\d4\\d5\\d6\\d7\\d8\\deep.lua"]);
        assert_eq!(d.report.depth_capped.len(), 1);
        assert!(d.report.depth_capped[0].ends_with("d9"));
    }

    #[test]
    fn discover_caps_at_max_scripts_and_counts_the_rest() {
        let t = TestDir::new("cap");
        for i in 0..(MAX_SCRIPTS + 3) {
            t.file(&format!("OnBoot/s{i:03}.lua"), "");
        }
        let d = discover(t.path(), "OnBoot").unwrap();
        assert_eq!(d.files.len(), MAX_SCRIPTS);
        assert_eq!(d.report.dropped, 3);
        assert_eq!(d.files.last().unwrap().rel_path, format!("s{:03}.lua", MAX_SCRIPTS - 1));
    }

    #[cfg(unix)]
    #[test]
    fn discover_skips_symlinks() {
        let t = TestDir::new("link");
        t.file("OnBoot/real.lua", "");
        std::os::unix::fs::symlink(t.path().join("OnBoot"), t.path().join("OnBoot/loop")).unwrap();
        std::os::unix::fs::symlink(t.path().join("OnBoot/real.lua"), t.path().join("OnBoot/alias.lua")).unwrap();
        let d = discover(t.path(), "OnBoot").unwrap();
        assert_eq!(rels(&d), vec!["real.lua"]);
        assert_eq!(d.report.reparse_skipped.len(), 2);
    }

    #[test]
    fn discover_missing_folder_is_an_error() {
        let t = TestDir::new("missing");
        assert!(discover(t.path(), "OnBoot").is_err());
    }

    #[test]
    fn ensure_directories_creates_all_four() {
        let t = TestDir::new("dirs");
        let root = t.path().join("scripts");
        ensure_loader_directories(&root).unwrap();
        for f in [FOLDER_ON_BOOT, FOLDER_ON_LOAD, FOLDER_ON_KEY] {
            assert!(root.join(f).is_dir());
        }
        ensure_loader_directories(&root).unwrap();
    }

    #[test]
    fn ini_header_written_once() {
        let t = TestDir::new("hdr");
        let p = t.path().join("lua_loader.ini");
        assert!(ensure_loader_ini(&p).unwrap());
        assert_eq!(std::fs::read(&p).unwrap(), LOADER_INI_HEADER);
        std::fs::write(&p, "[OnBoot]\n").unwrap();
        assert!(!ensure_loader_ini(&p).unwrap());
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "[OnBoot]\n");
    }

    #[test]
    fn ini_get_rules() {
        let ini = LoaderIni::parse("; c=1\n[onboot]\n  A.lua = 10 \nq.lua=\"20\"\ne.lua=\n[OnBoot]\nA.lua=99\n[OnLoad]\nA.lua=5\n");
        assert_eq!(ini.get("OnBoot", "a.LUA"), Some("10"));
        assert_eq!(ini.get("OnBoot", "q.lua"), Some("20"));
        assert_eq!(ini.get("OnBoot", "e.lua"), Some(""));
        assert_eq!(ini.get("OnBoot", "z.lua"), None);
        assert_eq!(ini.get("OnLoad", "A.lua"), Some("5"));
        assert_eq!(ini.get("OnKey", "A.lua"), None);
    }

    #[test]
    fn ini_set_appends_section_with_file_line_endings() {
        let mut ini = LoaderIni::parse("[OnBoot]\nprint_boot.lua=10\n");
        ini.set("OnLoad", "x.lua", "10");
        assert_eq!(ini.render(), "[OnBoot]\nprint_boot.lua=10\n[OnLoad]\nx.lua=10\n");
        let t = TestDir::new("hdrset");
        let p = t.path().join("lua_loader.ini");
        let mut ini = LoaderIni::parse(&Cp1252.decode(LOADER_INI_HEADER).unwrap());
        ini.set("OnKey", "k.lua", "F5");
        ini.save(&p, &Cp1252).unwrap();
        assert_eq!(std::fs::read(&p).unwrap(), [LOADER_INI_HEADER, b"[OnKey]\r\nk.lua=F5\r\n"].concat());
    }

    #[test]
    fn ini_set_inserts_after_last_key_and_replaces_existing() {
        let mut ini = LoaderIni::parse("[OnBoot]\na.lua=10\n\n[OnLoad]\nb.lua=10\n");
        ini.set("OnBoot", "c.lua", "30");
        ini.set("onload", "B.LUA", "off");
        assert_eq!(ini.render(), "[OnBoot]\na.lua=10\nc.lua=30\n\n[OnLoad]\nb.lua=off\n");
        assert!(ini.is_dirty());
    }

    #[test]
    fn ini_untouched_round_trips() {
        let text = "x\r\n[OnBoot]\r\n a = 1 \r\n";
        assert_eq!(LoaderIni::parse(text).render(), text);
        assert_eq!(LoaderIni::parse("a\nb").render(), "a\nb");
    }

    #[test]
    fn register_assigns_writes_back_and_orders() {
        let mut ini = LoaderIni::parse("[OnBoot]\nb.lua=5\nc.lua=OFF\nd.lua=-1\n");
        let files = ["a.lua", "b.lua", "c.lua", "d.lua", "e.lua"]
            .iter()
            .map(|r| ScriptFile { path: PathBuf::from(r), rel_path: r.to_string() })
            .collect();
        let (scripts, assigned) = register(&mut ini, "OnBoot", files);
        let got: Vec<_> = scripts.iter().map(|s| (s.rel_path.as_str(), s.order.clone())).collect();
        assert_eq!(
            got,
            vec![
                ("c.lua", Order::Disabled),
                ("b.lua", Order::Run(5)),
                ("a.lua", Order::Run(10)),
                ("d.lua", Order::Run(40)),
                ("e.lua", Order::Run(50)),
            ]
        );
        assert_eq!(assigned, vec![("a.lua".into(), 10), ("d.lua".into(), 40), ("e.lua".into(), 50)]);
        assert_eq!(ini.render(), "[OnBoot]\nb.lua=5\nc.lua=OFF\nd.lua=40\na.lua=10\ne.lua=50\n");
    }

    #[test]
    fn register_disable_values_and_invalid_orders() {
        let mut ini = LoaderIni::parse("[OnLoad]\na.lua=no\nb.lua=Disabled\nc.lua=0\nd.lua=1O\ne.lua=-7\n");
        let files = ["a.lua", "b.lua", "c.lua", "d.lua", "e.lua"]
            .iter()
            .map(|r| ScriptFile { path: PathBuf::from(r), rel_path: r.to_string() })
            .collect();
        let (scripts, assigned) = register(&mut ini, "OnLoad", files);
        let get = |r: &str| scripts.iter().find(|s| s.rel_path == r).unwrap().order.clone();
        assert_eq!(get("a.lua"), Order::Disabled);
        assert_eq!(get("b.lua"), Order::Disabled);
        assert_eq!(get("c.lua"), Order::Run(0));
        assert_eq!(get("d.lua"), Order::Invalid("1O".into()));
        assert_eq!(get("e.lua"), Order::Run(-7));
        assert!(assigned.is_empty());
        assert!(!ini.is_dirty());
    }

    fn loader(t: &TestDir) -> ScriptLoader {
        let l = ScriptLoader::new(LoaderPaths {
            scripts_root: t.path().join("scripts"),
            ini_path: t.path().join("lua_loader.ini"),
        }, Box::new(Cp1252));
        l.ensure_directories().unwrap();
        l
    }

    #[test]
    fn register_folder_runs_nothing_and_writes_ini() {
        let t = TestDir::new("regonly");
        let l = loader(&t);
        t.file("scripts/OnLoad/z.lua", "x");
        t.file("scripts/OnLoad/y.lua", "x");
        let reg = l.register_folder(FOLDER_ON_LOAD).unwrap();
        assert!(reg.ini_created);
        assert_eq!(reg.assigned, vec![("y.lua".into(), 10), ("z.lua".into(), 20)]);
        let ini = LoaderIni::load(&l.paths().ini_path, &Cp1252).unwrap();
        assert_eq!(ini.get("OnLoad", "z.lua"), Some("20"));
    }

    #[test]
    fn empty_folder_leaves_ini_header_only() {
        let t = TestDir::new("empty");
        let l = loader(&t);
        let reg = l.register_folder(FOLDER_ON_BOOT).unwrap();
        assert!(reg.scripts.is_empty());
        assert_eq!(std::fs::read(&l.paths().ini_path).unwrap(), LOADER_INI_HEADER);
    }

    #[test]
    fn run_folder_in_order_with_outcomes() {
        let t = TestDir::new("run");
        let l = loader(&t);
        std::fs::write(&l.paths().ini_path, "[OnBoot]\nb.lua=5\noff.lua=off\nbad.lua=x\n").unwrap();
        t.file("scripts/OnBoot/a.lua", "return 'a'");
        t.file("scripts/OnBoot/b.lua", "return 'b'");
        t.file("scripts/OnBoot/off.lua", "return 'off'");
        t.file("scripts/OnBoot/bad.lua", "return 'bad'");
        t.file("scripts/OnBoot/empty.lua", "");
        t.file("scripts/OnBoot/reject.lua", "nope");
        let mut prepared = 0;
        let mut ran = Vec::new();
        let run = l
            .run_folder(
                FOLDER_ON_BOOT,
                &mut || {
                    prepared += 1;
                    Ok(())
                },
                &mut |p, b| {
                    if p.ends_with("reject.lua") { Err("too big".into()) } else { Ok(b.to_ascii_uppercase()) }
                },
                &mut |s, code| {
                    ran.push(s.rel_path.clone());
                    Ok(String::from_utf8(code.to_vec()).unwrap())
                },
            )
            .unwrap();
        assert_eq!(prepared, 1);
        assert_eq!(ran, vec!["b.lua", "a.lua"]);
        let outcomes: Vec<_> = run.reports.iter().map(|r| (r.rel_path.as_str(), &r.outcome)).collect();
        assert!(matches!(outcomes[0], ("bad.lua", ScriptOutcome::InvalidOrder(_))));
        assert_eq!(outcomes[1], ("off.lua", &ScriptOutcome::Disabled));
        assert_eq!(outcomes[2], ("b.lua", &ScriptOutcome::Ran { bytes: 10, result: "RETURN 'B'".into() }));
        assert_eq!(outcomes[3], ("a.lua", &ScriptOutcome::Ran { bytes: 10, result: "RETURN 'A'".into() }));
        assert!(matches!(outcomes[4], ("empty.lua", ScriptOutcome::LoadFailed(_))));
        assert_eq!(outcomes[5], ("reject.lua", &ScriptOutcome::LoadFailed("too big".into())));
        assert_eq!(run.reports.len(), 6);
    }

    #[test]
    fn run_folder_execute_failure_is_reported() {
        let t = TestDir::new("exfail");
        let l = loader(&t);
        t.file("scripts/OnBoot/a.lua", "x");
        let run = l
            .run_folder(FOLDER_ON_BOOT, &mut || Ok(()), &mut |_, b| Ok(b), &mut |_, _| Err("no Lua state".into()))
            .unwrap();
        assert_eq!(run.reports[0].outcome, ScriptOutcome::ExecuteFailed("no Lua state".into()));
    }

    #[test]
    fn run_folder_without_scripts_skips_prepare() {
        let t = TestDir::new("noprep");
        let l = loader(&t);
        let run = l
            .run_folder(FOLDER_ON_BOOT, &mut || panic!("prepare must not run"), &mut |_, b| Ok(b), &mut |_, _| unreachable!())
            .unwrap();
        assert!(run.reports.is_empty());
    }

    #[test]
    fn nested_pass_is_refused() {
        let t = TestDir::new("nest");
        let l = loader(&t);
        t.file("scripts/OnBoot/a.lua", "x");
        let mut inner = None;
        l.run_folder(FOLDER_ON_BOOT, &mut || Ok(()), &mut |_, b| Ok(b), &mut |_, _| {
            inner = Some(l.register_folder(FOLDER_ON_LOAD));
            Ok(String::new())
        })
        .unwrap();
        let err = inner.unwrap().unwrap_err();
        assert!(err.contains("another folder pass"), "{err}");
        assert!(l.register_folder(FOLDER_ON_LOAD).is_ok(), "guard released after the pass");
    }

    #[test]
    fn cp1252_every_byte_round_trips_and_matches_the_windows_table() {
        for b in 0..=255u8 {
            let c = Cp1252::decode_byte(b);
            assert_eq!(crate::encoding::to_cp1252(c), Some(b), "byte 0x{b:02X}");
            assert_eq!(Cp1252.encode(&c.to_string()), Ok(vec![b]), "byte 0x{b:02X}");
        }
    }

    #[test]
    fn cp1252_encode_is_exact_not_best_fit() {
        assert_eq!(crate::encoding::to_cp1252('\u{100}'), Some(b'A'));
        assert_eq!(Cp1252.encode("x\u{100}.lua"), Err('\u{100}'));
        assert_eq!(Cp1252.encode("\u{65e5}"), Err('\u{65e5}'));
        assert_eq!(Cp1252.encode("Caf\u{e9} \u{20ac}"), Ok(b"Caf\xe9 \x80".to_vec()));
    }

    #[test]
    fn cp1252_path_keys_round_trip_through_the_ini() {
        let t = TestDir::new("ansi");
        let l = loader(&t);
        std::fs::write(&l.paths().ini_path, b"[OnBoot]\r\nM\xf6bius.lua=5\r\n").unwrap();
        t.file("scripts/OnBoot/M\u{f6}bius.lua", "x");
        t.file("scripts/OnBoot/Caf\u{e9}.lua", "x");
        let reg = l.register_folder(FOLDER_ON_BOOT).unwrap();
        let got: Vec<_> = reg.scripts.iter().map(|s| (s.rel_path.as_str(), s.order.clone())).collect();
        assert_eq!(got, vec![("M\u{f6}bius.lua", Order::Run(5)), ("Caf\u{e9}.lua", Order::Run(10))]);
        assert_eq!(std::fs::read(&l.paths().ini_path).unwrap(), b"[OnBoot]\r\nM\xf6bius.lua=5\r\nCaf\xe9.lua=10\r\n");
    }

    #[test]
    fn path_without_an_ansi_form_is_a_hard_error() {
        let t = TestDir::new("noansi");
        let l = loader(&t);
        std::fs::write(&l.paths().ini_path, b"[OnBoot]\r\na.lua=5\r\n").unwrap();
        t.file("scripts/OnBoot/a.lua", "x");
        t.file("scripts/OnBoot/\u{65e5}\u{672c}.lua", "x");
        let err = l.register_folder(FOLDER_ON_BOOT).unwrap_err();
        assert!(err.contains(&l.paths().ini_path.display().to_string()), "{err}");
        assert!(err.contains("\u{65e5}\u{672c}.lua"), "{err}");
        assert_eq!(std::fs::read(&l.paths().ini_path).unwrap(), b"[OnBoot]\r\na.lua=5\r\n");
    }

    #[test]
    fn feed_line_format() {
        assert_eq!(feed_line("OnKey", "sub\\k.lua", "ok"), "[OnKey] sub\\k.lua -> ok");
    }
}
