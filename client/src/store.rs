//! `lua_loader_data.ini`: the values behind `Loader.SaveVar` / `Loader.LoadVar`.
//!
//! One entry per line, `key=<type>:<value>`, with `n` (number), `b` (boolean, `0`/`1`) and `s` (string with
//! `\\`, `\n`, `\r` escaped). Blank lines and lines whose first non-blank byte is `;` or `#` are comments.
//! Entries keep the order in which their keys were first saved, across any number of load/save cycles.
//! Numbers are written with 9 significant digits (C `%.9g`), which round-trips every value of the game's
//! single-precision `lua_Number`. Files are written as CRLF lines; LF and CRLF are both read.

use std::ffi::OsString;
use std::fs;
use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};

/// Longest key, in bytes.
pub const KEY_MAX: usize = 127;
/// Longest string value, in bytes.
pub const STR_MAX: usize = 2048;

/// Comment block at the top of every written file.
const HEADER: &str = "; lua_loader_data.ini \u{2014} auto-managed by lua-bridge Loader.SaveVar/LoadVar\r\n\
; Format: key=type:value   (n=number, s=string, b=boolean 0/1)\r\n\
; Safe to hand-edit while the game is not running; edits made while\r\n\
; the game is running will be overwritten on the next SaveVar.\r\n\
\r\n";

/// One stored value, with the Lua type it was saved as.
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Number(f64),
    Bool(bool),
    Str(Vec<u8>),
}

/// The loaded entries of one data file, and where they are persisted.
#[derive(Debug, Clone)]
pub struct Store {
    path: PathBuf,
    entries: Vec<(Vec<u8>, Value)>,
}

impl Store {
    /// Read `path`. A file that does not exist yet is an empty store; any other read error, or any line
    /// that does not parse, is an error.
    pub fn load(path: impl Into<PathBuf>) -> Result<Store, String> {
        let path = path.into();
        let entries = match fs::read(&path) {
            Ok(bytes) => parse(&bytes).map_err(|e| format!("{}: {e}", path.display()))?,
            Err(e) if e.kind() == ErrorKind::NotFound => Vec::new(),
            Err(e) => return Err(format!("reading {}: {e}", path.display())),
        };
        Ok(Store { path, entries })
    }

    /// The file this store persists to.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Every entry, in first-saved order.
    pub fn entries(&self) -> &[(Vec<u8>, Value)] {
        &self.entries
    }

    pub fn get(&self, key: &[u8]) -> Option<&Value> {
        self.entries.iter().find(|(k, _)| k == key).map(|(_, v)| v)
    }

    /// Insert or replace. A replaced key keeps its position; a new key goes last. A key or value the file
    /// format cannot hold is rejected and the store is left unchanged.
    pub fn set(&mut self, key: &[u8], value: Value) -> Result<(), String> {
        check_entry(key, &value)?;
        upsert(&mut self.entries, key, value);
        Ok(())
    }

    /// Delete `key`; whether it was present. The other entries keep their order.
    pub fn remove(&mut self, key: &[u8]) -> bool {
        let before = self.entries.len();
        self.entries.retain(|(k, _)| k != key);
        self.entries.len() != before
    }

    /// Write every entry to `<path>.tmp`, flush it to disk, then rename it over `path`, so an interrupted
    /// save leaves the previous file whole.
    pub fn persist(&self) -> Result<(), String> {
        let bytes = render(&self.entries)?;
        let mut tmp: OsString = self.path.clone().into_os_string();
        tmp.push(".tmp");
        let tmp = PathBuf::from(tmp);
        let mut f = fs::File::create(&tmp).map_err(|e| format!("creating {}: {e}", tmp.display()))?;
        f.write_all(&bytes).map_err(|e| format!("writing {}: {e}", tmp.display()))?;
        f.sync_all().map_err(|e| format!("flushing {}: {e}", tmp.display()))?;
        drop(f);
        fs::rename(&tmp, &self.path)
            .map_err(|e| format!("replacing {} with {}: {e}", self.path.display(), tmp.display()))
    }
}

