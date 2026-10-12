//! The raw-TCP REPL server.
//!
//! Protocol, one raw session at a time:
//!
//! ```text
//!   client -> <line>\n ... <<<RUN>>>\n     lines before the RUN line form the chunk
//!   server -> [queued]\n                    sent as soon as the chunk is queued
//!   server -> <result>\n<<<END>>>\n         sent after the game thread has run it
//!   server -> [bridge] chunk rejected: <why>\n<<<END>>>\n
//!                                           instead of [queued], when the chunk cannot be run
//! ```
//!
//! Lines end at `\n`; one trailing `\r` is stripped. The RUN marker must be a whole line. A RUN with
//! no accumulated lines queues nothing and sends nothing. Every raw connection gets a new session,
//! so a result for an earlier connection is never sent on this one.
//!
//! Each accepted connection is handled on its own thread, so the accept loop never waits on a
//! client. When a WebSocket hub is supplied, a connection whose first bytes are `GET ` is upgraded
//! to a WebSocket (see [`crate::ws`]); those first bytes, and the handshake, must arrive within
//! [`FIRST_BYTES_TIMEOUT`]. A raw connection that arrives while another raw session is attached
//! waits for it to end.

use std::io::{self, Read, Write};
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4, TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::queue::{END_MARKER, Normalize, Origin, Queue, lock};
use crate::ws::WsHub;

/// Terminates a request chunk; must be a line of its own.
pub const RUN_MARKER: &[u8] = b"<<<RUN>>>";
/// Acknowledges a queued chunk.
pub const QUEUED_ACK: &[u8] = b"[queued]";
/// Receive buffer. At most `RX_BUFFER - 1` bytes of a not-yet-terminated line can be held; a line
/// that reaches that without a newline ends the connection.
pub const RX_BUFFER: usize = 65536;
/// Chunk buffer. A chunk (each line plus its newline) must stay below this size.
pub const CHUNK_BUFFER: usize = 1048576;
/// How long a new connection may stay silent (and how long a WebSocket handshake may take) when
/// the WebSocket transport is enabled.
pub const FIRST_BYTES_TIMEOUT: Duration = Duration::from_secs(5);
/// How long a raw session waits for input before flushing pending output again.
const POLL: Duration = Duration::from_millis(50);

/// A running REPL listener.
pub struct ReplServer {
    local_addr: SocketAddr,
}

impl ReplServer {
    /// The bound address (the real port when `port` was 0).
    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }
}

struct Ctx {
    queue: Queue,
    ws: Option<WsHub>,
    normalize: Normalize,
    /// Held for the whole of a raw session: one raw client at a time.
    raw_slot: Mutex<()>,
}

/// Bind `host:port` and start the accept thread. A bind failure is returned to the caller.
pub fn start(
    host: Ipv4Addr,
    port: u16,
    queue: Queue,
    ws: Option<WsHub>,
    normalize: Normalize,
) -> io::Result<ReplServer> {
    let listener = TcpListener::bind(SocketAddrV4::new(host, port))?;
    let local_addr = listener.local_addr()?;
    queue.log(&format!("[*] lua_bridge: listening on {local_addr}"));
    let ctx = Arc::new(Ctx {
        queue,
        ws,
        normalize,
        raw_slot: Mutex::new(()),
    });
    std::thread::Builder::new()
        .name("lua_bridge accept".into())
        .spawn(move || accept_loop(&listener, &ctx))?;
    Ok(ReplServer { local_addr })
}

fn accept_loop(listener: &TcpListener, ctx: &Arc<Ctx>) {
    loop {
        let (stream, peer) = match listener.accept() {
            Ok(a) => a,
            Err(e) => {
                ctx.queue.log(&format!("[!] lua_bridge: accept failed: {e}"));
                continue;
            }
        };
        let c = Arc::clone(ctx);
        if let Err(e) = std::thread::Builder::new()
            .name("lua_bridge connection".into())
            .spawn(move || connection(&c, stream, peer))
        {
            ctx.queue.log(&format!(
                "[!] lua_bridge: could not start a thread for {peer}; connection closed: {e}"
            ));
        }
    }
}

