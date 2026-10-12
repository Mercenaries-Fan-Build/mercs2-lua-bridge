//! The game-thread pump: run on m2-sdk.dll's Lua tick (`m2_lua_on_tick`), which fires thousands of times
//! a frame and is skipped by the SDK while a chunk runs on that thread.
//!
//! The fast path is two atomic loads: the queue's pending count and whether OnLoad is due. With work,
//! the pump runs OnLoad's scripts first (once per load cycle, when the world-loaded milestone has been
//! seen), then every queued chunk, and routes each result: `lua_bridge.log`, the raw-TCP session that
//! asked, and for a loader-queued script (OnKey) the live log feed. The timestamps it keeps are what the
//! watchdog reads.

use crate::queue::{Origin, Queue};
use crate::watchdog::Snapshot;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

/// The pump's timestamps (`GetTickCount` values, 0 = never) and the OnLoad flag.
#[derive(Default)]
pub struct Pump {
    last_tick: AtomicU32,
    last_attempt: AtomicU32,
    last_progress: AtomicU32,
    exec_start: AtomicU32,
    onload_due: AtomicBool,
}

/// What a tick hands the pump: the clock, the executor and the folder runner, all on the game thread.
pub trait TickHost {
    fn now(&self) -> u32;
    /// Run one chunk on the tick's state; the result line (`[ok]…`, `[runtime]…`, …).
    fn exec(&mut self, code: &[u8]) -> Vec<u8>;
    /// Run the OnLoad folder on the tick's state.
    fn run_onload(&mut self);
    /// Mirror a loader-queued script's result to the live log feed.
    fn feed_loader(&self, rel_path: &str, result: &[u8]);
    fn log(&self, line: &str);
}

/// A nonzero tick count: 0 means "never" in the timestamps.
fn stamp(now: u32) -> u32 {
    now.max(1)
}

impl Pump {
    pub const fn new() -> Self {
        Pump {
            last_tick: AtomicU32::new(0),
            last_attempt: AtomicU32::new(0),
            last_progress: AtomicU32::new(0),
            exec_start: AtomicU32::new(0),
            onload_due: AtomicBool::new(false),
        }
    }

    /// The world-loaded milestone was seen with OnLoad enabled: run OnLoad on the next tick.
    pub fn schedule_onload(&self) {
        self.onload_due.store(true, Ordering::Release);
    }

    /// A new load cycle: an OnLoad not yet run belongs to the cycle that ended.
    pub fn cancel_onload(&self) {
        self.onload_due.store(false, Ordering::Release);
    }

    /// Run `f` as a chunk for the watchdog: `exec_start` is set for its duration.
    pub fn timed<R>(&self, now: u32, f: impl FnOnce() -> R) -> R {
        self.exec_start.store(stamp(now), Ordering::Release);
        let r = f();
        self.exec_start.store(0, Ordering::Release);
        r
    }

    /// One tick. `now` is read once for the timestamp; the host is only built when there is work.
    pub fn tick<H: TickHost>(&self, queue: &Queue, now: u32, host: impl FnOnce() -> H) {
        self.last_tick.store(stamp(now), Ordering::Relaxed);
        if queue.pending() == 0 && !self.onload_due.load(Ordering::Acquire) {
            return;
        }
        self.last_attempt.store(stamp(now), Ordering::Relaxed);
        let mut host = host();
        if self.onload_due.swap(false, Ordering::AcqRel) {
            self.timed(host.now(), || host.run_onload());
        }
        while let Some(chunk) = queue.pop() {
            let result = self.timed(host.now(), || host.exec(&chunk.code));
            host.log(&format!("[+] lua_bridge: Script executed. Result: {}", String::from_utf8_lossy(&result)));
            if let Origin::Loader(rel) = &chunk.origin {
                host.feed_loader(rel, &result);
            }
            queue.deliver(chunk.origin, &result);
            self.last_progress.store(stamp(host.now()), Ordering::Relaxed);
        }
    }

