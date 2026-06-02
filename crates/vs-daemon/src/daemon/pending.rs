//! Pending-input queue. Backs the `vs_prompt_input` MCP path: the
//! `vs mcp` subprocess has no tty, so an MCP-driven agent that calls
//! `vs_prompt_input` enqueues a pending entry and blocks (with a
//! timeout) on a condvar. The user — at their interactive shell —
//! runs `vs pending fulfill <id>` (or `vs pending list` to see what's
//! queued), types the value into the local tty, and that fulfills
//! the entry. The condvar wakes the parked MCP request, the daemon
//! actually fills the field, and the agent's tool call returns
//! success.
//!
//! Local `vs prompt-input` never touches this queue — it reads from
//! the tty in-process. The queue exists only for the "no tty" case.

use std::collections::HashMap;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

/// One entry in the pending-input queue. Visible-to-the-user fields
/// (`page`, `r`, `message`, `secret`) are surfaced by `vs pending
/// list`; the daemon-internal fields (`token`, `group`) are passed
/// through to the `vs_act fill` call on fulfillment.
#[derive(Debug, Clone)]
pub struct PendingEntry {
    pub id: String,
    pub page: String,
    pub r: u32,
    pub message: String,
    pub secret: bool,
    pub token: String,
    pub group: Option<String>,
    pub created_at: Instant,
}

/// Outcome of a pending entry once it leaves the queue.
#[derive(Debug, Clone)]
pub enum FulfillState {
    Pending,
    Fulfilled(String),
    Cancelled,
}

/// The queue itself. `Inner.queue` holds the registry; `Inner.cv` is
/// the wake signal for parked `vs_prompt_input` calls. Wrapped in
/// `Arc<Mutex>` so multiple daemon threads can share it.
#[derive(Default)]
pub struct PendingQueue {
    inner: Mutex<HashMap<String, (PendingEntry, FulfillState)>>,
    cv: Condvar,
}

impl PendingQueue {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Enqueue a pending entry and block on the condvar until it is
    /// fulfilled, cancelled, or `timeout` elapses. Returns the value
    /// on fulfillment, `None` on cancellation or timeout.
    pub fn enqueue_and_wait(&self, entry: PendingEntry, timeout: Duration) -> Option<String> {
        let id = entry.id.clone();
        {
            let mut guard = self.inner.lock().unwrap();
            guard.insert(id.clone(), (entry, FulfillState::Pending));
        }
        let deadline = Instant::now() + timeout;
        let mut guard = self.inner.lock().unwrap();
        loop {
            let remaining = match deadline.checked_duration_since(Instant::now()) {
                Some(r) if !r.is_zero() => r,
                _ => {
                    // Timed out — drop the entry so list doesn't keep
                    // surfacing a dead prompt.
                    guard.remove(&id);
                    return None;
                }
            };
            let (g, _) = self.cv.wait_timeout(guard, remaining).unwrap();
            guard = g;
            if let Some((_, state)) = guard.get(&id) {
                match state.clone() {
                    FulfillState::Pending => continue,
                    FulfillState::Fulfilled(v) => {
                        guard.remove(&id);
                        return Some(v);
                    }
                    FulfillState::Cancelled => {
                        guard.remove(&id);
                        return None;
                    }
                }
            } else {
                return None;
            }
        }
    }

    /// Snapshot of all pending entries (id + user-visible metadata).
    /// Used by `vs pending list`.
    pub fn list(&self) -> Vec<PendingEntry> {
        let guard = self.inner.lock().unwrap();
        guard
            .values()
            .filter_map(|(e, s)| matches!(s, FulfillState::Pending).then(|| e.clone()))
            .collect()
    }

    /// Fulfill a pending entry with `value`. Returns `true` if the
    /// entry was found and was still pending.
    pub fn fulfill(&self, id: &str, value: String) -> bool {
        let mut guard = self.inner.lock().unwrap();
        if let Some((_, state)) = guard.get_mut(id) {
            if matches!(state, FulfillState::Pending) {
                *state = FulfillState::Fulfilled(value);
                self.cv.notify_all();
                return true;
            }
        }
        false
    }

    /// Cancel a pending entry. Same semantics as `fulfill` minus the
    /// value.
    pub fn cancel(&self, id: &str) -> bool {
        let mut guard = self.inner.lock().unwrap();
        if let Some((_, state)) = guard.get_mut(id) {
            if matches!(state, FulfillState::Pending) {
                *state = FulfillState::Cancelled;
                self.cv.notify_all();
                return true;
            }
        }
        false
    }

    /// Read a pending entry's metadata. Used by `vs pending fulfill`
    /// CLI to display the prompt text before reading user input.
    pub fn peek(&self, id: &str) -> Option<PendingEntry> {
        let guard = self.inner.lock().unwrap();
        guard
            .get(id)
            .and_then(|(e, s)| matches!(s, FulfillState::Pending).then(|| e.clone()))
    }
}

/// Generate a short, URL-safe id for a new entry. We don't need
/// cryptographic strength — collisions inside one daemon process are
/// vanishingly unlikely with 64 random bits.
pub fn new_id() -> String {
    use std::time::SystemTime;
    let nanos = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    // Mix nanos with a process-local atomic so two enqueues in the
    // same nanosecond don't collide.
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let counter = COUNTER.fetch_add(1, Ordering::Relaxed);
    let combined = ((nanos as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15)) ^ counter;
    format!("p_{combined:016x}")
}