fn upsert(entries: &mut Vec<(Vec<u8>, Value)>, key: &[u8], value: Value) {
    match entries.iter_mut().find(|(k, _)| k == key) {
        Some(slot) => slot.1 = value,
        None => entries.push((key.to_vec(), value)),
    }
}

/// Whether `key`/`value` can be written as one line and read back unchanged.
fn check_entry(key: &[u8], value: &Value) -> Result<(), String> {
    let shown = String::from_utf8_lossy(key);
    if key.is_empty() {
        return Err("empty key".into());
    }
    if key.len() > KEY_MAX {
        return Err(format!("key `{shown}` is {} bytes; the limit is {KEY_MAX}", key.len()));
    }
    if let Some(&b) = key.iter().find(|&&b| matches!(b, b'=' | b'\r' | b'\n' | 0)) {
        return Err(format!("key `{shown}` contains byte 0x{b:02X}, which the line format cannot hold"));
    }
    if matches!(key[0], b' ' | b'\t' | b';' | b'#') {
        return Err(format!("key `{shown}` starts with a blank or comment byte"));
    }
    if matches!(key[key.len() - 1], b' ' | b'\t') {
        return Err(format!("key `{shown}` ends with a blank"));
    }
    if let Value::Str(s) = value {
        if s.len() > STR_MAX {
            return Err(format!("string for key `{shown}` is {} bytes; the limit is {STR_MAX}", s.len()));
        }
        if s.contains(&0) {
            return Err(format!("string for key `{shown}` contains a NUL byte"));
        }
    }
    Ok(())
}

/// Parse a data file into its entries, in file order. A key that appears twice keeps its first position
/// and its last value.
pub fn parse(text: &[u8]) -> Result<Vec<(Vec<u8>, Value)>, String> {
    let mut entries: Vec<(Vec<u8>, Value)> = Vec::new();
    for (index, raw) in text.split(|&b| b == b'\n').enumerate() {
        let lineno = index + 1;
        let end = raw.len() - raw.iter().rev().take_while(|&&b| b == b'\r').count();
        let line = &raw[..end];
        let start = line.iter().take_while(|&&b| b == b' ' || b == b'\t').count();
        let line = &line[start..];
        if line.is_empty() || line[0] == b';' || line[0] == b'#' {
            continue;
        }
        if line.contains(&0) {
            return Err(format!("line {lineno}: contains a NUL byte"));
        }
        let eq = line
            .iter()
            .position(|&b| b == b'=')
            .ok_or_else(|| format!("line {lineno}: no `=` between key and value"))?;
        let key_end = eq - line[..eq].iter().rev().take_while(|&&b| b == b' ' || b == b'\t').count();
        let key = &line[..key_end];
        let typed = &line[eq + 1..];
        if typed.len() < 2 || typed[1] != b':' {
            return Err(format!("line {lineno}: value is not `<type>:<value>`"));
        }
        let raw_value = &typed[2..];
        let value = match typed[0] {
            b'n' => Value::Number(parse_number(raw_value).ok_or_else(|| {
                format!("line {lineno}: `{}` is not a number", String::from_utf8_lossy(raw_value))
            })?),
            b'b' => match raw_value {
                b"0" => Value::Bool(false),
                b"1" => Value::Bool(true),
                _ => {
                    return Err(format!(
                        "line {lineno}: boolean must be 0 or 1, got `{}`",
                        String::from_utf8_lossy(raw_value)
                    ));
                }
            },
            b's' => Value::Str(unescape(raw_value).map_err(|e| format!("line {lineno}: {e}"))?),
            other => {
                return Err(format!(
                    "line {lineno}: unknown type `{}`; expected n, b or s",
                    char::from(other).escape_default()
                ));
            }
        };
        check_entry(key, &value).map_err(|e| format!("line {lineno}: {e}"))?;
        upsert(&mut entries, key, value);
    }
    Ok(entries)
}