    /// The watchdog's view at `now`.
    pub fn snapshot(&self, queue: &Queue, now: u32) -> Snapshot {
        Snapshot {
            now,
            last_tick: self.last_tick.load(Ordering::Relaxed),
            last_attempt: self.last_attempt.load(Ordering::Relaxed),
            last_progress: self.last_progress.load(Ordering::Relaxed),
            exec_start: self.exec_start.load(Ordering::Acquire),
            pending: queue.pending(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::queue::tests::recording_log;
    use std::cell::{Cell, RefCell};

    /// What the hosts of every tick did, in order.
    #[derive(Default)]
    struct Record {
        clock: Cell<u32>,
        events: RefCell<Vec<String>>,
        built: Cell<u32>,
    }

    struct Host<'a>(&'a Record);

    impl TickHost for Host<'_> {
        fn now(&self) -> u32 {
            self.0.clock.set(self.0.clock.get() + 5);
            self.0.clock.get()
        }
        fn exec(&mut self, code: &[u8]) -> Vec<u8> {
            self.0.events.borrow_mut().push(format!("exec {}", String::from_utf8_lossy(code)));
            [b"[ok]\t".as_slice(), code].concat()
        }
        fn run_onload(&mut self) {
            self.0.events.borrow_mut().push("onload".into());
        }
        fn feed_loader(&self, rel_path: &str, result: &[u8]) {
            self.0.events.borrow_mut().push(format!("feed {rel_path} {}", String::from_utf8_lossy(result)));
        }
        fn log(&self, line: &str) {
            self.0.events.borrow_mut().push(line.into());
        }
    }

    fn host(r: &Record) -> Host<'_> {
        r.built.set(r.built.get() + 1);
        Host(r)
    }

    #[test]
    fn an_idle_tick_touches_nothing_but_the_tick_stamp() {
        let (log, _) = recording_log();
        let q = Queue::new(log);
        let p = Pump::new();
        let r = Record::default();
        p.tick(&q, 100, || host(&r));
        assert_eq!(r.built.get(), 0, "no work: the host is never built");
        let s = p.snapshot(&q, 100);
        assert_eq!((s.last_tick, s.last_attempt, s.last_progress, s.exec_start, s.pending), (100, 0, 0, 0, 0));
    }

    #[test]
    fn onload_runs_once_before_the_queue_drains_in_order() {
        let (log, _) = recording_log();
        let q = Queue::new(log);
        let p = Pump::new();
        q.push(b"a".to_vec(), Origin::Loader("OnKey/a.lua".into()));
        q.push(b"b".to_vec(), Origin::Ws);
        p.schedule_onload();
        let r = Record { clock: Cell::new(1000), ..Record::default() };
        p.tick(&q, 1000, || host(&r));
        p.tick(&q, 2000, || host(&r));
        assert_eq!(r.built.get(), 1, "the second tick has no work");
        assert_eq!(
            *r.events.borrow(),
            [
                "onload",
                "exec a",
                "[+] lua_bridge: Script executed. Result: [ok]\ta",
                "feed OnKey/a.lua [ok]\ta",
                "exec b",
                "[+] lua_bridge: Script executed. Result: [ok]\tb"
            ]
        );
        let s = p.snapshot(&q, 2000);
        assert_eq!((s.last_tick, s.last_attempt, s.exec_start, s.pending), (2000, 1000, 0, 0));
        assert!(s.last_progress > 1000);
    }

    #[test]
    fn only_the_raw_session_that_asked_gets_a_result() {
        let (log, _) = recording_log();
        let q = Queue::new(log);
        let session = q.begin_session();
        q.push(b"1".to_vec(), Origin::Raw(session));
        q.push(b"2".to_vec(), Origin::Ws);
        q.push(b"3".to_vec(), Origin::Loader("OnKey/3.lua".into()));
        q.push(b"4".to_vec(), Origin::Raw(session - 1));
        let p = Pump::new();
        let r = Record::default();
        p.tick(&q, 10, || host(&r));
        assert_eq!(q.take_output(), b"[ok]\t1\n<<<END>>>\n");
    }

    #[test]
    fn a_cancelled_onload_does_not_run() {
        let (log, _) = recording_log();
        let q = Queue::new(log);
        let p = Pump::new();
        p.schedule_onload();
        p.cancel_onload();
        let r = Record::default();
        p.tick(&q, 1, || host(&r));
        assert_eq!(r.built.get(), 0);
    }
}
