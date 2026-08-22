//! Live progress for a request that has not answered yet.
//!
//! A render takes tens of seconds and says nothing until it is finished, which
//! is a long time to show a spinner. The pieces needed to do better were
//! already here: the queue knows who is ahead of whom, and the marcher already
//! stops at every output column to check for cancellation, so counting there
//! costs an atomic increment.
//!
//! Reported over a side channel rather than in the response, because the
//! response is one multipart body that arrives at the end. The client makes up
//! a token, sends it with the request, and subscribes to it separately -- so
//! nothing about the existing request or response changes, and no render id,
//! result store or expiry has to be invented.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU8, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Phase {
    Queued,
    Rendering,
    Encoding,
    Done,
}

impl Phase {
    fn as_str(self) -> &'static str {
        match self {
            Phase::Queued => "queued",
            Phase::Rendering => "rendering",
            Phase::Encoding => "encoding",
            Phase::Done => "done",
        }
    }

    fn from_u8(v: u8) -> Self {
        match v {
            1 => Phase::Rendering,
            2 => Phase::Encoding,
            3 => Phase::Done,
            _ => Phase::Queued,
        }
    }
}

/// One request's progress, shared between the handler, the queue, the render
/// and whoever is watching.
#[derive(Default)]
pub struct Job {
    phase: AtomicU8,
    /// Renders that must finish before this one starts. 0 means next.
    ahead: AtomicUsize,
    done: AtomicUsize,
    total: AtomicUsize,
}

impl Job {
    pub fn set_phase(&self, phase: Phase) {
        self.phase.store(phase as u8, Ordering::Relaxed);
    }

    pub fn set_ahead(&self, n: usize) {
        self.ahead.store(n, Ordering::Relaxed);
    }

    /// How many units of work the render will report, set before it starts.
    pub fn set_total(&self, n: usize) {
        self.total.store(n, Ordering::Relaxed);
    }

    /// One unit finished. Called from every worker, so it must stay cheap.
    pub fn tick(&self) {
        self.done.fetch_add(1, Ordering::Relaxed);
    }

    pub fn snapshot(&self) -> Snapshot {
        let total = self.total.load(Ordering::Relaxed);
        let done = self.done.load(Ordering::Relaxed).min(total);
        Snapshot {
            phase: Phase::from_u8(self.phase.load(Ordering::Relaxed)),
            ahead: self.ahead.load(Ordering::Relaxed),
            #[allow(clippy::cast_precision_loss)]
            fraction: if total == 0 {
                0.0
            } else {
                done as f64 / total as f64
            },
        }
    }
}

pub struct Snapshot {
    pub phase: Phase,
    pub ahead: usize,
    pub fraction: f64,
}

impl Snapshot {
    /// The wire form. Deliberately small -- no ETA, because the client knows
    /// when it started and can work that out better than the server can.
    ///
    /// `final` rather than a phase the client must recognise: a stream can end
    /// for reasons that are not "the render finished" -- the request was
    /// rejected before it ever registered, or nothing arrived under this token
    /// at all -- and a client that only closes on `done` would reconnect for
    /// ever in those cases.
    pub fn to_json(&self, last: bool) -> serde_json::Value {
        serde_json::json!({
            "phase": self.phase.as_str(),
            "ahead": self.ahead,
            "percent": (self.fraction * 100.0).round() as u32,
            "final": last,
        })
    }
}

/// Tokens currently being worked on.
///
/// Bounded, because the token comes from the caller: without a cap, requests
/// that never arrive would still leave entries behind.
#[derive(Clone, Default)]
pub struct Jobs(Arc<Mutex<HashMap<String, Arc<Job>>>>);

const MAX_JOBS: usize = 256;
/// Long enough to be unguessable by another client, short enough to bound the
/// map. A UUID fits comfortably.
const MAX_TOKEN: usize = 64;

impl Jobs {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a token, returning the job and a guard that unregisters it.
    ///
    /// The guard is owned by the request, so a client that hangs up takes its
    /// entry with it -- the same reason cancellation is tied to a guard.
    pub fn register(&self, token: &str) -> Option<(Arc<Job>, Registration)> {
        if token.is_empty() || token.len() > MAX_TOKEN {
            return None;
        }
        let job = Arc::new(Job::default());
        {
            let mut map = self.0.lock().unwrap();
            if map.len() >= MAX_JOBS {
                return None;
            }
            // Refused rather than replaced. Tokens come from callers, so two
            // requests can arrive under one -- a retry, a panorama and a
            // viewshed sharing it, or another client guessing. Overwriting
            // would leave the first request's guard to unregister the second,
            // whose subscriber would then be told the render had finished
            // while it was still queued.
            if map.contains_key(token) {
                return None;
            }
            map.insert(token.to_owned(), job.clone());
        }
        let registration = Registration {
            jobs: self.clone(),
            token: token.to_owned(),
            job: job.clone(),
        };
        Some((job, registration))
    }

    pub fn get(&self, token: &str) -> Option<Arc<Job>> {
        self.0.lock().unwrap().get(token).cloned()
    }
}

pub struct Registration {
    jobs: Jobs,
    token: String,
    job: Arc<Job>,
}

impl Drop for Registration {
    fn drop(&mut self) {
        let mut map = self.jobs.0.lock().unwrap();
        // Only if the entry is still this request's own. Registration refuses
        // duplicates, so it should always be -- and removing by name alone
        // would be the one way a later request could lose its entry to an
        // earlier one's guard.
        if map.get(&self.token).is_some_and(|j| Arc::ptr_eq(j, &self.job)) {
            map.remove(&self.token);
        }
    }
}
