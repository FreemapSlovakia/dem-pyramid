//! A one-at-a-time render queue that honours priority.
//!
//! A plain semaphore would do the serialising, but tokio's is FIFO: a premium
//! request arriving behind three anonymous ones waits for all three, which is
//! backwards. This keeps the waiters in a heap instead, so whoever is waiting
//! with the highest priority goes next.
//!
//! Priority is supplied by the caller. The service does not know what premium
//! means -- the proxy that authenticates the user decides, the same way it
//! decides what quality they may ask for.

use std::cmp::Ordering;
use std::collections::BinaryHeap;
use std::sync::{Arc, Mutex};
use tokio::sync::oneshot;

struct Waiter {
    priority: i32,
    /// Arrival order, so equal priorities stay first-come-first-served.
    seq: u64,
    wake: oneshot::Sender<()>,
}

impl Ord for Waiter {
    fn cmp(&self, other: &Self) -> Ordering {
        // BinaryHeap is a max-heap: higher priority first, then lower seq.
        self.priority
            .cmp(&other.priority)
            .then_with(|| other.seq.cmp(&self.seq))
    }
}
impl PartialOrd for Waiter {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}
impl PartialEq for Waiter {
    fn eq(&self, other: &Self) -> bool {
        self.priority == other.priority && self.seq == other.seq
    }
}
impl Eq for Waiter {}

#[derive(Default)]
struct Inner {
    busy: bool,
    waiting: BinaryHeap<Waiter>,
    next_seq: u64,
}

#[derive(Clone, Default)]
pub struct Queue(Arc<Mutex<Inner>>);

/// Held for the duration of a render; releases the queue when dropped,
/// including on panic or early return.
pub struct Permit(Queue);

impl Queue {
    pub fn new() -> Self {
        Self::default()
    }

    /// Wait for the render slot. Higher `priority` goes first.
    pub async fn acquire(&self, priority: i32) -> Permit {
        let rx = {
            let mut inner = self.0.lock().unwrap();
            if !inner.busy {
                inner.busy = true;
                return Permit(self.clone());
            }
            let (tx, rx) = oneshot::channel();
            let seq = inner.next_seq;
            inner.next_seq += 1;
            inner.waiting.push(Waiter {
                priority,
                seq,
                wake: tx,
            });
            rx
        };

        // If the sender is dropped the queue is going away; treat that as
        // having the slot rather than hanging for ever.
        let _ = rx.await;
        Permit(self.clone())
    }

    /// How many are waiting, for the queue-depth header.
    pub fn depth(&self) -> usize {
        self.0.lock().unwrap().waiting.len()
    }

    fn release(&self) {
        let mut inner = self.0.lock().unwrap();
        loop {
            match inner.waiting.pop() {
                // A waiter whose receiver has gone abandoned the queue --
                // client hung up while waiting. Skip to the next one rather
                // than handing the slot to nobody.
                Some(w) => {
                    if w.wake.send(()).is_ok() {
                        return; // stays busy, now owned by the woken waiter
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
