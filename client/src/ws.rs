//! The WebSocket transport, sharing the REPL port.
//!
//! Wire contract (JSON text frames, one message per request):
//!
//! ```text
//!   client -> bridge   {"id":"q17abc","code":"<lua source>"}
//!   bridge -> client   {"type":"ack","id":"q17abc","status":"queued"}
//!   bridge -> client   {"type":"ack","id":"q17abc","status":"rejected","error":"<why>"}
//!   bridge -> client   {"type":"log","line":"<a Loader.Printf line>"}
//!   bridge -> client   {"type":"ws","line":"<a Loader.WsSend line>"}
//! ```
//!
//! Broadcast lines are game-side CP1252 text: bytes >= 0x80 go out as `\uXXXX` of their Unicode code
//! point, so those frames are pure ASCII. The ack's `id` and `error` are UTF-8 text and are escaped
//! as such.
//!
//! Every client has a bounded outbox drained by its own sender thread, so a broadcaster (the game
//! thread) never blocks on a socket or on a lock a socket write holds, and one slow client never
//! delays another.

use std::collections::VecDeque;
use std::io::{self, Read, Write};
use std::net::{Shutdown, TcpStream};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};

use base64::Engine as _;
use sha1::{Digest, Sha1};

use crate::queue::{Normalize, Origin, Queue, lock};

/// RFC 6455 section 1.3 GUID, appended to the client's key to derive `Sec-WebSocket-Accept`.
pub const WS_GUID: &str = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11";
/// Concurrent WebSocket clients served; one more is answered 503 instead of 101.
pub const MAX_CLIENTS: usize = 16;
/// Bytes of encoded frames a client may have waiting. A broadcast that would pass this disconnects
/// that client.
pub const BACKLOG_BYTES: usize = 4 * 1024 * 1024;
/// Receive buffer for one assembled message. The largest accepted payload is one byte less.
pub const MESSAGE_BUFFER: usize = 1024 * 1024;
/// Request-header buffer for the handshake. The largest accepted request is one byte less.
pub const HANDSHAKE_BUFFER: usize = 8192;

pub const OP_CONT: u8 = 0x0;
pub const OP_TEXT: u8 = 0x1;
pub const OP_BINARY: u8 = 0x2;
pub const OP_CLOSE: u8 = 0x8;
pub const OP_PING: u8 = 0x9;
pub const OP_PONG: u8 = 0xA;

/// String-capacity limits of the request parser. A decoded string must be shorter than its cap.
const KEY_CAP: usize = 32;
const ID_CAP: usize = 128;
const UNKNOWN_VALUE_CAP: usize = 256;
const CODE_CAP: usize = MESSAGE_BUFFER;

/// Everything the WebSocket side can fail with.
#[derive(Debug)]
pub enum WsError {
    /// The handshake request was not a usable WebSocket upgrade.
    Handshake(String),
    /// A frame broke the protocol rules; the connection is closed.
    Protocol(String),
    /// The peer closed the connection (EOF or a close frame).
    Closed,
    Io(io::Error),
}

impl std::fmt::Display for WsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WsError::Handshake(m) => write!(f, "websocket handshake rejected: {m}"),
            WsError::Protocol(m) => write!(f, "websocket protocol error: {m}"),
            WsError::Closed => write!(f, "websocket peer closed the connection"),
            WsError::Io(e) => write!(f, "websocket I/O error: {e}"),
        }
    }
}

impl std::error::Error for WsError {}

