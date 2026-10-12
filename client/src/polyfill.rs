//! The Lua chunk that fills in what a `luaL_register` table cannot carry: the constants `math.pi`
//! and `math.huge`, and the base function `assert`, which the game's Lua build does not ship.
//!
//! Every definition is guarded by `if not ... then`, so running the chunk again after the game has
//! rebuilt `_G` restores what is missing and leaves everything else alone.
//!
//! `assert` raises at level 2, so the error points at the caller of `assert`, as stock Lua's does.
//! With the game's 32-bit float `lua_Number`, the literal `1e308` becomes infinity.

/// The chunk text, run once per (re)registration of the namespaces.
pub const POLYFILL: &str = concat!(
    "if math then ",
    "  if not math.pi   then math.pi   = 3.14159265358979323846 end ",
    "  if not math.huge then math.huge = 1e308 end ",
    "end ",
    "if not _G.assert then ",
    "  _G.assert = function(v, msg) ",
    "    if not v then error(msg or 'assertion failed!', 2) end ",
    "    return v ",
    "  end ",
    "end",
);

#[cfg(test)]
mod tests {
    use super::POLYFILL;

    #[test]
    fn polyfill_text_is_exact() {
        let expected = "if math then   if not math.pi   then math.pi   = 3.14159265358979323846 end   \
                        if not math.huge then math.huge = 1e308 end end if not _G.assert then   \
                        _G.assert = function(v, msg)     if not v then error(msg or 'assertion failed!', 2) end     \
                        return v   end end";
        assert_eq!(POLYFILL, expected);
        assert_eq!(POLYFILL.len(), 260);
    }
}