fn connection(ctx: &Ctx, stream: TcpStream, peer: SocketAddr) {
    let log = |m: &str| ctx.queue.log(m);
    if let Some(hub) = &ctx.ws {
        if let Err(e) = stream.set_read_timeout(Some(FIRST_BYTES_TIMEOUT)) {
            log(&format!("[!] lua_bridge: {peer}: could not set a read timeout; closed: {e}"));
            return;
        }
        let mut peek = [0u8; 16];
        match stream.peek(&mut peek) {
            Ok(n) if n >= 4 && &peek[..4] == b"GET " => {
                hub.serve(stream);
                return;
            }
            Ok(0) => {
                log(&format!("[*] lua_bridge: {peer} closed without sending anything"));
                return;
            }
            Ok(_) => {}
            Err(e) if matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut) => {
                log(&format!(
                    "[!] lua_bridge: closed silent connection from {peer}: nothing received \
                     within {} s",
                    FIRST_BYTES_TIMEOUT.as_secs()
                ));
                return;
            }
            Err(e) => {
                log(&format!("[!] lua_bridge: {peer}: peek failed; closed: {e}"));
                return;
            }
        }
    }

    let _slot = lock(&ctx.raw_slot, "raw session slot");
    let session = ctx.queue.begin_session();
    log(&format!("[*] lua_bridge: session {session} attached ({peer})"));
    let end = serve_raw(stream, session, &ctx.queue, &ctx.normalize);
    ctx.queue.end_session(session);
    match end {
        Ok(()) => log(&format!("[*] lua_bridge: session {session} ended ({peer} disconnected)")),
        Err(e) => log(&format!("[!] lua_bridge: session {session} ended: {e}")),
    }
}

/// Why a raw session stopped, when it was not a plain disconnect.
#[derive(Debug)]
enum RawEnd {
    Io(io::Error),
    LineTooLong,
}

impl std::fmt::Display for RawEnd {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RawEnd::Io(e) => write!(f, "I/O error: {e}"),
            RawEnd::LineTooLong => write!(
                f,
                "a line reached {} bytes without a newline; connection closed",
                RX_BUFFER - 1
            ),
        }
    }
}

/// Assembles lines into chunks for one session.
struct Framer {
    rx: Vec<u8>,
    chunk: Vec<u8>,
    /// Set when a line did not fit; the chunk is rejected at the next RUN.
    overflowed: bool,
}

/// What one RUN line produced.
#[derive(Debug, PartialEq, Eq)]
enum Run {
    Chunk(Vec<u8>),
    Rejected(String),
}

impl Framer {
    fn new() -> Framer {
        Framer {
            rx: Vec::with_capacity(RX_BUFFER),
            chunk: Vec::new(),
            overflowed: false,
        }
    }

    /// Room left for the next read.
    fn room(&self) -> usize {
        RX_BUFFER - 1 - self.rx.len()
    }

    /// Feed received bytes; returns the RUNs completed by them.
    fn feed(&mut self, bytes: &[u8]) -> Vec<Run> {
        self.rx.extend_from_slice(bytes);
        let mut runs = Vec::new();
        let mut start = 0;
        while let Some(i) = self.rx[start..].iter().position(|&b| b == b'\n') {
            let mut line = &self.rx[start..start + i];
            if let [rest @ .., b'\r'] = line {
                line = rest;
            }
            if line == RUN_MARKER {
                if self.overflowed {
                    runs.push(Run::Rejected(format!(
                        "larger than the {}-byte chunk limit",
                        CHUNK_BUFFER - 1
                    )));
                } else if !self.chunk.is_empty() {
                    runs.push(Run::Chunk(std::mem::take(&mut self.chunk)));
                }
                self.chunk.clear();
                self.overflowed = false;
            } else if self.chunk.len() + line.len() + 1 < CHUNK_BUFFER {
                self.chunk.extend_from_slice(line);
                self.chunk.push(b'\n');
            } else {
                self.overflowed = true;
            }
            start += i + 1;
        }
        self.rx.drain(..start);
        runs
    }
}

/// Reject a chunk on the wire: `[bridge] chunk rejected: <why>\n<<<END>>>\n`.
fn reject(queue: &Queue, session: u64, why: &str) {
    let msg = format!("[bridge] chunk rejected: {why}");
    queue.log(&format!("[!] lua_bridge: session {session}: {msg}"));
    queue.append_output(msg.as_bytes());
    queue.append_output(END_MARKER);
}

