//! `Tcp.Send(host, port, msg)`: open a TCP connection to a loopback address, write `msg`, close.
//!
//! Only 127.0.0.0/8 (or the name `localhost`, which means 127.0.0.1) is reachable, so a script
//! cannot scan the player's network or send data off the machine. The connect and the write each
//! time out after [`TIMEOUT`], so an unresponsive port cannot hold the game thread.
//!
//! Every failure raises a Lua error: a missing or mistyped argument, a host that is not a dotted
//! IPv4 address or `localhost`, a non-loopback address, a port outside 1..=65535, a refused or
//! timed-out connect, a failed write.

use crate::lua_api::{Lua, Ret, TNUMBER, TSTRING, type_name};
use std::io::Write;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4, TcpStream};
use std::time::Duration;

/// How long the connect, and then the write, may take.
pub const TIMEOUT: Duration = Duration::from_millis(500);

fn bad_arg(i: i32, msg: &str) -> String {
    format!("bad argument #{} to 'Tcp.Send' ({msg})", i + 1)
}

fn string_arg<L: Lua>(l: &L, i: i32) -> Result<Vec<u8>, String> {
    let tag = l.arg_type(i);
    match (tag == TSTRING).then(|| l.arg_string(i)).flatten() {
        Some(b) => Ok(b),
        None => Err(bad_arg(i, &format!("string expected, got {}", type_name(tag)))),
    }
}

/// The loopback address `host` names.
pub fn parse_loopback_host(host: &[u8]) -> Result<Ipv4Addr, String> {
    if host == b"localhost" {
        return Ok(Ipv4Addr::LOCALHOST);
    }
    let text = String::from_utf8_lossy(host);
    let ip: Ipv4Addr = std::str::from_utf8(host)
        .ok()
        .and_then(|h| h.parse().ok())
        .ok_or_else(|| bad_arg(0, &format!("'{text}' is not a dotted IPv4 address or 'localhost'")))?;
    if !ip.is_loopback() {
        return Err(bad_arg(
            0,
            &format!("{ip} is not a loopback address (127.0.0.0/8 only)"),
        ));
    }
    Ok(ip)
}

fn port_arg<L: Lua>(l: &L) -> Result<u16, String> {
    let tag = l.arg_type(1);
    if tag != TNUMBER {
        return Err(bad_arg(1, &format!("number expected, got {}", type_name(tag))));
    }
    let n = l
        .arg_number(1)
        .ok_or_else(|| bad_arg(1, "number expected, got an unreadable number"))?;
    let t = (n as f32).trunc();
    if (1.0..=65535.0).contains(&t) {
        Ok(t as u16)
    } else {
        Err(bad_arg(1, &format!("port {n} is outside 1..=65535")))
    }
}

/// `Tcp.Send(sHost, nPort, sMsg)` -> nothing.
pub fn send<L: Lua>(l: &mut L) -> Ret {
    match send_inner(l) {
        Ok(()) => Ret::Values(0),
        Err(e) => Ret::Raise(e),
    }
}

