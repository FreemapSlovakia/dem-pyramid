//! A one-at-a-time render queue that honours priority.
//!
//! A plain semaphore would do the serialising, but tokio's is FIFO: a premium
//! request arriving behind three anonymous ones waits for all three, which is
//! backwards. Waiters sit in a heap instead, so whoever is waiting with the
//! highest priority goes next.
//!
//! Priority is supplied by the caller. The service does not know what premium
//! means -- whatever authenticates the user decides, the same way it decides
//! what quality they may ask for.

use std::sync::{Arc, Mutex};
use std::time::Instant;
use tokio::sync::oneshot;

/// How fast waiting converts into priority, per second.
///
/// Strict priority starves: with a steady trickle of premium requests an
/// anonymous one never runs, it just sits until the proxy times out with no
/// response and nothing logged. At this rate a waiter overtakes a fresh
/// request ten points above it after fifty seconds.
const AGE_PER_SECOND: f64 = 0.2;

/// Refuse rather than accept work that will not be reached in any reasonable
/// time. Each entry is seconds of nine cores.
pub const MAX_QUEUE: usize = 32;

struct Waiter {
    priority: i32,
    seq: u64,
    since: Instant,
    /// Carries the permit itself, not a signal. That is what makes a lost
    /// grant impossible: if the waiter has gone by the time this is delivered,
    /// the undelivered permit is dropped, and its own Drop hands the slot to
    /// the next waiter. Signalling instead would leave the queue marked busy
    /// for ever, because the waiter that was told to proceed never built a
    /// permit to release.
    wake: oneshot::Sender<Permit>,
}

impl Waiter {
    fn effective(&self, now: Instant) -> f64 {
        f64::from(self.priority) + now.duration_since(self.since).as_secs_f64() * AGE_PER_SECOND
    }
}

#[derive(Default)]
struct Inner {
    busy: bool,
    waiting: Vec<Waiter>,
    next_seq: u64,
}

#[derive(Clone, Default)]
pub struct Queue(Arc<Mutex<Inner>>);

/// Held for the duration of a render; releases the slot when dropped,
/// including on panic, early return, or never being delivered at all.
pub struct Permit(Queue);

/// Nobody will get to this request in a sensible time.
pub struct QueueFull;

impl Queue {
    pub fn new() -> Self {
        Self::default()
    }

    /// Wait for the render slot. Higher `priority` goes first, tempered by how
    /// long each waiter has already waited.
    ///
    /// Returns the depth on arrival alongside the permit, counting the render
    /// in progress -- a caller who arrives to a busy service with nobody
    /// queued still waits a full render, and reporting 0 there would tell the
    /// client the opposite of the truth.
    pub async fn acquire(&self, priority: i32) -> Result<(Permit, usize), QueueFull> {
        let (rx, depth) = {
            let mut inner = self.0.lock().unwrap();
            let depth = inner.waiting.len() + usize::from(inner.busy);
            if !inner.busy {
                inner.busy = true;
                return Ok((Permit(self.clone()), depth));
            }
            if inner.waiting.len() >= MAX_QUEUE {
                return Err(QueueFull);
            }
            let (tx, rx) = oneshot::channel();
            let seq = inner.next_seq;
            inner.next_seq += 1;
            inner.waiting.push(Waiter {
                priority,
                seq,
                since: Instant::now(),
                wake: tx,
            });
            (rx, depth)
        };

        match rx.await {
            Ok(permit) => Ok((permit, depth)),
            // The queue is going away; nothing to wait for.
            Err(_) => Err(QueueFull),
        }
    }

    fn release(&self) {
        let mut inner = self.0.lock().unwrap();
        loop {
            let now = Instant::now();
            let best = inner
                .waiting
                .iter()
                .enumerate()
                .max_by(|(_, a), (_, b)| {
                    a.effective(now)
                        .partial_cmp(&b.effective(now))
                        .unwrap()
                        // Equal standing: first come, first served.
                        .then(b.seq.cmp(&a.seq))
                })
                .map(|(i, _)| i);

            match best {
                Some(i) => {
                    let w = inner.waiting.swap_remove(i);
                    match w.wake.send(Permit(self.clone())) {
                        // Handed over; the slot stays taken by its new owner.
                        Ok(()) => return,
                        // That waiter hung up while queued. Forget the permit
                        // rather than dropping it, which would re-enter
                        // release() while this lock is held, and keep looking.
                        Err(permit) => std::mem::forget(permit),
                    }
                }
                None => {
                    inner.busy = false;
                    return;
                }
            }
        }
    }
}

impl Drop for Permit {
    fn drop(&mut self) {
        self.0.release();
    }
}