impl From<io::Error> for WsError {
    fn from(e: io::Error) -> Self {
        if e.kind() == io::ErrorKind::UnexpectedEof {
            WsError::Closed
        } else {
            WsError::Io(e)
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Handshake
// ---------------------------------------------------------------------------------------------

/// `base64(sha1(key + WS_GUID))`.
pub fn accept_key(key: &[u8]) -> String {
    let mut h = Sha1::new();
    h.update(key);
    h.update(WS_GUID.as_bytes());
    base64::engine::general_purpose::STANDARD.encode(h.finalize())
}

/// The exact 101 response for a given accept value.
pub fn handshake_response(accept: &str) -> String {
    format!(
        "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\
         Sec-WebSocket-Accept: {accept}\r\n\r\n"
    )
}

/// The exact refusal sent when [`MAX_CLIENTS`] are already connected.
pub fn busy_response() -> String {
    let body = format!("lua_bridge: {MAX_CLIENTS} WebSocket clients already connected\n");
    format!(
        "HTTP/1.1 503 Service Unavailable\r\nContent-Type: text/plain\r\nContent-Length: {}\r\n\
         Connection: close\r\n\r\n{body}",
        body.len()
    )
}

/// Read an HTTP upgrade request up to its blank line and return its `Sec-WebSocket-Key`. A read
/// timeout on the stream surfaces as a handshake error.
pub fn read_handshake_key<R: Read>(stream: &mut R) -> Result<Vec<u8>, WsError> {
    let mut req: Vec<u8> = Vec::with_capacity(HANDSHAKE_BUFFER);
    let mut buf = [0u8; HANDSHAKE_BUFFER];
    while find(&req, b"\r\n\r\n").is_none() {
        let room = HANDSHAKE_BUFFER - 1 - req.len();
        if room == 0 {
            return Err(WsError::Handshake(format!(
                "request headers exceed {} bytes without a terminating blank line",
                HANDSHAKE_BUFFER - 1
            )));
        }
        let n = match stream.read(&mut buf[..room]) {
            Ok(n) => n,
            Err(e) if matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut) => {
                return Err(WsError::Handshake(format!(
                    "request headers incomplete when the read timed out ({} bytes received)",
                    req.len()
                )));
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(WsError::Io(e)),
        };
        if n == 0 {
            return Err(WsError::Handshake(
                "connection closed before the request headers ended".into(),
            ));
        }
        req.extend_from_slice(&buf[..n]);
    }
    find_key(&req).map(<[u8]>::to_vec)
}

/// The `Sec-WebSocket-Key` value: the first line starting (case-insensitively) with the header
/// name, leading spaces/tabs skipped, up to the line end.
fn find_key(req: &[u8]) -> Result<&[u8], WsError> {
    const NEEDLE: &[u8] = b"Sec-WebSocket-Key:";
    let mut start = 0;
    while start < req.len() {
        let line_end = req[start..]
            .iter()
            .position(|&b| b == b'\n')
            .map_or(req.len(), |i| start + i);
        let line = &req[start..line_end];
        if line.len() >= NEEDLE.len() && line[..NEEDLE.len()].eq_ignore_ascii_case(NEEDLE) {
            let mut v = &line[NEEDLE.len()..];
            while let [b' ' | b'\t', rest @ ..] = v {
                v = rest;
            }
            let end = v.iter().position(|&b| b == b'\r').unwrap_or(v.len());
            let key = &v[..end];
            if key.is_empty() || key.len() >= 128 {
                return Err(WsError::Handshake(format!(
                    "Sec-WebSocket-Key is {} bytes (must be 1..=127)",
                    key.len()
                )));
            }
            return Ok(key);
        }
        start = line_end + 1;
    }
    Err(WsError::Handshake("no Sec-WebSocket-Key header".into()))
}

// ---------------------------------------------------------------------------------------------
// Frames
// ---------------------------------------------------------------------------------------------

/// One unmasked, final server frame.
pub fn encode_frame(opcode: u8, payload: &[u8]) -> Vec<u8> {
    let len = payload.len();
    let mut f = Vec::with_capacity(len + 10);
    f.push(0x80 | (opcode & 0x0F));
    if len < 126 {
        f.push(len as u8);
    } else if len < 65536 {
        f.push(126);
        f.extend_from_slice(&(len as u16).to_be_bytes());
    } else {
        f.push(127);
        f.extend_from_slice(&(len as u64).to_be_bytes());
    }
    f.extend_from_slice(payload);
    f
}

/// Receive one complete data message, assembling continuation frames. Ping is answered with a
/// pong and close is echoed (both through `control`) while assembly continues. Client frames must
/// be masked; data must start with text or binary and continue with continuation frames; control
/// payloads are at most 125 bytes; the assembled message must be smaller than [`MESSAGE_BUFFER`].
pub fn recv_message<R: Read>(
    r: &mut R,
    mut control: impl FnMut(u8, &[u8]) -> io::Result<()>,
) -> Result<Vec<u8>, WsError> {
    let mut msg: Vec<u8> = Vec::new();
    let mut first_opcode: Option<u8> = None;
    loop {
        let mut hdr = [0u8; 2];
        r.read_exact(&mut hdr)?;
        let fin = hdr[0] & 0x80 != 0;
        let opcode = hdr[0] & 0x0F;
        let masked = hdr[1] & 0x80 != 0;
        let mut plen = u64::from(hdr[1] & 0x7F);
        if plen == 126 {
            let mut ext = [0u8; 2];
            r.read_exact(&mut ext)?;
            plen = u64::from(u16::from_be_bytes(ext));
        } else if plen == 127 {
            let mut ext = [0u8; 8];
            r.read_exact(&mut ext)?;
            plen = u64::from_be_bytes(ext);
            if plen > MESSAGE_BUFFER as u64 {
                return Err(WsError::Protocol(format!(
                    "frame payload of {plen} bytes exceeds the {MESSAGE_BUFFER}-byte message buffer"
                )));
            }
        }
        if !masked {
            return Err(WsError::Protocol("client frame is not masked".into()));
        }
        let mut mask = [0u8; 4];
        r.read_exact(&mut mask)?;
        let plen = plen as usize;

        if opcode >= 0x8 {
            if plen > 125 {
                return Err(WsError::Protocol(format!(
                    "control frame payload of {plen} bytes exceeds 125"
                )));
            }
            let mut cpl = vec![0u8; plen];
            r.read_exact(&mut cpl)?;
            unmask(&mut cpl, mask);
            match opcode {
                OP_PING => {
                    control(OP_PONG, &cpl)?;
                    continue;
                }
                OP_CLOSE => {
                    control(OP_CLOSE, &cpl)?;
                    return Err(WsError::Closed);
                }
                _ => continue,
            }
        }

        match first_opcode {
            None => {
                if opcode != OP_TEXT && opcode != OP_BINARY {
                    return Err(WsError::Protocol(format!(
                        "data message starts with opcode {opcode:#x} (expected text or binary)"
                    )));
                }
                first_opcode = Some(opcode);
            }
            Some(_) => {
                if opcode != OP_CONT {
                    return Err(WsError::Protocol(format!(
                        "fragment carries opcode {opcode:#x} (expected continuation)"
                    )));
                }
            }
        }

        if plen > 0 {
            if msg.len() + plen >= MESSAGE_BUFFER {
                return Err(WsError::Protocol(format!(
                    "message of {} bytes reaches the {MESSAGE_BUFFER}-byte message buffer",
                    msg.len() + plen
                )));
            }
            let at = msg.len();
            msg.resize(at + plen, 0);
            r.read_exact(&mut msg[at..])?;
            unmask(&mut msg[at..], mask);
        }
        if fin {
            return Ok(msg);
        }
    }
}

fn unmask(data: &mut [u8], mask: [u8; 4]) {
    for (i, b) in data.iter_mut().enumerate() {
        *b ^= mask[i & 3];
    }
}

// ---------------------------------------------------------------------------------------------
// JSON
// ---------------------------------------------------------------------------------------------

/// A parsed `{id, code}` request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Request {
    /// The client's request id, decoded as JSON text.
    pub id: String,
    /// The Lua source bytes.
    pub code: Vec<u8>,
}

fn skip_ws(b: &[u8], p: &mut usize) {
    while *p < b.len() && matches!(b[*p], b' ' | b'\t' | b'\r' | b'\n') {
        *p += 1;
    }
}

/// Decode the JSON string at `b[*p]` (which must be `"`). The decoded string must be shorter than
/// `cap`. `\uXXXX` above 0xFF decodes to `?`.
fn parse_string(b: &[u8], p: &mut usize, cap: usize) -> Result<Vec<u8>, String> {
    if *p >= b.len() || b[*p] != b'"' {
        return Err("expected a string".into());
    }
    *p += 1;
    let mut out = Vec::new();
    while *p < b.len() {
        let ch = b[*p];
        if ch == b'"' {
            *p += 1;
            return Ok(out);
        }
        if out.len() + 1 >= cap {
            return Err(format!("string longer than {} bytes", cap - 1));
        }
        *p += 1;
        if ch != b'\\' {
            out.push(ch);
            continue;
        }
        let Some(&esc) = b.get(*p) else {
            return Err("unterminated escape".into());
        };
        *p += 1;
        out.push(match esc {
            b'"' => b'"',
            b'\\' => b'\\',
            b'/' => b'/',
            b'n' => b'\n',
            b'r' => b'\r',
            b't' => b'\t',
            b'b' => 0x08,
            b'f' => 0x0C,
            b'u' => {
                let hex = b.get(*p..*p + 4).ok_or("truncated \\u escape")?;
                let mut v: u32 = 0;
                for &h in hex {
                    let d = (h as char)
                        .to_digit(16)
                        .ok_or_else(|| format!("bad hex digit {:?} in \\u escape", h as char))?;
                    v = (v << 4) | d;
                }
                *p += 4;
                if v <= 0xFF { v as u8 } else { b'?' }
            }
            other => return Err(format!("unknown escape \\{}", other as char)),
        });
    }
    Err("unterminated string".into())
}

fn hex4(b: &[u8], p: &mut usize) -> Result<u32, String> {
    let hex = b.get(*p..*p + 4).ok_or("truncated \\u escape")?;
    let mut v: u32 = 0;
    for &h in hex {
        let d = (h as char)
            .to_digit(16)
            .ok_or_else(|| format!("bad hex digit {:?} in \\u escape", h as char))?;
        v = (v << 4) | d;
    }
    *p += 4;
    Ok(v)
}

/// Decode the JSON string at `b[*p]` (which must be `"`) as Unicode text: raw bytes must be UTF-8,
/// `\uXXXX` escapes (with surrogate pairs) decode to their code points. The UTF-8 result must be
/// shorter than `cap` bytes.
fn parse_string_utf8(b: &[u8], p: &mut usize, cap: usize) -> Result<String, String> {
    if *p >= b.len() || b[*p] != b'"' {
        return Err("expected a string".into());
    }
    *p += 1;
    let mut out: Vec<u8> = Vec::new();
    loop {
        let Some(&ch) = b.get(*p) else {
            return Err("unterminated string".into());
        };
        *p += 1;
        match ch {
            b'"' => break,
            b'\\' => {
                let Some(&esc) = b.get(*p) else {
                    return Err("unterminated escape".into());
                };
                *p += 1;
                let c = match esc {
                    b'"' => '"',
                    b'\\' => '\\',
                    b'/' => '/',
                    b'n' => '\n',
                    b'r' => '\r',
                    b't' => '\t',
                    b'b' => '\u{8}',
                    b'f' => '\u{c}',
                    b'u' => {
                        let v = hex4(b, p)?;
                        let cp = if (0xD800..0xDC00).contains(&v) {
                            if b.get(*p..*p + 2) != Some(b"\\u") {
                                return Err("high surrogate without a low surrogate".into());
                            }
                            *p += 2;
                            let lo = hex4(b, p)?;
                            if !(0xDC00..0xE000).contains(&lo) {
                                return Err("high surrogate without a low surrogate".into());
                            }
                            0x10000 + ((v - 0xD800) << 10) + (lo - 0xDC00)
                        } else {
                            v
                        };
                        char::from_u32(cp).ok_or("lone low surrogate in \\u escape")?
                    }
                    other => return Err(format!("unknown escape \\{}", other as char)),
                };
                let mut tmp = [0u8; 4];
                out.extend_from_slice(c.encode_utf8(&mut tmp).as_bytes());
            }
            _ => out.push(ch),
        }
        if out.len() >= cap {
            return Err(format!("string longer than {} bytes", cap - 1));
        }
    }
    String::from_utf8(out).map_err(|_| "string is not valid UTF-8".into())
}

/// Parse `{"id":"...","code":"..."}`. Other keys are skipped (string values, or anything up to the
/// next `,` / `}`). Both fields are required; neither may contain a NUL byte.
pub fn parse_request(body: &[u8]) -> Result<Request, String> {
    let mut p = 0;
    let mut id = None;
    let mut code = None;
    skip_ws(body, &mut p);
    if body.get(p) != Some(&b'{') {
        return Err("not a JSON object".into());
    }
    p += 1;
    loop {
        skip_ws(body, &mut p);
        match body.get(p) {
            None => return Err("unterminated object".into()),
            Some(b'}') => break,
            Some(_) => {}
        }
        let key = parse_string(body, &mut p, KEY_CAP).map_err(|e| format!("key: {e}"))?;
        skip_ws(body, &mut p);
        if body.get(p) != Some(&b':') {
            return Err("expected ':' after a key".into());
        }
        p += 1;
        skip_ws(body, &mut p);
        match key.as_slice() {
            b"id" => {
                id = Some(parse_string_utf8(body, &mut p, ID_CAP).map_err(|e| format!("id: {e}"))?)
            }
            b"code" => {
                code = Some(parse_string(body, &mut p, CODE_CAP).map_err(|e| format!("code: {e}"))?)
            }
            _ => {
                if body.get(p) == Some(&b'"') {
                    parse_string(body, &mut p, UNKNOWN_VALUE_CAP)
                        .map_err(|e| format!("value of an unknown key: {e}"))?;
                } else {
                    while p < body.len() && body[p] != b',' && body[p] != b'}' {
                        p += 1;
                    }
                }
            }
        }
        skip_ws(body, &mut p);
        match body.get(p) {
            Some(b',') => p += 1,
            Some(b'}') => break,
            None => return Err("unterminated object".into()),
            Some(&c) => return Err(format!("unexpected {:?} after a value", c as char)),
        }
    }
    let id = id.ok_or("missing \"id\"")?;
    let code = code.ok_or("missing \"code\"")?;
    if id.contains('\0') || code.contains(&0) {
        return Err("id or code contains a NUL byte".into());
    }
    Ok(Request { id, code })
}

/// CP1252 0x80..=0x9F to Unicode. 0xA0..=0xFF map to the same code point.
const CP1252_HIGH: [u16; 32] = [
    0x20AC, 0x0081, 0x201A, 0x0192, 0x201E, 0x2026, 0x2020, 0x2021, 0x02C6, 0x2030, 0x0160, 0x2039,
    0x0152, 0x008D, 0x017D, 0x008F, 0x0090, 0x2018, 0x2019, 0x201C, 0x201D, 0x2022, 0x2013, 0x2014,
    0x02DC, 0x2122, 0x0161, 0x203A, 0x0153, 0x009D, 0x017E, 0x0178,
];

/// Append `src` (CP1252 bytes) to `out` as the body of a JSON string.
pub fn escape_json(src: &[u8], out: &mut Vec<u8>) {
    for &ch in src {
        match ch {
            b'\\' | b'"' => out.extend_from_slice(&[b'\\', ch]),
            b'\n' => out.extend_from_slice(b"\\n"),
            b'\r' => out.extend_from_slice(b"\\r"),
            b'\t' => out.extend_from_slice(b"\\t"),
            0x08 => out.extend_from_slice(b"\\b"),
            0x0C => out.extend_from_slice(b"\\f"),
            0x00..=0x1F => out.extend_from_slice(format!("\\u{:04x}", ch).as_bytes()),
            0x80..=0xFF => {
                let cp = if ch < 0xA0 {
                    CP1252_HIGH[usize::from(ch - 0x80)]
                } else {
                    u16::from(ch)
                };
                out.extend_from_slice(format!("\\u{cp:04x}").as_bytes());
            }
            _ => out.push(ch),
        }
    }
}

/// `{"type":"<kind>","line":"<line>"}`.
pub fn typed_line(kind: &str, line: &[u8]) -> Vec<u8> {
    let mut m = Vec::with_capacity(line.len() + 32);
    m.extend_from_slice(b"{\"type\":\"");
    escape_json(kind.as_bytes(), &mut m);
    m.extend_from_slice(b"\",\"line\":\"");
    escape_json(line, &mut m);
    m.extend_from_slice(b"\"}");
    m
}

/// Append UTF-8 text to `out` as the body of a JSON string: quote, backslash and control
/// characters are escaped, everything else is copied as UTF-8.
pub fn escape_json_utf8(src: &str, out: &mut Vec<u8>) {
    for c in src.chars() {
        match c {
            '"' | '\\' => out.extend_from_slice(&[b'\\', c as u8]),
            '\n' => out.extend_from_slice(b"\\n"),
            '\r' => out.extend_from_slice(b"\\r"),
            '\t' => out.extend_from_slice(b"\\t"),
            '\u{8}' => out.extend_from_slice(b"\\b"),
            '\u{c}' => out.extend_from_slice(b"\\f"),
            c if (c as u32) < 0x20 => {
                out.extend_from_slice(format!("\\u{:04x}", c as u32).as_bytes())
            }
            c => {
                let mut tmp = [0u8; 4];
                out.extend_from_slice(c.encode_utf8(&mut tmp).as_bytes());
            }
        }
    }
}

/// `{"type":"ack","id":"<id>","status":"queued"}`.
pub fn ack_queued(id: &str) -> Vec<u8> {
    let mut m = Vec::with_capacity(id.len() + 48);
    m.extend_from_slice(b"{\"type\":\"ack\",\"id\":\"");
    escape_json_utf8(id, &mut m);
    m.extend_from_slice(b"\",\"status\":\"queued\"}");
    m
}

/// `{"type":"ack","id":"<id>","status":"rejected","error":"<error>"}`.
pub fn ack_rejected(id: &str, error: &str) -> Vec<u8> {
    let mut m = Vec::with_capacity(id.len() + error.len() + 64);
    m.extend_from_slice(b"{\"type\":\"ack\",\"id\":\"");
    escape_json_utf8(id, &mut m);
    m.extend_from_slice(b"\",\"status\":\"rejected\",\"error\":\"");
    escape_json_utf8(error, &mut m);
    m.extend_from_slice(b"\"}");
    m
}

// ---------------------------------------------------------------------------------------------
// Client hub
// ---------------------------------------------------------------------------------------------

/// Frames waiting to be sent to one client.
struct Outbox {
    frames: VecDeque<Arc<Vec<u8>>>,
    bytes: usize,
    closed: bool,
}

/// One connected client. Broadcasts land in its outbox; its own sender thread drains the outbox to
/// the socket, so nobody but that thread ever blocks on a slow peer.
struct ClientConn {
    id: u64,
    peer: String,
    outbox: Mutex<Outbox>,
    ready: Condvar,
    /// Serialises whole-frame writes (sender thread, acks, pongs, close echo).
    writer: Mutex<TcpStream>,
    /// A handle used only to shut the socket down, never to write.
    socket: TcpStream,
}

impl ClientConn {
    /// Queue a frame. Fails with the bytes already queued when the frame would push the backlog
    /// over [`BACKLOG_BYTES`].
    fn enqueue(&self, frame: Arc<Vec<u8>>) -> Result<(), usize> {
        let mut ob = lock(&self.outbox, "websocket outbox");
        if ob.closed {
            return Ok(());
        }
        if ob.bytes + frame.len() > BACKLOG_BYTES {
            return Err(ob.bytes);
        }
        ob.bytes += frame.len();
        ob.frames.push_back(frame);
        self.ready.notify_one();
        Ok(())
    }

    /// Mark closed, wake the sender thread, shut the socket. Returns true for the call that closed
    /// it, so each disconnect is logged exactly once.
    fn close(&self) -> bool {
        let mut ob = lock(&self.outbox, "websocket outbox");
        if ob.closed {
            return false;
        }
        ob.closed = true;
        ob.frames.clear();
        ob.bytes = 0;
        drop(ob);
        self.ready.notify_all();
        // A shutdown error means the socket is already gone, which is the goal.
        let _ = self.socket.shutdown(Shutdown::Both);
        true
    }
}

struct HubShared {
    clients: Mutex<Vec<Arc<ClientConn>>>,
    next_id: AtomicU64,
    queue: Queue,
    normalize: Normalize,
}

/// The set of connected WebSocket clients. Cloning shares the hub.
#[derive(Clone)]
pub struct WsHub {
    shared: Arc<HubShared>,
}

impl WsHub {
    pub fn new(queue: Queue, normalize: Normalize) -> WsHub {
        WsHub {
            shared: Arc::new(HubShared {
                clients: Mutex::new(Vec::new()),
                next_id: AtomicU64::new(1),
                queue,
                normalize,
            }),
        }
    }

    /// Queue `{"type":"<kind>","line":"<line>"}` for every connected client. Never touches a
    /// socket: each client's sender thread does the sending. A client whose backlog would exceed
    /// [`BACKLOG_BYTES`] is disconnected and logged.
    pub fn broadcast(&self, kind: &str, line: &[u8]) {
        broadcast(&self.shared, kind, line);
    }

    /// Connected clients right now.
    pub fn client_count(&self) -> usize {
        lock(&self.shared.clients, "websocket client list").len()
    }

    /// Serve a connection whose first bytes are `GET `, on the calling (per-connection) thread.
    /// The stream's read timeout bounds the handshake read; it is cleared once the client is
    /// registered.
    pub(crate) fn serve(&self, stream: TcpStream) {
        serve_connection(&self.shared, stream);
    }
}

fn broadcast(shared: &HubShared, kind: &str, line: &[u8]) {
    let frame = Arc::new(encode_frame(OP_TEXT, &typed_line(kind, line)));
    let clients: Vec<Arc<ClientConn>> = lock(&shared.clients, "websocket client list").clone();
    for c in clients {
        if let Err(queued) = c.enqueue(Arc::clone(&frame)) {
            disconnect(
                shared,
                &c,
                &format!(
                    "broadcast backlog full ({queued} bytes queued, a {}-byte frame would pass \
                     the {BACKLOG_BYTES}-byte limit)",
                    frame.len()
                ),
            );
        }
    }
}

fn disconnect(shared: &HubShared, c: &ClientConn, why: &str) {
    if !c.close() {
        return;
    }
    let count = {
        let mut clients = lock(&shared.clients, "websocket client list");
        clients.retain(|x| x.id != c.id);
        clients.len()
    };
    shared.queue.log(&format!(
        "[*] lua_bridge: ws: client {} ({}) disconnected: {why} ({count}/{MAX_CLIENTS})",
        c.id, c.peer
    ));
}

fn serve_connection(shared: &Arc<HubShared>, mut stream: TcpStream) {
    let log = |m: &str| shared.queue.log(m);
    let peer = stream
        .peer_addr()
        .map_or_else(|e| format!("unknown peer: {e}"), |a| a.to_string());
    let key = match read_handshake_key(&mut stream) {
        Ok(k) => k,
        Err(e) => {
            log(&format!("[!] lua_bridge: ws: handshake from {peer} failed: {e}"));
            return;
        }
    };
    let (writer, socket) = match (stream.try_clone(), stream.try_clone()) {
        (Ok(w), Ok(s)) => (w, s),
        (Err(e), _) | (_, Err(e)) => {
            log(&format!("[!] lua_bridge: ws: could not clone the socket of {peer}: {e}"));
            return;
        }
    };
    let conn = Arc::new(ClientConn {
        id: shared.next_id.fetch_add(1, Ordering::Relaxed),
        peer,
        outbox: Mutex::new(Outbox {
            frames: VecDeque::new(),
            bytes: 0,
            closed: false,
        }),
        ready: Condvar::new(),
        writer: Mutex::new(writer),
        socket,
    });

    let count = {
        let mut clients = lock(&shared.clients, "websocket client list");
        if clients.len() >= MAX_CLIENTS {
            drop(clients);
            let sent = write_raw(&conn, busy_response().as_bytes());
            let _ = conn.socket.shutdown(Shutdown::Both);
            log(&format!(
                "[!] lua_bridge: ws: refused {} with 503 - {MAX_CLIENTS} clients already \
                 connected{}",
                conn.peer,
                match sent {
                    Ok(()) => String::new(),
                    Err(e) => format!(" (the 503 could not be sent: {e})"),
                }
            ));
            return;
        }
        clients.push(Arc::clone(&conn));
        clients.len()
    };

    if let Err(e) = write_raw(&conn, handshake_response(&accept_key(&key)).as_bytes()) {
        disconnect(shared, &conn, &format!("the 101 response could not be sent: {e}"));
        return;
    }
    if let Err(e) = stream.set_read_timeout(None) {
        disconnect(shared, &conn, &format!("could not clear the read timeout: {e}"));
        return;
    }
    log(&format!(
        "[*] lua_bridge: ws: client {} ({}) connected ({count}/{MAX_CLIENTS})",
        conn.id, conn.peer
    ));

    let (sender_shared, sender_conn) = (Arc::clone(shared), Arc::clone(&conn));
    if let Err(e) = std::thread::Builder::new()
        .name("lua_bridge ws sender".into())
        .spawn(move || sender(&sender_shared, &sender_conn))
    {
        disconnect(shared, &conn, &format!("could not start its sender thread: {e}"));
        return;
    }

    let why = match serve_client(shared, stream, &conn) {
        WsError::Closed => "closed by the client".to_string(),
        other => other.to_string(),
    };
    disconnect(shared, &conn, &why);
}

fn write_raw(conn: &ClientConn, bytes: &[u8]) -> io::Result<()> {
    let mut w = lock(&conn.writer, "websocket writer");
    w.write_all(bytes)?;
    w.flush()
}

/// Drain one client's outbox to its socket until the client is closed.
fn sender(shared: &HubShared, conn: &ClientConn) {
    loop {
        let frame = {
            let mut ob = lock(&conn.outbox, "websocket outbox");
            loop {
                if ob.closed {
                    return;
                }
                if let Some(f) = ob.frames.pop_front() {
                    ob.bytes -= f.len();
                    break f;
                }
                ob = match conn.ready.wait(ob) {
                    Ok(g) => g,
                    Err(_) => panic!("lua_bridge: the websocket outbox mutex is poisoned"),
                };
            }
        };
        if let Err(e) = write_raw(conn, &frame) {
            disconnect(shared, conn, &format!("broadcast send failed: {e}"));
            return;
        }
    }
}

/// Serve one client's requests until it disconnects or breaks the protocol; returns why it ended.
fn serve_client(shared: &HubShared, mut reader: TcpStream, conn: &ClientConn) -> WsError {
    loop {
        let msg = match recv_message(&mut reader, |op, payload| {
            write_raw(conn, &encode_frame(op, payload))
        }) {
            Ok(m) => m,
            Err(e) => return e,
        };
        let req = match parse_request(&msg) {
            Ok(r) => r,
            Err(why) => {
                shared.queue.log(&format!(
                    "[!] lua_bridge: ws: rejected malformed request ({} bytes) from client {}: \
                     {why}",
                    msg.len(),
                    conn.id
                ));
                continue;
            }
        };
        let reply = match dispatch(&shared.queue, &shared.normalize, &req) {
            Ok(_) => ack_queued(&req.id),
            Err(why) => {
                broadcast(
                    shared,
                    "log",
                    format!("[!] lua_bridge: ws: chunk id={} rejected: {why}", req.id).as_bytes(),
                );
                ack_rejected(&req.id, &why)
            }
        };
        if let Err(e) = write_raw(conn, &encode_frame(OP_TEXT, &reply)) {
            return WsError::Io(e);
        }
    }
}

/// Normalise a parsed request's code and queue it as a WebSocket chunk. On a normalisation error
/// nothing is queued and the error is returned.
pub fn dispatch(queue: &Queue, normalize: &Normalize, req: &Request) -> Result<usize, String> {
    let label = format!("ws chunk id={}", req.id);
    let code = match normalize(&label, &req.code) {
        Ok(c) => c,
        Err(why) => {
            queue.log(&format!(
                "[!] lua_bridge: ws: rejected chunk (id={}, {} bytes): {why}",
                req.id,
                req.code.len()
            ));
            return Err(why);
        }
    };
    let len = code.len();
    let pending = queue.push(code, Origin::Ws);
    queue.log(&format!("[+] lua_bridge: ws: queued chunk (id={}, {len} bytes)", req.id));
    Ok(pending)
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::queue::tests::recording_log;
    use std::io::Cursor;

    fn masked_frame(fin: bool, opcode: u8, payload: &[u8]) -> Vec<u8> {
        let mask = [0x37, 0xFA, 0x21, 0x3D];
        let mut f = vec![if fin { 0x80 } else { 0 } | opcode];
        let len = payload.len();
        if len < 126 {
            f.push(0x80 | len as u8);
        } else if len < 65536 {
            f.push(0x80 | 126);
            f.extend_from_slice(&(len as u16).to_be_bytes());
        } else {
            f.push(0x80 | 127);
            f.extend_from_slice(&(len as u64).to_be_bytes());
        }
        f.extend_from_slice(&mask);
        f.extend(payload.iter().enumerate().map(|(i, b)| b ^ mask[i & 3]));
        f
    }

    #[test]
    fn accept_key_matches_rfc6455_example() {
        assert_eq!(
            accept_key(b"dGhlIHNhbXBsZSBub25jZQ=="),
            "s3pPLMBiTxaQ9kYGzzhZRbK+xOo="
        );
    }

    #[test]
    fn handshake_key_is_found_case_insensitively_after_whitespace() {
        let req =
            b"GET / HTTP/1.1\r\nHost: x\r\nsec-websocket-key: \t dGhlIHNhbXBsZSBub25jZQ==\r\n\r\n";
        let key = read_handshake_key(&mut Cursor::new(req.to_vec())).unwrap();
        assert_eq!(key, b"dGhlIHNhbXBsZSBub25jZQ==");
        assert_eq!(
            handshake_response(&accept_key(&key)).as_bytes(),
            b"HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\
              Sec-WebSocket-Accept: s3pPLMBiTxaQ9kYGzzhZRbK+xOo=\r\n\r\n"
        );
    }

    #[test]
    fn handshake_without_key_or_with_oversize_headers_fails() {
        let r = read_handshake_key(&mut Cursor::new(b"GET / HTTP/1.1\r\nHost: x\r\n\r\n".to_vec()));
        assert!(matches!(r, Err(WsError::Handshake(_))));
        let mut big = b"GET / HTTP/1.1\r\nX: ".to_vec();
        big.resize(HANDSHAKE_BUFFER + 10, b'a');
        assert!(matches!(read_handshake_key(&mut Cursor::new(big)), Err(WsError::Handshake(_))));
        let r = read_handshake_key(&mut Cursor::new(b"GET / HTTP/1.1\r\n".to_vec()));
        assert!(matches!(r, Err(WsError::Handshake(_))));
    }

    #[test]
    fn busy_response_is_a_complete_503() {
        let r = busy_response();
        let (head, body) = r.split_once("\r\n\r\n").unwrap();
        assert!(head.starts_with("HTTP/1.1 503 Service Unavailable\r\n"));
        assert!(head.contains(&format!("Content-Length: {}", body.len())));
        assert_eq!(body, "lua_bridge: 16 WebSocket clients already connected\n");
    }

    #[test]
    fn frame_length_encodings() {
        assert_eq!(encode_frame(OP_TEXT, b"hi"), b"\x81\x02hi");
        let f = encode_frame(OP_TEXT, &[0u8; 126]);
        assert_eq!(&f[..4], &[0x81, 126, 0, 126]);
        let f = encode_frame(OP_TEXT, &vec![0u8; 65536]);
        assert_eq!(&f[..10], &[0x81, 127, 0, 0, 0, 0, 0, 1, 0, 0]);
        assert_eq!(encode_frame(OP_PONG, b""), b"\x8a\x00");
    }

    #[test]
    fn recv_assembles_fragments_and_answers_ping_mid_message() {
        let mut wire = masked_frame(false, OP_TEXT, b"hel");
        wire.extend(masked_frame(true, OP_PING, b"p!"));
        wire.extend(masked_frame(true, OP_CONT, b"lo"));
        let mut ctl = Vec::new();
        let m = recv_message(&mut Cursor::new(wire), |op, p| {
            ctl.push((op, p.to_vec()));
            Ok(())
        })
        .unwrap();
        assert_eq!(m, b"hello");
        assert_eq!(ctl, vec![(OP_PONG, b"p!".to_vec())]);
    }

    #[test]
    fn recv_echoes_close_and_reports_closed() {
        let wire = masked_frame(true, OP_CLOSE, b"\x03\xe8");
        let mut ctl = Vec::new();
        let r = recv_message(&mut Cursor::new(wire), |op, p| {
            ctl.push((op, p.to_vec()));
            Ok(())
        });
        assert!(matches!(r, Err(WsError::Closed)));
        assert_eq!(ctl, vec![(OP_CLOSE, b"\x03\xe8".to_vec())]);
    }

    #[test]
    fn recv_protocol_violations() {
        let none = |_: u8, _: &[u8]| Ok(());
        // Unmasked.
        let r = recv_message(&mut Cursor::new(b"\x81\x01a".to_vec()), none);
        assert!(matches!(r, Err(WsError::Protocol(_))));
        // Starts with a continuation.
        let r = recv_message(&mut Cursor::new(masked_frame(true, OP_CONT, b"a")), none);
        assert!(matches!(r, Err(WsError::Protocol(_))));
        // Text then a new text instead of a continuation.
        let mut w = masked_frame(false, OP_TEXT, b"a");
        w.extend(masked_frame(true, OP_TEXT, b"b"));
        assert!(matches!(recv_message(&mut Cursor::new(w), none), Err(WsError::Protocol(_))));
        // Declared 64-bit length over the buffer.
        let mut w = vec![0x81, 0x80 | 127];
        w.extend_from_slice(&((MESSAGE_BUFFER as u64) + 1).to_be_bytes());
        assert!(matches!(recv_message(&mut Cursor::new(w), none), Err(WsError::Protocol(_))));
        // A message exactly the buffer size is one byte too many.
        let big = vec![b'x'; MESSAGE_BUFFER];
        let r = recv_message(&mut Cursor::new(masked_frame(true, OP_TEXT, &big)), none);
        assert!(matches!(r, Err(WsError::Protocol(_))));
        let ok = vec![b'x'; MESSAGE_BUFFER - 1];
        let r = recv_message(&mut Cursor::new(masked_frame(true, OP_TEXT, &ok)), none);
        assert_eq!(r.unwrap().len(), MESSAGE_BUFFER - 1);
        // EOF mid-header is a close.
        assert!(matches!(recv_message(&mut Cursor::new(vec![0x81]), none), Err(WsError::Closed)));
    }

    #[test]
    fn parse_request_rules() {
        // Code keeps byte semantics: \u00XX is one byte, anything above 0xFF is '?'.
        let body = format!(
            r#" {{ "x" : 5 , "id":"a\"b", "y":"s", "code":"p{}{}\n" }} "#,
            esc("00e9"),
            esc("0100")
        );
        let r = parse_request(body.as_bytes()).unwrap();
        assert_eq!(r.id, "a\"b");
        assert_eq!(r.code, b"p\xe9?\n");
        assert!(parse_request(br#"{"id":"a"}"#).is_err());
        assert!(parse_request(br#"{"code":"a"}"#).is_err());
        assert!(parse_request(br#"{"id":"a","code":"\q"}"#).is_err());
        assert!(parse_request(br#"{"id":"a","code":"x\u0000y"}"#).is_err());
        assert!(parse_request(br#"{"id":"a\u0000","code":"x"}"#).is_err());
        assert!(parse_request(b"[1]").is_err());
        let long_id = format!(r#"{{"id":"{}","code":"x"}}"#, "i".repeat(128));
        assert!(parse_request(long_id.as_bytes()).is_err());
        let max_id = format!(r#"{{"id":"{}","code":"x"}}"#, "i".repeat(127));
        assert!(parse_request(max_id.as_bytes()).is_ok());
        let long_key = format!(r#"{{"{}":1,"id":"a","code":"x"}}"#, "k".repeat(32));
        assert!(parse_request(long_key.as_bytes()).is_err());
        let long_unknown = format!(r#"{{"u":"{}","id":"a","code":"x"}}"#, "v".repeat(256));
        assert!(parse_request(long_unknown.as_bytes()).is_err());
    }

    #[test]
    fn id_decodes_as_unicode_text() {
        // Escaped: a BMP character and a surrogate pair decode to their code points.
        let body = format!(
            r#"{{"id":"{}{}{}","code":"x"}}"#,
            esc("00e9"),
            esc("d83d"),
            esc("de00")
        );
        assert_eq!(parse_request(body.as_bytes()).unwrap().id, "\u{e9}\u{1F600}");
        // Raw UTF-8 passes through.
        let raw = "{\"id\":\"\u{e9}\u{2713}\",\"code\":\"x\"}";
        assert_eq!(parse_request(raw.as_bytes()).unwrap().id, "\u{e9}\u{2713}");
        // Invalid UTF-8 and lone surrogates are rejected.
        assert!(parse_request(b"{\"id\":\"\xe9\",\"code\":\"x\"}").is_err());
        let lone = format!(r#"{{"id":"{}","code":"x"}}"#, esc("d83d"));
        assert!(parse_request(lone.as_bytes()).is_err());
        let low = format!(r#"{{"id":"{}","code":"x"}}"#, esc("de00"));
        assert!(parse_request(low.as_bytes()).is_err());
    }

    #[test]
    fn typed_line_escapes_cp1252_and_controls() {
        let line = b"it\x92s caf\xe9 \"q\" \\ \n\t\x01\x80";
        assert_eq!(
            typed_line("log", line),
            format!(
                r#"{{"type":"log","line":"it{}s caf{} \"q\" \\ \n\t{}{}"}}"#,
                esc("2019"),
                esc("00e9"),
                esc("0001"),
                esc("20ac")
            )
            .into_bytes()
        );
    }

    /// A JSON `\uXXXX` escape for the given hex digits.
    pub(crate) fn esc(hex: &str) -> String {
        format!("{}u{hex}", '\\')
    }

    #[test]
    fn ack_escapes_the_id_as_utf8() {
        assert_eq!(ack_queued("q17abc"), br#"{"type":"ack","id":"q17abc","status":"queued"}"#);
        assert_eq!(
            ack_queued("a\",\"status\":\"x"),
            br#"{"type":"ack","id":"a\",\"status\":\"x","status":"queued"}"#
        );
        let id = "\u{e9}\u{2713}\u{1F600}\n";
        assert_eq!(
            ack_queued(id),
            "{\"type\":\"ack\",\"id\":\"\u{e9}\u{2713}\u{1F600}\\n\",\"status\":\"queued\"}"
                .as_bytes()
        );
        assert_eq!(
            ack_rejected("7", "UTF-16 \"BOM\""),
            br#"{"type":"ack","id":"7","status":"rejected","error":"UTF-16 \"BOM\""}"#
        );
    }

    #[test]
    fn non_ascii_id_round_trips_through_parse_and_ack() {
        let id = "r\u{e9}q-\u{4e2d}\u{1F600}";
        let mut body = b"{\"id\":\"".to_vec();
        escape_json_utf8(id, &mut body);
        body.extend_from_slice(b"\",\"code\":\"x\"}");
        let req = parse_request(&body).unwrap();
        assert_eq!(req.id, id);
        let mut expect = b"{\"type\":\"ack\",\"id\":\"".to_vec();
        escape_json_utf8(id, &mut expect);
        expect.extend_from_slice(b"\",\"status\":\"queued\"}");
        assert_eq!(ack_queued(&req.id), expect);
        assert!(std::str::from_utf8(&expect).is_ok());
    }

    #[test]
    fn dispatch_normalises_and_queues_as_ws() {
        let (log, lines) = recording_log();
        let q = Queue::new(log);
        let normalize: Normalize = Arc::new(|label: &str, b: &[u8]| {
            assert!(label.starts_with("ws chunk"));
            Ok(b.to_ascii_uppercase())
        });
        let req = Request {
            id: "7".into(),
            code: b"x=1".to_vec(),
        };
        assert_eq!(dispatch(&q, &normalize, &req), Ok(1));
        let c = q.pop().unwrap();
        assert_eq!((c.code.as_slice(), c.origin), (&b"X=1"[..], Origin::Ws));
        assert!(lines.lock().unwrap()[0].contains("queued chunk (id=7, 3 bytes)"));
    }

    #[test]
    fn dispatch_rejection_queues_nothing() {
        let (log, lines) = recording_log();
        let q = Queue::new(log);
        let normalize: Normalize = Arc::new(|_: &str, _: &[u8]| Err("looks UTF-16".into()));
        let req = Request {
            id: "8".into(),
            code: b"\xff\xfex".to_vec(),
        };
        assert_eq!(dispatch(&q, &normalize, &req), Err("looks UTF-16".to_string()));
        assert_eq!(q.pending(), 0);
        assert!(lines.lock().unwrap()[0].contains("rejected chunk (id=8, 3 bytes): looks UTF-16"));
    }
}
