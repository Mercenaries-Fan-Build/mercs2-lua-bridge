//! The Lua stack as the bindings see it: arguments read and results pushed through m2-sdk.dll's
//! `m2_lua_*` exports in the game, and through [`fake::FakeLua`] in host tests.
//!
//! Argument indices are 0-based, as `m2_lua_arg_*` take them. An absent argument has type -1.

/// Lua 5.1 type tags (`m2/m2_luastack.h`).
pub const TNIL: i32 = 0;
pub const TBOOLEAN: i32 = 1;
pub const TNUMBER: i32 = 3;
pub const TSTRING: i32 = 4;
pub const TTABLE: i32 = 5;
pub const TFUNCTION: i32 = 6;

/// The longest string argument a binding reads, in bytes.
pub const MAX_ARG_STRING: usize = 64 * 1024;

pub trait Lua {
    /// The argument's type tag, or -1 when it is absent.
    fn arg_type(&self, i: i32) -> i32;
    fn arg_number(&self, i: i32) -> Option<f64>;
    /// Lua truthiness of a present argument (only nil and false are false).
    fn arg_bool(&self, i: i32) -> Option<bool>;
    /// A string argument's bytes, up to its first NUL and [`MAX_ARG_STRING`].
    fn arg_string(&self, i: i32) -> Option<Vec<u8>>;
    fn push_nil(&mut self);
    fn push_bool(&mut self, b: bool);
    fn push_number(&mut self, n: f64);
    fn push_string(&mut self, s: &[u8]);
}

/// The type name `type()` reports for a tag.
pub fn type_name(tag: i32) -> &'static str {
    match tag {
        -1 => "no value",
        TNIL => "nil",
        TBOOLEAN => "boolean",
        2 | 7 => "userdata",
        TNUMBER => "number",
        TSTRING => "string",
        TTABLE => "table",
        TFUNCTION => "function",
        8 => "thread",
        _ => "unknown",
    }
}

/// What a binding wants returned to Lua.
#[derive(Debug, Clone, PartialEq)]
pub enum Ret {
    /// This many values were pushed.
    Values(i32),
    /// Raise a Lua error with this message (nothing was pushed).
    Raise(String),
}

#[cfg(all(windows, target_arch = "x86"))]
pub mod game {
    //! The game's stack through m2-sdk.dll.
    use super::*;
    use core::ffi::{c_char, c_void};
    use mercs2_formats::mod_runtime::ffi;

    pub struct GameLua(pub *mut c_void);

    impl Lua for GameLua {
        fn arg_type(&self, i: i32) -> i32 {
            unsafe { ffi::m2_lua_arg_type(self.0, i) }
        }
        fn arg_number(&self, i: i32) -> Option<f64> {
            let mut v = 0.0;
            (unsafe { ffi::m2_lua_arg_number(self.0, i, &mut v) } == 1).then_some(v)
        }
        fn arg_bool(&self, i: i32) -> Option<bool> {
            let mut v = 0;
            (unsafe { ffi::m2_lua_arg_bool(self.0, i, &mut v) } == 1).then_some(v != 0)
        }
        fn arg_string(&self, i: i32) -> Option<Vec<u8>> {
            if self.arg_type(i) != TSTRING {
                return None;
            }
            let mut buf = vec![0u8; MAX_ARG_STRING + 1];
            let n = unsafe { ffi::m2_lua_arg_string(self.0, i, buf.as_mut_ptr().cast::<c_char>(), buf.len() as i32) };
            buf.truncate(n.max(0) as usize);
            Some(buf)
        }
        fn push_nil(&mut self) {
            unsafe { ffi::m2_lua_push_nil(self.0) }
        }
        fn push_bool(&mut self, b: bool) {
            unsafe { ffi::m2_lua_push_bool(self.0, i32::from(b)) }
        }
        fn push_number(&mut self, n: f64) {
            unsafe { ffi::m2_lua_push_number(self.0, n) }
        }
        fn push_string(&mut self, s: &[u8]) {
            unsafe { ffi::m2_lua_push_string(self.0, s.as_ptr().cast::<c_char>(), s.len() as i32) }
        }
    }

    /// Run a binding on the game's stack: its pushed count, or a raised error. `m2_lua_raise` does not
    /// return, so the message is leaked and the binding has dropped everything it owned by then.
    ///
    /// # Safety
    /// `l` is the `lua_State` the game passed to the Lua C function this runs inside, on the game thread.
    pub unsafe fn call(l: *mut c_void, f: impl FnOnce(&mut GameLua) -> Ret) -> i32 {
        let ret = f(&mut GameLua(l));
        match ret {
            Ret::Values(n) => n,
            Ret::Raise(msg) => {
                let c = std::ffi::CString::new(msg.replace('\0', " ")).expect("NULs replaced");
                let p = c.into_raw();
                unsafe { ffi::m2_lua_raise(l, p) }
            }
        }
    }
}

#[cfg(test)]
pub mod fake {
    //! A stack for host tests: the arguments given, and the values pushed.
    use super::*;

    #[derive(Debug, Clone, PartialEq)]
    pub enum Val {
        Nil,
        Bool(bool),
        Num(f64),
        Str(Vec<u8>),
        Table,
    }

    #[derive(Default)]
    pub struct FakeLua {
        pub args: Vec<Val>,
        pub pushed: Vec<Val>,
    }

    impl FakeLua {
        pub fn with(args: Vec<Val>) -> Self {
            FakeLua { args, pushed: Vec::new() }
        }
        fn arg(&self, i: i32) -> Option<&Val> {
            usize::try_from(i).ok().and_then(|i| self.args.get(i))
        }
    }

    pub fn s(text: &str) -> Val {
        Val::Str(text.as_bytes().to_vec())
    }

    impl Lua for FakeLua {
        fn arg_type(&self, i: i32) -> i32 {
            match self.arg(i) {
                None => -1,
                Some(Val::Nil) => TNIL,
                Some(Val::Bool(_)) => TBOOLEAN,
                Some(Val::Num(_)) => TNUMBER,
                Some(Val::Str(_)) => TSTRING,
                Some(Val::Table) => TTABLE,
            }
        }
        /// The game's `lua_Number` is a 32-bit float: values round through `f32` as they would there.
        fn arg_number(&self, i: i32) -> Option<f64> {
            match self.arg(i) {
                Some(Val::Num(n)) => Some(f64::from(*n as f32)),
                _ => None,
            }
        }
        fn arg_bool(&self, i: i32) -> Option<bool> {
            self.arg(i).map(|v| !matches!(v, Val::Nil | Val::Bool(false)))
        }
        fn arg_string(&self, i: i32) -> Option<Vec<u8>> {
            match self.arg(i) {
                Some(Val::Str(b)) => {
                    let end = b.iter().position(|&c| c == 0).unwrap_or(b.len()).min(MAX_ARG_STRING);
                    Some(b[..end].to_vec())
                }
                _ => None,
            }
        }
        fn push_nil(&mut self) {
            self.pushed.push(Val::Nil);
        }
        fn push_bool(&mut self, b: bool) {
            self.pushed.push(Val::Bool(b));
        }
        fn push_number(&mut self, n: f64) {
            self.pushed.push(Val::Num(f64::from(n as f32)));
        }
        fn push_string(&mut self, s: &[u8]) {
            self.pushed.push(Val::Str(s.to_vec()));
        }
    }
}