fn send_inner<L: Lua>(l: &L) -> Result<(), String> {
    let host = string_arg(l, 0)?;
    let port = port_arg(l)?;
    let msg = string_arg(l, 2)?;
    let ip = parse_loopback_host(&host)?;
    let addr = SocketAddr::V4(SocketAddrV4::new(ip, port));
    let mut stream =
        TcpStream::connect_timeout(&addr, TIMEOUT).map_err(|e| format!("Tcp.Send: connect to {addr} failed: {e}"))?;
    stream
        .set_write_timeout(Some(TIMEOUT))
        .map_err(|e| format!("Tcp.Send: setting the write timeout on {addr} failed: {e}"))?;
    stream
        .write_all(&msg)
        .map_err(|e| format!("Tcp.Send: writing {} bytes to {addr} failed: {e}", msg.len()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lua_api::fake::{FakeLua, Val, s};
    use std::io::Read;
    use std::net::TcpListener;
    use std::time::Instant;

    fn run(args: Vec<Val>) -> Ret {
        let mut l = FakeLua::with(args);
        let r = send(&mut l);
        assert!(l.pushed.is_empty());
        r
    }

    fn listener() -> (TcpListener, u16) {
        let lst = TcpListener::bind("127.0.0.1:0").expect("bind a loopback listener");
        let port = lst.local_addr().expect("listener address").port();
        (lst, port)
    }

    fn receive_one(lst: TcpListener) -> std::thread::JoinHandle<Vec<u8>> {
        std::thread::spawn(move || {
            let (mut conn, _) = lst.accept().expect("accept");
            let mut got = Vec::new();
            conn.read_to_end(&mut got).expect("read to end");
            got
        })
    }

    #[test]
    fn sends_the_message_to_a_loopback_listener() {
        let (lst, port) = listener();
        let rx = receive_one(lst);
        assert_eq!(
            run(vec![s("127.0.0.1"), Val::Num(f64::from(port)), s("hello\tworld")]),
            Ret::Values(0)
        );
        assert_eq!(rx.join().expect("listener thread"), b"hello\tworld");
    }

    #[test]
    fn localhost_is_accepted() {
        let (lst, port) = listener();
        let rx = receive_one(lst);
        assert_eq!(
            run(vec![s("localhost"), Val::Num(f64::from(port)), s("ping")]),
            Ret::Values(0)
        );
        assert_eq!(rx.join().expect("listener thread"), b"ping");
    }

    #[test]
    fn non_number_port_raises() {
        match run(vec![s("127.0.0.1"), s("8080"), s("x")]) {
            Ret::Raise(m) => assert_eq!(m, "bad argument #2 to 'Tcp.Send' (number expected, got string)"),
            other => panic!("{other:?}"),
        }
        assert!(matches!(run(vec![s("127.0.0.1"), Val::Nil, s("x")]), Ret::Raise(_)));
    }

    #[test]
    fn out_of_range_port_raises() {
        for p in [0.0, 65536.0, -1.0, f64::NAN] {
            match run(vec![s("127.0.0.1"), Val::Num(p), s("x")]) {
                Ret::Raise(m) => assert!(m.contains("outside 1..=65535"), "{m}"),
                other => panic!("{p}: {other:?}"),
            }
        }
    }

    #[test]
    fn non_loopback_and_unparsable_hosts_raise() {
        match run(vec![s("10.0.0.1"), Val::Num(80.0), s("x")]) {
            Ret::Raise(m) => assert!(m.contains("not a loopback address"), "{m}"),
            other => panic!("{other:?}"),
        }
        match run(vec![s("example.com"), Val::Num(80.0), s("x")]) {
            Ret::Raise(m) => assert!(m.contains("not a dotted IPv4 address"), "{m}"),
            other => panic!("{other:?}"),
        }
        assert_eq!(parse_loopback_host(b"127.255.0.9"), Ok(Ipv4Addr::new(127, 255, 0, 9)));
    }

    #[test]
    fn missing_arguments_raise() {
        assert!(matches!(run(vec![]), Ret::Raise(m) if m.contains("#1")));
        assert!(matches!(run(vec![s("127.0.0.1"), Val::Num(80.0)]), Ret::Raise(m) if m.contains("#3")));
    }

    #[test]
    fn refused_connection_raises_promptly() {
        let (lst, port) = listener();
        drop(lst);
        let started = Instant::now();
        match run(vec![s("127.0.0.1"), Val::Num(f64::from(port)), s("x")]) {
            Ret::Raise(m) => assert!(m.starts_with("Tcp.Send: connect to 127.0.0.1:"), "{m}"),
            other => panic!("{other:?}"),
        }
        assert!(started.elapsed() < Duration::from_secs(2));
    }
}