fn serve_raw(
    mut stream: TcpStream,
    session: u64,
    queue: &Queue,
    normalize: &Normalize,
) -> Result<(), RawEnd> {
    stream.set_read_timeout(Some(POLL)).map_err(RawEnd::Io)?;
    let mut framer = Framer::new();
    let mut buf = vec![0u8; RX_BUFFER];
    loop {
        let out = queue.take_output();
        if !out.is_empty() {
            stream.write_all(&out).map_err(RawEnd::Io)?;
        }

        let room = framer.room();
        if room == 0 {
            let mut reply =
                format!("[bridge] line rejected: {} bytes without a newline\n", RX_BUFFER - 1)
                    .into_bytes();
            reply.extend_from_slice(END_MARKER);
            reply.push(b'\n');
            stream.write_all(&reply).map_err(RawEnd::Io)?;
            return Err(RawEnd::LineTooLong);
        }
        let n = match stream.read(&mut buf[..room]) {
            Ok(0) => return Ok(()),
            Ok(n) => n,
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut | io::ErrorKind::Interrupted
                ) =>
            {
                continue;
            }
            Err(e) => return Err(RawEnd::Io(e)),
        };

        for run in framer.feed(&buf[..n]) {
            match run {
                Run::Chunk(code) => match normalize("repl chunk", &code) {
                    Ok(code) => {
                        queue.log(&format!(
                            "[+] lua_bridge: queued chunk ({} bytes, session {session})",
                            code.len()
                        ));
                        // The ack goes into the output buffer before the chunk is visible to the
                        // game thread, so it always precedes the chunk's result on the wire.
                        queue.append_output(QUEUED_ACK);
                        queue.push(code, Origin::Raw(session));
                    }
                    Err(why) => reject(queue, session, &why),
                },
                Run::Rejected(why) => reject(queue, session, &why),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::queue::Delivery;
    use crate::queue::tests::recording_log;
    use crate::ws::{self, OP_CLOSE, OP_PING, OP_PONG, OP_TEXT};
    use std::time::Instant;

    type Lines = Arc<Mutex<Vec<String>>>;

    fn identity() -> Normalize {
        Arc::new(|_: &str, b: &[u8]| Ok(b.to_vec()))
    }

    fn server_with(ws_on: bool, normalize: Normalize) -> (Queue, Option<WsHub>, SocketAddr, Lines) {
        let (log, lines) = recording_log();
        let q = Queue::new(log);
        let hub = ws_on.then(|| WsHub::new(q.clone(), Arc::clone(&normalize)));
        let s = start(Ipv4Addr::LOCALHOST, 0, q.clone(), hub.clone(), normalize).unwrap();
        (q, hub, s.local_addr(), lines)
    }

    fn server(ws_on: bool) -> (Queue, Option<WsHub>, SocketAddr, Lines) {
        server_with(ws_on, identity())
    }

    fn connect(addr: SocketAddr) -> TcpStream {
        let s = TcpStream::connect(addr).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
        s
    }

    fn read_exact_n(s: &mut TcpStream, n: usize) -> Vec<u8> {
        let mut b = vec![0u8; n];
        s.read_exact(&mut b).unwrap();
        b
    }

    fn wait_until(what: &str, mut cond: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !cond() {
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    fn wait_pending(q: &Queue, n: usize) {
        wait_until(&format!("{n} pending"), || q.pending() == n);
    }

    fn logged(lines: &Lines, needle: &str) -> bool {
        lines.lock().unwrap().iter().any(|l| l.contains(needle))
    }

    #[test]
    fn framer_lines_crlf_empty_run_and_cap() {
        let mut f = Framer::new();
        assert_eq!(f.feed(b"a = 1\r\nb"), vec![]);
        assert_eq!(f.feed(b" = 2\n<<<RUN>>>\r\n"), vec![Run::Chunk(b"a = 1\nb = 2\n".to_vec())]);
        assert_eq!(f.feed(b"<<<RUN>>>\n"), vec![]);
        assert_eq!(f.feed(b" <<<RUN>>>\nx\n<<<RUN>>>\n"), vec![Run::Chunk(b" <<<RUN>>>\nx\n".to_vec())]);
        let line = vec![b'x'; 60000];
        for _ in 0..18 {
            f.feed(&line);
            f.feed(b"\n");
        }
        let r = f.feed(b"<<<RUN>>>\n");
        assert!(matches!(&r[..], [Run::Rejected(m)] if m.contains("1048575")));
        assert_eq!(f.feed(b"ok\n<<<RUN>>>\n"), vec![Run::Chunk(b"ok\n".to_vec())]);
    }

    #[test]
    fn framer_accepts_a_chunk_just_under_the_cap() {
        let mut f = Framer::new();
        let line = vec![b'y'; 65533];
        let mut total = 0;
        while total + 65534 < CHUNK_BUFFER {
            f.feed(&line);
            f.feed(b"\n");
            total += 65534;
        }
        let last = vec![b'z'; CHUNK_BUFFER - 2 - total];
        f.feed(&last);
        f.feed(b"\n");
        match &f.feed(b"<<<RUN>>>\n")[..] {
            [Run::Chunk(c)] => assert_eq!(c.len(), CHUNK_BUFFER - 1),
            other => panic!("expected a chunk, got {other:?}"),
        }
    }

    #[test]
    fn wire_bytes_queued_then_result_then_end() {
        let (q, _, addr, _) = server(true);
        let mut c = connect(addr);
        c.write_all(b"return 1\n<<<RUN>>>\n").unwrap();
        assert_eq!(read_exact_n(&mut c, 9), b"[queued]\n");
        wait_pending(&q, 1);
        let chunk = q.pop().unwrap();
        assert_eq!(chunk.code, b"return 1\n");
        assert_eq!(q.deliver(chunk.origin, b"1"), Delivery::Delivered);
        assert_eq!(read_exact_n(&mut c, 12), b"1\n<<<END>>>\n");
    }

    #[test]
    fn normalize_runs_before_queueing() {
        let norm: Normalize = Arc::new(|label: &str, b: &[u8]| {
            assert_eq!(label, "repl chunk");
            Ok(b.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(b).to_vec())
        });
        let (q, _, addr, _) = server_with(false, norm);
        let mut c = connect(addr);
        c.write_all(b"\xEF\xBB\xBFx=1\n<<<RUN>>>\n").unwrap();
        assert_eq!(read_exact_n(&mut c, 9), b"[queued]\n");
        wait_pending(&q, 1);
        assert_eq!(q.pop().unwrap().code, b"x=1\n");
    }

    #[test]
    fn normalize_error_rejects_the_chunk_on_the_wire() {
        let norm: Normalize = Arc::new(|_: &str, b: &[u8]| {
            if b.starts_with(b"\xFF\xFE") {
                Err("looks UTF-16 encoded".into())
            } else {
                Ok(b.to_vec())
            }
        });
        let (q, _, addr, lines) = server_with(false, norm);
        let mut c = connect(addr);
        c.write_all(b"\xFF\xFEx\n<<<RUN>>>\nok\n<<<RUN>>>\n").unwrap();
        let expect = b"[bridge] chunk rejected: looks UTF-16 encoded\n<<<END>>>\n[queued]\n";
        assert_eq!(read_exact_n(&mut c, expect.len()), expect);
        wait_pending(&q, 1);
        assert_eq!(q.pop().unwrap().code, b"ok\n");
        assert!(logged(&lines, "chunk rejected: looks UTF-16 encoded"));
    }

    #[test]
    fn result_for_a_departed_client_is_not_sent_to_the_next() {
        let (q, _, addr, lines) = server(true);
        let mut a = connect(addr);
        a.write_all(b"slow()\n<<<RUN>>>\n").unwrap();
        assert_eq!(read_exact_n(&mut a, 9), b"[queued]\n");
        drop(a);
        wait_until("session A to end", || q.current_session().is_none());
        wait_pending(&q, 1);
        let stale = q.pop().unwrap();
        assert!(matches!(
            q.deliver(stale.origin, b"A"),
            Delivery::DroppedStale { current: None, .. }
        ));
        assert!(logged(&lines, "dropped result"));

        let mut b = connect(addr);
        b.write_all(b"fast()\n<<<RUN>>>\n").unwrap();
        wait_pending(&q, 1);
        let mine = q.pop().unwrap();
        assert_eq!(q.deliver(mine.origin, b"B"), Delivery::Delivered);
        assert_eq!(read_exact_n(&mut b, 9 + 12), b"[queued]\nB\n<<<END>>>\n");
    }

    #[test]
    fn second_raw_client_waits_for_the_first() {
        let (q, _, addr, _) = server(true);
        let mut a = connect(addr);
        a.write_all(b"a()\n<<<RUN>>>\n").unwrap();
        assert_eq!(read_exact_n(&mut a, 9), b"[queued]\n");
        let mut b = connect(addr);
        b.write_all(b"b()\n<<<RUN>>>\n").unwrap();
        std::thread::sleep(Duration::from_millis(300));
        assert_eq!(q.pending(), 1, "the waiting client's chunk must not be read yet");
        drop(a);
        assert_eq!(read_exact_n(&mut b, 9), b"[queued]\n");
        wait_pending(&q, 2);
    }

    #[test]
    fn oversize_line_is_rejected_and_closes() {
        let (q, _, addr, _) = server(false);
        let mut c = connect(addr);
        c.write_all(&vec![b'x'; RX_BUFFER - 1]).unwrap();
        let mut got = Vec::new();
        c.read_to_end(&mut got).unwrap();
        assert_eq!(got, b"[bridge] line rejected: 65535 bytes without a newline\n<<<END>>>\n");
        assert_eq!(q.pending(), 0);
    }

    #[test]
    fn silent_connection_is_closed_after_the_timeout() {
        let (_, _, addr, lines) = server(true);
        let mut c = connect(addr);
        let started = Instant::now();
        let mut b = [0u8; 1];
        assert_eq!(c.read(&mut b).unwrap(), 0);
        assert!(started.elapsed() >= FIRST_BYTES_TIMEOUT - Duration::from_millis(100));
        wait_until("the silent-connection log line", || {
            logged(&lines, "closed silent connection")
        });
    }

    /// The client framing of the `mercs2_bridge` crate: `Bridge::eval` writes `chunk` then
    /// `<<<RUN>>>` with no newline after the marker, then reads until the first `<<<END>>>`.
    fn bridge_client_eval(s: &mut TcpStream, chunk: &str) {
        s.write_all(chunk.as_bytes()).unwrap();
        s.write_all(b"<<<RUN>>>").unwrap();
        s.flush().unwrap();
    }

    fn bridge_client_read_until_end(s: &mut TcpStream) -> String {
        let mut buf = Vec::new();
        let mut chunk = [0u8; 4096];
        loop {
            let n = s.read(&mut chunk).unwrap();
            assert_ne!(n, 0, "server closed");
            buf.extend_from_slice(&chunk[..n]);
            if let Some(pos) = buf.windows(END_MARKER.len()).position(|w| w == END_MARKER) {
                buf.truncate(pos);
                return String::from_utf8_lossy(&buf).into_owned();
            }
        }
    }

    #[test]
    fn mercs2_bridge_client_framing_needs_a_newline_after_run() {
        let (q, _, addr, _) = server(true);
        let mut c = connect(addr);
        // As `mercs2_repl` calls it: the chunk with one trailing newline.
        bridge_client_eval(&mut c, "print(1)\n");
        std::thread::sleep(Duration::from_millis(300));
        assert_eq!(q.pending(), 0, "an unterminated RUN line must not queue");

        // The marker line completes only once its newline arrives.
        c.write_all(b"\n").unwrap();
        wait_pending(&q, 1);
        let chunk = q.pop().unwrap();
        assert_eq!(chunk.code, b"print(1)\n");
        q.deliver(chunk.origin, b"1");
        // The client's read-until-END sees the ack line ahead of the result.
        assert_eq!(bridge_client_read_until_end(&mut c), "[queued]\n1\n");
    }

    // --- WebSocket over the shared port ---------------------------------------------------------

    fn masked(opcode: u8, payload: &[u8]) -> Vec<u8> {
        let mask = [1u8, 2, 3, 4];
        let mut f = vec![0x80 | opcode];
        let len = payload.len();
        if len < 126 {
            f.push(0x80 | len as u8);
        } else {
            f.push(0x80 | 126);
            f.extend_from_slice(&(len as u16).to_be_bytes());
        }
        f.extend_from_slice(&mask);
        f.extend(payload.iter().enumerate().map(|(i, b)| b ^ mask[i & 3]));
        f
    }

    fn read_frame(s: &mut TcpStream) -> (u8, Vec<u8>) {
        let h = read_exact_n(s, 2);
        let mut len = usize::from(h[1] & 0x7F);
        if len == 126 {
            let e = read_exact_n(s, 2);
            len = usize::from(u16::from_be_bytes([e[0], e[1]]));
        } else if len == 127 {
            let e = read_exact_n(s, 8);
            len = u64::from_be_bytes(e.try_into().unwrap()) as usize;
        }
        (h[0], read_exact_n(s, len))
    }

    const UPGRADE: &[u8] = b"GET / HTTP/1.1\r\nHost: 127.0.0.1\r\nUpgrade: websocket\r\n\
        Connection: Upgrade\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\
        Sec-WebSocket-Version: 13\r\n\r\n";

    fn ws_connect(addr: SocketAddr) -> TcpStream {
        let mut c = connect(addr);
        c.write_all(UPGRADE).unwrap();
        let expect = ws::handshake_response("s3pPLMBiTxaQ9kYGzzhZRbK+xOo=");
        assert_eq!(read_exact_n(&mut c, expect.len()), expect.as_bytes());
        c
    }

    fn wait_clients(hub: &WsHub, n: usize) {
        wait_until(&format!("{n} websocket clients"), || hub.client_count() == n);
    }

    #[test]
    fn ws_request_ack_and_queue() {
        let (q, hub, addr, _) = server(true);
        let hub = hub.unwrap();
        let mut c = ws_connect(addr);
        c.write_all(&masked(OP_TEXT, br#"{"id":"q\"1","code":"return 2"}"#)).unwrap();
        let (h, body) = read_frame(&mut c);
        assert_eq!(h, 0x81);
        assert_eq!(body, br#"{"type":"ack","id":"q\"1","status":"queued"}"#);
        wait_pending(&q, 1);
        let chunk = q.pop().unwrap();
        assert_eq!((chunk.code.as_slice(), chunk.origin), (&b"return 2"[..], Origin::Ws));

        // A non-ASCII id comes back as the same UTF-8 text.
        let id = "\u{e9}t\u{e9}-\u{1F600}";
        let req = format!("{{\"id\":\"{id}\",\"code\":\"x\"}}");
        c.write_all(&masked(OP_TEXT, req.as_bytes())).unwrap();
        let (_, body) = read_frame(&mut c);
        assert_eq!(body, format!("{{\"type\":\"ack\",\"id\":\"{id}\",\"status\":\"queued\"}}").as_bytes());

        // Malformed requests are logged and skipped; the connection stays up.
        c.write_all(&masked(OP_TEXT, b"not json")).unwrap();
        c.write_all(&masked(OP_PING, b"hb")).unwrap();
        assert_eq!(read_frame(&mut c), (0x80 | OP_PONG, b"hb".to_vec()));

        wait_clients(&hub, 1);
        hub.broadcast("ws", b"tag \x92ok\x92");
        assert_eq!(
            read_frame(&mut c),
            (
                0x81,
                format!(r#"{{"type":"ws","line":"tag {q}ok{q}"}}"#, q = ws::tests::esc("2019"))
                    .into_bytes()
            )
        );

        c.write_all(&masked(OP_CLOSE, b"\x03\xe8")).unwrap();
        assert_eq!(read_frame(&mut c), (0x80 | OP_CLOSE, b"\x03\xe8".to_vec()));
        wait_clients(&hub, 0);
    }

    #[test]
    fn broadcast_through_a_cloned_hub_reaches_a_client() {
        let (_, hub, addr, _) = server(true);
        // The server holds the original; the game thread keeps its own clone.
        let game_side: WsHub = hub.clone().unwrap();
        drop(hub);
        let mut c = ws_connect(addr);
        wait_clients(&game_side, 1);
        std::thread::spawn(move || game_side.broadcast("log", b"from the clone"))
            .join()
            .unwrap();
        assert_eq!(read_frame(&mut c).1, br#"{"type":"log","line":"from the clone"}"#);
    }

    #[test]
    fn ws_normalize_error_acks_rejected_and_logs_to_the_feed() {
        let norm: Normalize = Arc::new(|_: &str, _: &[u8]| Err("looks UTF-16 encoded".into()));
        let (q, hub, addr, lines) = server_with(true, norm);
        let hub = hub.unwrap();
        let mut c = ws_connect(addr);
        wait_clients(&hub, 1);
        c.write_all(&masked(OP_TEXT, br#"{"id":"r1","code":"x"}"#)).unwrap();
        let mut frames = vec![read_frame(&mut c).1, read_frame(&mut c).1];
        frames.sort();
        let mut expect = vec![
            br#"{"type":"ack","id":"r1","status":"rejected","error":"looks UTF-16 encoded"}"#
                .to_vec(),
            br#"{"type":"log","line":"[!] lua_bridge: ws: chunk id=r1 rejected: looks UTF-16 encoded"}"#
                .to_vec(),
        ];
        expect.sort();
        assert_eq!(frames, expect);
        assert_eq!(q.pending(), 0);
        assert!(logged(&lines, "rejected chunk (id=r1"));
    }

    #[test]
    fn ws_connects_while_a_raw_session_is_attached() {
        let (q, hub, addr, _) = server(true);
        let hub = hub.unwrap();
        let mut raw = connect(addr);
        raw.write_all(b"x()\n<<<RUN>>>\n").unwrap();
        assert_eq!(read_exact_n(&mut raw, 9), b"[queued]\n");
        let session = q.current_session().unwrap();
        // The raw client now sits idle, attached.
        let mut c = ws_connect(addr);
        wait_clients(&hub, 1);
        c.write_all(&masked(OP_TEXT, br#"{"id":"w","code":"y"}"#)).unwrap();
        assert_eq!(read_frame(&mut c).1, br#"{"type":"ack","id":"w","status":"queued"}"#);
        assert_eq!(q.current_session(), Some(session));
        drop(raw);
    }

    #[test]
    fn ws_seventeenth_client_gets_503_before_any_101() {
        let (_, hub, addr, lines) = server(true);
        let hub = hub.unwrap();
        let clients: Vec<TcpStream> = (0..ws::MAX_CLIENTS).map(|_| ws_connect(addr)).collect();
        wait_clients(&hub, ws::MAX_CLIENTS);
        let mut extra = connect(addr);
        extra.write_all(UPGRADE).unwrap();
        let mut got = Vec::new();
        extra.read_to_end(&mut got).unwrap();
        assert_eq!(got, ws::busy_response().as_bytes());
        assert_eq!(hub.client_count(), ws::MAX_CLIENTS);
        assert!(logged(&lines, "with 503"));
        drop(clients);
    }

    #[test]
    fn stalled_client_is_dropped_at_the_backlog_bound_without_stalling_others() {
        let (_, hub, addr, lines) = server(true);
        let hub = hub.unwrap();
        let stalled = ws_connect(addr);
        let mut healthy = ws_connect(addr);
        wait_clients(&hub, 2);

        const N: usize = 4000;
        let reader = std::thread::spawn(move || {
            let mut n = 0;
            loop {
                let (_, body) = read_frame(&mut healthy);
                if body.ends_with(br#""line":"done"}"#) {
                    return (n, healthy);
                }
                n += 1;
            }
        });

        let line = vec![0xE9u8; 2048];
        let started = Instant::now();
        for _ in 0..N {
            hub.broadcast("log", &line);
        }
        hub.broadcast("log", b"done");
        // About 49 MB of frames against a peer that reads nothing: a broadcast that waited on
        // that socket would never have returned.
        assert!(started.elapsed() < Duration::from_secs(10));

        let (received, healthy) = reader.join().unwrap();
        assert_eq!(received, N, "the healthy client received every line");
        wait_clients(&hub, 1);
        assert!(logged(&lines, "broadcast backlog full"));
        drop((stalled, healthy));
    }

    #[test]
    fn unmasked_ws_frame_closes_the_connection() {
        let (_, hub, addr, _) = server(true);
        let hub = hub.unwrap();
        let mut c = ws_connect(addr);
        wait_clients(&hub, 1);
        c.write_all(b"\x81\x01a").unwrap();
        let mut b = [0u8; 1];
        assert_eq!(c.read(&mut b).unwrap(), 0);
        wait_clients(&hub, 0);
    }

    #[test]
    fn get_is_raw_tcp_when_ws_is_disabled() {
        let (q, _, addr, _) = server(false);
        let mut c = connect(addr);
        c.write_all(b"GET = 1\n<<<RUN>>>\n").unwrap();
        assert_eq!(read_exact_n(&mut c, 9), b"[queued]\n");
        wait_pending(&q, 1);
        assert_eq!(q.pop().unwrap().code, b"GET = 1\n");
    }
}
