//! `preimport`: engine Lua modules imported before any script or chunk runs.
//!
//! A library Shipment ships its Lua as an `add_script` module and, by design, nothing imports it:
//! Ess 0.7.0 says so in as many words ("Library only. Nothing runs until a Shipment that requires
//! ess imports it"). Its consumers therefore each ship a loose stub whose whole body is
//! `import("ess")`, placed in `OnLoad` and ordered only by a `0_` filename prefix. That duplicates a
//! dependency the manifest already declares, and the ordering is convention with nothing enforcing
//! it.
//!
//! Listing those module names here imports them *before* a folder or batch runs, so the ordering
//! question disappears and no consumer needs a stub. The names are configuration, not code: the
//! bridge is the dependency of a library Shipment, never the other way round, so no module name is
//! baked in.
//!
//! A name goes straight into a generated `import("<name>")` chunk, so [`validate`] admits only
//! bytes that cannot end the string literal or the statement: letters, digits and `_`. Every engine
//! module name seen in a WAD (`ess`, `ess_names`, `mrxpmc`, `qm_modloader`) is of that shape.
//!
//! # Why this does not run when a state is captured
//!
//! `import` is `_SYS._IMPORT(getfenv(2), module)`: it needs the engine's Lua-side module system
//! (`_MODULES`, `_SYS`) to be built. At the moment a state is first reported that has not happened,
//! and the call faults — a NULL dereference inside the script host, `C0000005` at `EIP=0x005AE362`,
//! `AV READ target=00000040` (`EAX=0` + field `0x40`), with the stack entirely in the Lua VM
//! (`0x85E150`–`0x86A2B0`). PROVEN by that crash: it killed the game 4.2 s in, at phase 0 of 20,
//! before the shell had initialised.
//!
//! The `math`/`assert` polyfill *is* safe there, because it only writes plain `_G` fields. Taking
//! the same lifecycle for `import` was the mistake. These imports therefore run only where the game
//! is already executing Lua of its own — before a loader folder, and before a batch of queued
//! chunks — and never from the state callback. [`Reporter`] keeps the logging to once per state so
//! the before-each-batch repeat is silent.

/// The longest module name accepted. Engine module names are short; this only bounds the chunk.
pub const NAME_MAX: usize = 64;

/// Whether `name` is a module name this may put in an `import()` chunk.
pub fn validate(name: &str) -> Result<(), String> {
    if name.is_empty() {
        return Err("empty module name".into());
    }
    if name.len() > NAME_MAX {
        return Err(format!("module name `{name}` is {} bytes; the limit is {NAME_MAX}", name.len()));
    }
    if let Some(c) = name.chars().find(|c| !(c.is_ascii_alphanumeric() || *c == '_')) {
        return Err(format!(
            "module name `{name}` contains {c:?}; only letters, digits and `_` are allowed"
        ));
    }
    Ok(())
}

/// The chunk that imports `name`. Only ever called with a [`validate`]d name.
pub fn chunk(name: &str) -> String {
    format!("import(\"{name}\")")
}

/// Split a comma-separated list into validated module names, in order. Blank entries are dropped,
/// so a trailing comma and an empty value are both an empty list rather than an error.
pub fn parse_list(value: &str) -> Result<Vec<String>, String> {
    let mut out = Vec::new();
    for part in value.split(',') {
        let name = part.trim();
        if name.is_empty() {
            continue;
        }
        validate(name)?;
        out.push(name.to_string());
    }
    Ok(out)
}

/// Which Lua state's preimport outcomes have been logged, so each state reports once however many
/// folders and batches run on it.
///
/// A state is identified by its pointer. The game can hand out a fresh state at the same address
/// after the old one ends, which would cost that state its one report — acceptable, because the
/// imports themselves still run; only a log line is skipped.
pub struct Reporter {
    last: std::sync::Mutex<Option<usize>>,
}

impl Reporter {
    pub const fn new() -> Self {
        Reporter { last: std::sync::Mutex::new(None) }
    }

    /// True the first time it is asked about `state`, false until the state changes.
    pub fn should_report(&self, state: usize) -> bool {
        let mut last = self.last.lock().expect("preimport reporter lock");
        if *last == Some(state) {
            return false;
        }
        *last = Some(state);
        true
    }
}

impl Default for Reporter {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_module_name_is_letters_digits_and_underscore() {
        for good in ["ess", "ess_names", "mrxpmc", "qm_modloader", "a", "A1_", "_leading"] {
            assert_eq!(validate(good), Ok(()), "{good:?}");
        }
    }

    #[test]
    fn a_name_that_could_escape_the_chunk_is_refused() {
        // Each of these would end the string literal, the call, or the statement.
        for bad in ["", "a\"", "a)", "a(", "a;b", "a b", "a.b", "a-b", "a\nb", "a\\", "a--", "ess,x"] {
            assert!(validate(bad).is_err(), "{bad:?} must not be accepted");
        }
        let long = "e".repeat(NAME_MAX + 1);
        assert!(validate(&long).unwrap_err().contains("the limit is 64"));
        assert_eq!(validate(&"e".repeat(NAME_MAX)), Ok(()), "exactly the limit is fine");
    }

    #[test]
    fn the_chunk_imports_the_one_module() {
        assert_eq!(chunk("ess"), r#"import("ess")"#);
        assert_eq!(chunk("ess_names"), r#"import("ess_names")"#);
    }

    #[test]
    fn a_list_keeps_its_order_and_drops_blanks() {
        assert_eq!(parse_list("ess, ess_names").unwrap(), ["ess", "ess_names"]);
        assert_eq!(parse_list("  ess_names ,ess ").unwrap(), ["ess_names", "ess"]);
        assert_eq!(parse_list("ess,").unwrap(), ["ess"]);
        assert_eq!(parse_list("").unwrap(), Vec::<String>::new());
        assert_eq!(parse_list("  ").unwrap(), Vec::<String>::new());
        assert_eq!(parse_list(",,").unwrap(), Vec::<String>::new());
    }

    #[test]
    fn each_state_reports_once_and_a_new_state_reports_again() {
        let r = Reporter::new();
        assert!(r.should_report(0x2006_FBC0), "first sight of a state reports");
        assert!(!r.should_report(0x2006_FBC0), "the same state stays quiet");
        assert!(!r.should_report(0x2006_FBC0));
        assert!(r.should_report(0x1FEF_FE70), "a different state reports again");
        assert!(!r.should_report(0x1FEF_FE70));
        assert!(r.should_report(0x2006_FBC0), "and back again is a change too");
    }

    #[test]
    fn one_bad_entry_fails_the_whole_list() {
        // Never a partial list: a typo must be reported, not silently dropped.
        let e = parse_list("ess, ess names").unwrap_err();
        assert!(e.contains("ess names"), "{e}");
        assert!(parse_list("ess, a\"b, ess_names").is_err());
    }
}