/// The full file for `entries`, header included.
pub fn render(entries: &[(Vec<u8>, Value)]) -> Result<Vec<u8>, String> {
    let mut out = HEADER.as_bytes().to_vec();
    for (key, value) in entries {
        check_entry(key, value)?;
        out.extend_from_slice(key);
        match value {
            Value::Number(n) => {
                out.extend_from_slice(b"=n:");
                out.extend_from_slice(format_g9(*n).as_bytes());
            }
            Value::Bool(b) => out.extend_from_slice(if *b { b"=b:1" } else { b"=b:0" }),
            Value::Str(s) => {
                out.extend_from_slice(b"=s:");
                out.extend_from_slice(&escape(s));
            }
        }
        out.extend_from_slice(b"\r\n");
    }
    Ok(out)
}

/// A decimal or exponent number, or an infinity/NaN in any of the spellings Windows C runtimes print
/// with `%g`: msvcrt.dll writes `1.#INF`, `1.#IND`, `1.#QNAN` (each optionally negative); the Universal
/// CRT writes `inf`, `nan`, `nan(ind)`, `nan(snan)` (likewise). A leading `-` on a NaN sets its sign bit.
fn parse_number(raw: &[u8]) -> Option<f64> {
    let text = std::str::from_utf8(raw).ok()?;
    let (negative, body) = match text.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, text),
    };
    let special = match body {
        "1.#INF" => Some(f64::INFINITY),
        "1.#IND" | "1.#QNAN" | "nan(ind)" | "nan(snan)" => Some(f64::NAN),
        _ => None,
    };
    match special {
        Some(v) if negative => Some(-v),
        Some(v) => Some(v),
        None => text.parse::<f64>().ok(),
    }
}

fn escape(s: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(s.len());
    for &b in s {
        match b {
            b'\\' => out.extend_from_slice(b"\\\\"),
            b'\n' => out.extend_from_slice(b"\\n"),
            b'\r' => out.extend_from_slice(b"\\r"),
            _ => out.push(b),
        }
    }
    out
}

fn unescape(s: &[u8]) -> Result<Vec<u8>, String> {
    let mut out = Vec::with_capacity(s.len());
    let mut it = s.iter();
    while let Some(&b) = it.next() {
        if b != b'\\' {
            out.push(b);
            continue;
        }
        match it.next() {
            Some(b'n') => out.push(b'\n'),
            Some(b'r') => out.push(b'\r'),
            Some(b'\\') => out.push(b'\\'),
            Some(&other) => {
                return Err(format!(
                    "unknown escape `\\{}`; only \\\\, \\n and \\r are defined",
                    char::from(other).escape_default()
                ));
            }
            None => return Err("string ends in a lone `\\`".into()),
        }
    }
    Ok(out)
}

/// C `printf("%.9g", v)`: 9 significant digits, fixed notation when the decimal exponent is in -4..9,
/// exponent notation (at least two exponent digits) otherwise, trailing zeros and a trailing point removed.
/// Infinities print as `inf`/`-inf` and NaN as `nan`, which `parse` reads back.
pub fn format_g9(v: f64) -> String {
    const P: i32 = 9;
    if v.is_nan() {
        return "nan".into();
    }
    if v.is_infinite() {
        return if v > 0.0 { "inf".into() } else { "-inf".into() };
    }
    if v == 0.0 {
        return if v.is_sign_negative() { "-0".into() } else { "0".into() };
    }
    let sci = format!("{:.*e}", (P - 1) as usize, v);
    let (mantissa, exp) = sci.split_once('e').expect("Rust `{:e}` output always has an exponent");
    let exp: i32 = exp.parse().expect("Rust `{:e}` exponent is a decimal integer");
    if (-4..P).contains(&exp) {
        let fixed = format!("{:.*}", (P - 1 - exp) as usize, v);
        strip_fraction_zeros(&fixed).to_string()
    } else {
        let sign = if exp < 0 { '-' } else { '+' };
        format!("{}e{sign}{:02}", strip_fraction_zeros(mantissa), exp.abs())
    }
}

