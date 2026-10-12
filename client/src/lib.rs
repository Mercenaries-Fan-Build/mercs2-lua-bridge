//! lua_bridge.asi: the Mercenaries 2 Lua bridge. A live REPL and WebSocket into the game's Lua, the
//! OnBoot/OnLoad/OnKey script loader, and the `Loader`, `Tcp` and `math` namespaces, on m2-sdk.dll's Lua
//! host. Everything but [`win`] is plain logic, tested on the host.

pub mod config;
pub mod encoding;
pub mod folders;
pub mod keys;
pub mod loader_ns;
pub mod lua_api;
pub mod math_ns;
pub mod onkey;
pub mod phase;
pub mod polyfill;
pub mod pump;
pub mod queue;
pub mod rearm;
pub mod repl;
pub mod store;
pub mod tcp_ns;
pub mod watchdog;
pub mod ws;

#[cfg(all(windows, target_arch = "x86"))]
mod win;
