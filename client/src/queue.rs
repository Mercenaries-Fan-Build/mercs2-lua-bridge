//! The chunk queue between the network threads and the game thread, and the raw-TCP result buffer.
//!
//! Server threads [`Queue::push`] chunks; the game thread [`Queue::pop`]s them, runs them, and hands
//! each result to [`Queue::deliver`]. Delivery writes into an in-memory output buffer that the REPL
//! connection thread drains and sends. The game thread therefore never touches a socket, and the
//! output lock is only ever held for a memory copy, never across a `send`.
//!
//! Result routing: a result goes back over raw TCP only when the chunk came from a raw-TCP session
//! and that session is still the attached one. Each raw connection gets a fresh session id, so a
//! result whose requester has gone is dropped (and logged) instead of being read by the next client
//! as the answer to its own request.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

/// A log sink shared by the network threads and the game thread.
pub type LogFn = Arc<dyn Fn(&str) + Send + Sync>;

/// Source-encoding normalisation applied to every network-submitted chunk before it is queued,
/// the same transform file loads get. Arguments: a label naming the source (for its log lines) and
/// the raw chunk bytes. Returns the bytes to queue, or why the chunk cannot be run.
pub type Normalize = Arc<dyn Fn(&str, &[u8]) -> Result<Vec<u8>, String> + Send + Sync>;

/// Terminates every raw-TCP result.
pub const END_MARKER: &[u8] = b"<<<END>>>";

/// Who submitted a chunk, which decides where its result may go.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Origin {
    /// A raw-TCP REPL request from the given session.
    Raw(u64),
    /// A WebSocket request. Its result travels over the WebSocket feed, never the raw-TCP buffer.
    Ws,
    /// A loader-fired script (OnBoot / OnLoad / OnKey), labelled with the script's relative path
    /// so its result can be mirrored to the log feed. Nobody asked for it over a socket.
    Loader(String),
}

/// One queued Lua chunk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Chunk {
    pub code: Vec<u8>,
    pub origin: Origin,
}

/// What [`Queue::deliver`] did with a result.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Delivery {
    /// Appended to the raw-TCP output buffer for the session that asked.
    Delivered,
    /// The requesting raw-TCP session is gone; the result was dropped and logged. `current` is the
    /// attached session, if any.
    DroppedStale { session: u64, current: Option<u64> },
    /// A WebSocket or loader chunk; there is no raw-TCP requester to answer.
    NotRaw,
}

struct Output {
    /// The attached raw-TCP session, if one is attached.
    current: Option<u64>,
    /// The id the next session receives.
    next: u64,
    bytes: Vec<u8>,
}

struct Shared {
    chunks: Mutex<VecDeque<Chunk>>,
    pending: AtomicUsize,
    output: Mutex<Output>,
    log: LogFn,
}

/// The thread-safe chunk queue plus the raw-TCP output buffer. Cloning shares the same queue.
#[derive(Clone)]
pub struct Queue {
    shared: Arc<Shared>,
}

impl Queue {
    pub fn new(log: LogFn) -> Queue {
        Queue {
            shared: Arc::new(Shared {
                chunks: Mutex::new(VecDeque::new()),
                pending: AtomicUsize::new(0),
                output: Mutex::new(Output {
                    current: None,
                    next: 1,
                    bytes: Vec::new(),
                }),
                log,
            }),
        }
    }

    /// Append a chunk. Returns the pending count after the push, so a caller can detect the
    /// empty-to-non-empty transition (a return of 1).
    pub fn push(&self, code: Vec<u8>, origin: Origin) -> usize {
        let mut chunks = lock(&self.shared.chunks, "chunk queue");
        chunks.push_back(Chunk { code, origin });
        let after = chunks.len();
        self.shared.pending.store(after, Ordering::Release);
        after
    }

    /// Take the oldest chunk, if any.
    pub fn pop(&self) -> Option<Chunk> {
        let mut chunks = lock(&self.shared.chunks, "chunk queue");
        let chunk = chunks.pop_front();
        self.shared.pending.store(chunks.len(), Ordering::Release);
        chunk
    }

    /// Chunks waiting to run. Lock-free, for the per-frame pump gate.
    pub fn pending(&self) -> usize {
        self.shared.pending.load(Ordering::Acquire)
    }

    /// The attached raw-TCP session id, if a raw client is attached.
    pub fn current_session(&self) -> Option<u64> {
        lock(&self.shared.output, "output buffer").current
    }

    /// Route one chunk's result. A raw-TCP result for the attached session is appended as
    /// `<result>\n<<<END>>>\n`. The session check and the append happen under one lock, so a
    /// session that starts in between cannot receive a result meant for its predecessor.
    pub fn deliver(&self, origin: Origin, result: &[u8]) -> Delivery {
        let Origin::Raw(session) = origin else {
            return Delivery::NotRaw;
        };
        let mut out = lock(&self.shared.output, "output buffer");
        if out.current == Some(session) {
            append_line(&mut out.bytes, result);
            append_line(&mut out.bytes, END_MARKER);
            Delivery::Delivered
        } else {
            let current = out.current;
            drop(out);
            let now = match current {
                Some(c) => format!("current session {c}"),
                None => "no session attached".to_string(),
            };
            (self.shared.log)(&format!(
                "[*] lua_bridge: dropped result for session {session} (client gone; {now}) - not \
                 delivering to another client"
            ));
            Delivery::DroppedStale { session, current }
        }
    }

    /// Attach a new raw-TCP session: assign the next id and discard any output left over.
    pub(crate) fn begin_session(&self) -> u64 {
        let mut out = lock(&self.shared.output, "output buffer");
        let id = out.next;
        out.next += 1;
        out.current = Some(id);
        out.bytes.clear();
        id
    }