fn strip_fraction_zeros(s: &str) -> &str {
    if s.contains('.') { s.trim_end_matches('0').trim_end_matches('.') } else { s }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entries(pairs: &[(&str, Value)]) -> Vec<(Vec<u8>, Value)> {
        pairs.iter().map(|(k, v)| (k.as_bytes().to_vec(), v.clone())).collect()
    }

    fn scratch_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("lb_store_{name}_{}", std::process::id()));
        if dir.exists() {
            fs::remove_dir_all(&dir).unwrap();
        }
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn g9_matches_printf() {
        for (v, want) in [
            (0.0, "0"),
            (-0.0, "-0"),
            (1.0, "1"),
            (18.0, "18"),
            (-2.5, "-2.5"),
            (0.1, "0.1"),
            (f64::from(0.1f32), "0.100000001"),
            (1e6, "1000000"),
            (123456789.0, "123456789"),
            (999999999.4, "999999999"),
            (999999999.6, "1e+09"),
            (1e9, "1e+09"),
            (1234567890.0, "1.23456789e+09"),
            (0.0001, "0.0001"),
            (0.00001, "1e-05"),
            (1.5e-7, "1.5e-07"),
            (1e100, "1e+100"),
            (-3.0e-300, "-3e-300"),
            (f64::from(3.4028235e38f32), "3.40282347e+38"),
            (f64::INFINITY, "inf"),
            (f64::NEG_INFINITY, "-inf"),
            (f64::NAN, "nan"),
        ] {
            assert_eq!(format_g9(v), want, "{v:e}");
        }
    }

    #[test]
    fn g9_round_trips_every_tested_f32() {
        let mut x: u32 = 0x1234_5678;
        for _ in 0..200_000 {
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            let f = f32::from_bits(x);
            let text = format_g9(f64::from(f));
            let back = text.parse::<f32>().unwrap();
            if f.is_nan() {
                assert!(back.is_nan(), "{text}");
            } else {
                assert_eq!(back.to_bits(), f.to_bits(), "{f:e} -> {text}");
            }
        }
    }

    #[test]
    fn six_digit_g_values_still_read() {
        let got = parse(b"a=n:4.56789\r\nb=n:1e+06\r\nc=n:-1.5e-05\r\nd=n:18\r\ne=n:inf\r\nf=n:1e+006\r\n").unwrap();
        let nums: Vec<f64> = got
            .iter()
            .map(|(_, v)| match v {
                Value::Number(n) => *n,
                other => panic!("{other:?}"),
            })
            .collect();
        assert_eq!(nums, [4.567_89, 1e6, -1.5e-5, 18.0, f64::INFINITY, 1e6]);
    }

    #[test]
    fn windows_crt_infinity_and_nan_spellings_read() {
        for (spelling, want) in [
            ("1.#INF", f64::INFINITY),
            ("-1.#INF", f64::NEG_INFINITY),
            ("inf", f64::INFINITY),
            ("-inf", f64::NEG_INFINITY),
        ] {
            let got = parse(format!("k=n:{spelling}\r\n").as_bytes()).unwrap();
            assert_eq!(got, entries(&[("k", Value::Number(want))]), "{spelling}");
        }
        for (spelling, negative) in [
            ("1.#IND", false),
            ("-1.#IND", true),
            ("1.#QNAN", false),
            ("-1.#QNAN", true),
            ("nan", false),
            ("-nan", true),
            ("nan(ind)", false),
            ("-nan(ind)", true),
            ("nan(snan)", false),
            ("-nan(snan)", true),
        ] {
            let got = parse(format!("k=n:{spelling}\r\n").as_bytes()).unwrap();
            match got.as_slice() {
                [(k, Value::Number(n))] if k == b"k" => {
                    assert!(n.is_nan(), "{spelling}");
                    assert_eq!(n.is_sign_negative(), negative, "{spelling}");
                }
                other => panic!("{spelling}: {other:?}"),
            }
        }
        for bad in ["1.#INFX", "2.#INF", "1.#", "#INF", "--1.#INF", "nan(", "nan(foo)"] {
            let e = parse(format!("k=n:{bad}\r\n").as_bytes()).unwrap_err();
            assert!(e.contains("not a number"), "{bad}: {e}");
        }
    }

    #[test]
    fn render_layout_and_header() {
        let e = entries(&[
            ("money", Value::Number(1500.0)),
            ("flag", Value::Bool(true)),
            ("off", Value::Bool(false)),
            ("name", Value::Str(b"a=b:c\\d\ne\rf".to_vec())),
        ]);
        let text = render(&e).unwrap();
        let mut want = HEADER.as_bytes().to_vec();
        want.extend_from_slice(b"money=n:1500\r\nflag=b:1\r\noff=b:0\r\nname=s:a=b:c\\\\d\\ne\\rf\r\n");
        assert_eq!(text, want);
        assert_eq!(&text[..22], b"; lua_loader_data.ini ");
        assert_eq!(&text[22..25], [0xE2, 0x80, 0x94]);
    }

    #[test]
    fn parse_render_round_trip() {
        let e = entries(&[
            ("z", Value::Number(-0.25)),
            ("a", Value::Str(Vec::new())),
            ("m", Value::Str(vec![0x80, 0xFF, b'\\', b'\n'])),
            ("b", Value::Bool(false)),
        ]);
        assert_eq!(parse(&render(&e).unwrap()).unwrap(), e);
    }

    #[test]
    fn parse_trims_key_blanks_and_reads_lf_files() {
        let got = parse(b"; c\n# c\n\n   \n\t key \t=n:5\nk2=s:  spaced  \n").unwrap();
        assert_eq!(
            got,
            entries(&[("key", Value::Number(5.0)), ("k2", Value::Str(b"  spaced  ".to_vec()))])
        );
    }

    #[test]
    fn duplicate_key_keeps_first_position_and_last_value() {
        let got = parse(b"a=n:1\nb=n:2\na=s:x\n").unwrap();
        assert_eq!(got, entries(&[("a", Value::Str(b"x".to_vec())), ("b", Value::Number(2.0))]));
    }

    #[test]
    fn malformed_lines_are_errors() {
        for (text, needle) in [
            ("novalue\n", "no `=`"),
            ("k=5\n", "`<type>:<value>`"),
            ("k=\n", "`<type>:<value>`"),
            ("k=x:1\n", "unknown type"),
            ("k=n:abc\n", "not a number"),
            ("k=n:5 \n", "not a number"),
            ("k=b:2\n", "0 or 1"),
            ("k=b:true\n", "0 or 1"),
            ("k=s:C:\\temp\n", "unknown escape"),
            ("k=s:end\\\n", "lone"),
            ("=n:1\n", "empty key"),
            ("k=s:a\0b\n", "NUL"),
        ] {
            let e = parse(text.as_bytes()).expect_err(text);
            assert!(e.contains(needle), "{text:?}: {e}");
            assert!(e.starts_with("line 1:"), "{text:?}: {e}");
        }
        let long_key = format!("{}=n:1\n", "k".repeat(KEY_MAX + 1));
        assert!(parse(long_key.as_bytes()).unwrap_err().contains("limit"));
        let long_str = format!("k=s:{}\n", "x".repeat(STR_MAX + 1));
        assert!(parse(long_str.as_bytes()).unwrap_err().contains("limit"));
    }

    #[test]
    fn set_rejects_unwritable_keys_and_values() {
        let mut s = Store { path: PathBuf::from("unused"), entries: Vec::new() };
        for key in [&b""[..], b"a=b", b"a\nb", b"a\rb", b" a", b"\ta", b";a", b"#a", b"a ", b"a\0"] {
            assert!(s.set(key, Value::Bool(true)).is_err(), "{key:?}");
        }
        assert!(s.set(&[b'k'; KEY_MAX], Value::Bool(true)).is_ok());
        assert!(s.set(b"s", Value::Str(vec![b'x'; STR_MAX + 1])).is_err());
        assert!(s.set(b"s", Value::Str(b"a\0b".to_vec())).is_err());
        assert!(s.set(b"s", Value::Str(vec![b'x'; STR_MAX])).is_ok());
        assert_eq!(s.entries().len(), 2);
    }

    #[test]
    fn missing_file_is_empty_store() {
        let dir = scratch_dir("missing");
        let s = Store::load(dir.join("lua_loader_data.ini")).unwrap();
        assert!(s.entries().is_empty());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn order_survives_repeated_load_and_persist() {
        let dir = scratch_dir("order");
        let path = dir.join("lua_loader_data.ini");
        let mut s = Store::load(&path).unwrap();
        s.set(b"first", Value::Number(1.0)).unwrap();
        s.set(b"second", Value::Bool(true)).unwrap();
        s.set(b"third", Value::Str(b"x".to_vec())).unwrap();
        s.persist().unwrap();
        for round in 0..3 {
            let mut s = Store::load(&path).unwrap();
            let keys: Vec<&[u8]> = s.entries().iter().map(|(k, _)| k.as_slice()).collect();
            assert_eq!(keys, [&b"first"[..], b"second", b"third"], "round {round}");
            s.set(b"second", Value::Bool(round % 2 == 0)).unwrap();
            s.persist().unwrap();
        }
        assert!(!dir.join("lua_loader_data.ini.tmp").exists());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn persist_replaces_existing_file_and_round_trips_values() {
        let dir = scratch_dir("replace");
        let path = dir.join("lua_loader_data.ini");
        fs::write(&path, b"old=n:1\n").unwrap();
        let mut s = Store::load(&path).unwrap();
        assert_eq!(s.get(b"old"), Some(&Value::Number(1.0)));
        let precise = f64::from(1234.5677f32);
        s.set(b"precise", Value::Number(precise)).unwrap();
        s.set(b"old", Value::Str(b"line1\nline2".to_vec())).unwrap();
        s.persist().unwrap();
        let back = Store::load(&path).unwrap();
        let keys: Vec<&[u8]> = back.entries().iter().map(|(k, _)| k.as_slice()).collect();
        assert_eq!(keys, [&b"old"[..], b"precise"]);
        assert_eq!(back.get(b"old"), Some(&Value::Str(b"line1\nline2".to_vec())));
        match back.get(b"precise") {
            Some(Value::Number(n)) => assert_eq!((*n as f32).to_bits(), 1234.5677f32.to_bits()),
            other => panic!("{other:?}"),
        }
        assert_eq!(back.get(b"absent"), None);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn remove_deletes_one_key_and_keeps_the_order() {
        let dir = std::env::temp_dir().join(format!("lb_store_remove_{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("lua_loader_data.ini");
        let _ = fs::remove_file(&path);
        let mut st = Store::load(&path).unwrap();
        st.set(b"a", Value::Number(1.0)).unwrap();
        st.set(b"b", Value::Bool(true)).unwrap();
        st.set(b"c", Value::Str(b"x".to_vec())).unwrap();
        assert!(st.remove(b"b"));
        assert!(!st.remove(b"b"), "already gone");
        st.persist().unwrap();
        let back = Store::load(&path).unwrap();
        assert_eq!(back.entries().iter().map(|(k, _)| k.as_slice()).collect::<Vec<_>>(), [b"a".as_slice(), b"c"]);
        assert_eq!(back.get(b"b"), None);
        fs::remove_dir_all(&dir).unwrap();
    }
}