    /// Detach `session` (if it is still the attached one) and discard its unsent output.
    pub(crate) fn end_session(&self, session: u64) {
        let mut out = lock(&self.shared.output, "output buffer");
        if out.current == Some(session) {
            out.current = None;
            out.bytes.clear();
        }
    }

    /// Append `line` plus a newline to the raw-TCP output buffer.
    pub(crate) fn append_output(&self, line: &[u8]) {
        let mut out = lock(&self.shared.output, "output buffer");
        append_line(&mut out.bytes, line);
    }

    /// Take everything buffered for the raw-TCP client, leaving the buffer empty. The caller sends
    /// it after the lock is released.
    pub(crate) fn take_output(&self) -> Vec<u8> {
        std::mem::take(&mut lock(&self.shared.output, "output buffer").bytes)
    }

    pub(crate) fn log(&self, line: &str) {
        (self.shared.log)(line);
    }
}

fn append_line(buf: &mut Vec<u8>, line: &[u8]) {
    buf.extend_from_slice(line);
    buf.push(b'\n');
}

/// Lock a mutex, failing hard if another thread panicked while holding it.
pub(crate) fn lock<'a, T>(m: &'a Mutex<T>, what: &str) -> MutexGuard<'a, T> {
    match m.lock() {
        Ok(g) => g,
        Err(_) => panic!("lua_bridge: the {what} mutex is poisoned (a thread panicked holding it)"),
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A log sink that records every line, for asserting on log output.
    pub(crate) fn recording_log() -> (LogFn, Arc<Mutex<Vec<String>>>) {
        let lines = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&lines);
        let log: LogFn = Arc::new(move |l: &str| sink.lock().unwrap().push(l.to_string()));
        (log, lines)
    }

    #[test]
    fn push_pop_is_fifo_and_tracks_pending() {
        let (log, _) = recording_log();
        let q = Queue::new(log);
        assert_eq!(q.pending(), 0);
        assert_eq!(q.push(b"a".to_vec(), Origin::Loader("OnKey/a.lua".into())), 1);
        assert_eq!(q.push(b"b".to_vec(), Origin::Ws), 2);
        assert_eq!(q.pending(), 2);
        let a = q.pop().unwrap();
        assert_eq!((a.code.as_slice(), a.origin), (&b"a"[..], Origin::Loader("OnKey/a.lua".into())));
        assert_eq!(q.pending(), 1);
        let b = q.pop().unwrap();
        assert_eq!((b.code.as_slice(), b.origin), (&b"b"[..], Origin::Ws));
        assert_eq!(q.pending(), 0);
        assert!(q.pop().is_none());
        assert_eq!(q.push(b"c".to_vec(), Origin::Raw(1)), 1);
    }

    #[test]
    fn current_session_result_is_framed_with_end_marker() {
        let (log, _) = recording_log();
        let q = Queue::new(log);
        let s = q.begin_session();
        assert_eq!(q.current_session(), Some(s));
        assert_eq!(q.deliver(Origin::Raw(s), b"42"), Delivery::Delivered);
        assert_eq!(q.take_output(), b"42\n<<<END>>>\n");
        assert!(q.take_output().is_empty());
    }

    #[test]
    fn stale_session_result_is_dropped_and_logged() {
        let (log, lines) = recording_log();
        let q = Queue::new(log);
        let old = q.begin_session();
        q.end_session(old);
        assert_eq!(
            q.deliver(Origin::Raw(old), b"late"),
            Delivery::DroppedStale {
                session: old,
                current: None
            }
        );
        let new = q.begin_session();
        assert_eq!(
            q.deliver(Origin::Raw(old), b"later"),
            Delivery::DroppedStale {
                session: old,
                current: Some(new)
            }
        );
        assert!(q.take_output().is_empty());
        let lines = lines.lock().unwrap();
        assert_eq!(lines.len(), 2);
        assert!(lines[0].contains("dropped result for session 1"));
        assert!(lines[0].contains("no session attached"));
        assert!(lines[1].contains("current session 2"));
    }

    #[test]
    fn ws_and_loader_results_never_reach_the_raw_buffer() {
        let (log, lines) = recording_log();
        let q = Queue::new(log);
        q.begin_session();
        assert_eq!(q.deliver(Origin::Ws, b"x"), Delivery::NotRaw);
        assert_eq!(q.deliver(Origin::Loader("OnKey/k.lua".into()), b"y"), Delivery::NotRaw);
        assert!(q.take_output().is_empty());
        assert!(lines.lock().unwrap().is_empty());
    }

    #[test]
    fn session_boundaries_discard_leftover_output() {
        let (log, _) = recording_log();
        let q = Queue::new(log);
        let s = q.begin_session();
        q.append_output(b"[queued]");
        q.deliver(Origin::Raw(s), b"r");
        q.begin_session();
        assert!(q.take_output().is_empty());
        let t = q.current_session().unwrap();
        q.append_output(b"[queued]");
        q.end_session(t);
        assert!(q.take_output().is_empty());
        assert_eq!(q.current_session(), None);
    }

    #[test]
    fn pushes_from_many_threads_all_arrive() {
        let (log, _) = recording_log();
        let q = Queue::new(log);
        let handles: Vec<_> = (0..8)
            .map(|t| {
                let q = q.clone();
                std::thread::spawn(move || {
                    for i in 0..100 {
                        q.push(format!("{t}:{i}").into_bytes(), Origin::Loader(format!("OnBoot/{t}.lua")));
                    }
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }
        assert_eq!(q.pending(), 800);
        let mut n = 0;
        while q.pop().is_some() {
            n += 1;
        }
        assert_eq!((n, q.pending()), (800, 0));
    }
}
