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
    /// Set when this entry is one field of a `vs_prompt_form`. All
    /// entries of a form share the id; `wait_form` collects them.
    pub form: Option<String>,
    /// Position within the form, so fills run in declaration order.
    pub form_index: u32,
    pub created_at: Instant,
}

/// How long an entry without a parked waiter may sit in the queue
/// before it is garbage-collected. Form entries are enqueued without
/// a waiter (the agent parks in a separate `vs_prompt_form_wait`
/// call), so an agent that enqueues and dies would otherwise leak
/// entries forever.
const ORPHAN_TTL: Duration = Duration::from_secs(30 * 60);

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
    #[must_use]
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Enqueue a pending entry and block on the condvar until it is
    /// fulfilled, cancelled, or `timeout` elapses. Returns the value
    /// on fulfillment, `None` on cancellation or timeout.
    #[must_use]
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
                    guard.remove(&id);
                    return None;
                }
            };
            let (g, _) = self.cv.wait_timeout(guard, remaining).unwrap();
            guard = g;
            let (_, state) = guard.get(&id)?;
            match state.clone() {
                FulfillState::Pending => {}
                FulfillState::Fulfilled(v) => {
                    guard.remove(&id);
                    return Some(v);
                }
                FulfillState::Cancelled => {
                    guard.remove(&id);
                    return None;
                }
            }
        }
    }

    /// Enqueue an entry without parking. Used by `vs_prompt_form`:
    /// the enqueue call returns immediately (with the entry web URL)
    /// and the agent parks later in `vs_prompt_form_wait`.
    pub fn enqueue(&self, entry: PendingEntry) {
        let mut guard = self.inner.lock().unwrap();
        Self::gc(&mut guard);
        guard.insert(entry.id.clone(), (entry, FulfillState::Pending));
    }

    /// Block until every entry of `form` is fulfilled, all are
    /// cancelled, or `timeout` elapses. On full fulfillment returns
    /// the entries with their values, sorted by `form_index`; on
    /// cancellation or timeout returns `None`. Either way the form's
    /// entries leave the queue.
    #[must_use]
    pub fn wait_form(&self, form: &str, timeout: Duration) -> Option<Vec<(PendingEntry, String)>> {
        let deadline = Instant::now() + timeout;
        let mut guard = self.inner.lock().unwrap();
        loop {
            let mut done = Vec::new();
            let mut open = 0usize;
            let mut cancelled = false;
            for (e, s) in guard.values() {
                if e.form.as_deref() != Some(form) {
                    continue;
                }
                match s {
                    FulfillState::Pending => open += 1,
                    FulfillState::Fulfilled(v) => done.push((e.clone(), v.clone())),
                    FulfillState::Cancelled => cancelled = true,
                }
            }
            let total = done.len() + open;
            if cancelled || total == 0 {
                guard.retain(|_, (e, _)| e.form.as_deref() != Some(form));
                return None;
            }
            if open == 0 {
                guard.retain(|_, (e, _)| e.form.as_deref() != Some(form));
                done.sort_by_key(|(e, _)| e.form_index);
                return Some(done);
            }
            let remaining = match deadline.checked_duration_since(Instant::now()) {
                Some(r) if !r.is_zero() => r,
                _ => {
                    guard.retain(|_, (e, _)| e.form.as_deref() != Some(form));
                    return None;
                }
            };
            let (g, _) = self.cv.wait_timeout(guard, remaining).unwrap();
            guard = g;
        }
    }

    /// Drop entries past [`ORPHAN_TTL`]. Entries with a parked waiter
    /// never reach the TTL (the waiter removes them on its own
    /// timeout, which is shorter); this catches enqueue-only entries
    /// whose agent never came back to wait.
    fn gc(guard: &mut HashMap<String, (PendingEntry, FulfillState)>) {
        guard.retain(|_, (e, _)| e.created_at.elapsed() < ORPHAN_TTL);
    }

    /// Snapshot of all pending entries (id + user-visible metadata).
    #[must_use]
    pub fn list(&self) -> Vec<PendingEntry> {
        let mut guard = self.inner.lock().unwrap();
        Self::gc(&mut guard);
        let mut entries: Vec<PendingEntry> = guard
            .values()
            .filter(|(_, s)| matches!(s, FulfillState::Pending))
            .map(|(e, _)| e.clone())
            .collect();
        entries.sort_by(|a, b| {
            (a.form.as_deref(), a.form_index, &a.id).cmp(&(b.form.as_deref(), b.form_index, &b.id))
        });
        entries
    }

    /// Fulfill a pending entry with `value`. Wakes parked waiters.
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

    /// Cancel a pending entry.
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

    /// Peek a pending entry (no removal).
    #[must_use]
    pub fn peek(&self, id: &str) -> Option<PendingEntry> {
        let guard = self.inner.lock().unwrap();
        guard
            .get(id)
            .filter(|(_, s)| matches!(s, FulfillState::Pending))
            .map(|(e, _)| e.clone())
    }
}

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::SystemTime;

static COUNTER: AtomicU64 = AtomicU64::new(0);

/// Generate a short, URL-safe id for a new entry.
#[must_use]
pub fn new_id() -> String {
    fresh_id("p")
}

/// Generate an id for a form (a group of entries fulfilled together).
#[must_use]
pub fn new_form_id() -> String {
    fresh_id("f")
}

fn fresh_id(prefix: &str) -> String {
    let nanos = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0_u128, |d| d.as_nanos());
    let counter = COUNTER.fetch_add(1, Ordering::Relaxed);
    #[allow(clippy::cast_possible_truncation)]
    let n = nanos as u64;
    let combined = n.wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ counter;
    format!("{prefix}_{combined:016x}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(id: &str, form: Option<&str>, idx: u32) -> PendingEntry {
        PendingEntry {
            id: id.into(),
            page: "p_1".into(),
            r: idx,
            message: format!("field {idx}"),
            secret: false,
            token: "0000000000000000".into(),
            group: None,
            form: form.map(Into::into),
            form_index: idx,
            created_at: Instant::now(),
        }
    }

    #[test]
    fn wait_form_collects_in_order_after_out_of_order_fulfill() {
        let q = PendingQueue::new();
        q.enqueue(entry("a", Some("f_1"), 0));
        q.enqueue(entry("b", Some("f_1"), 1));
        // Fulfill before the wait even starts, in reverse order.
        assert!(q.fulfill("b", "two".into()));
        assert!(q.fulfill("a", "one".into()));
        let got = q.wait_form("f_1", Duration::from_secs(1)).unwrap();
        let values: Vec<_> = got.iter().map(|(_, v)| v.as_str()).collect();
        assert_eq!(values, ["one", "two"]);
        assert!(q.list().is_empty(), "form entries must leave the queue");
    }

    #[test]
    fn wait_form_wakes_when_last_field_lands() {
        let q = PendingQueue::new();
        q.enqueue(entry("a", Some("f_2"), 0));
        q.enqueue(entry("b", Some("f_2"), 1));
        assert!(q.fulfill("a", "x".into()));
        let q2 = q.clone();
        let waiter = std::thread::spawn(move || q2.wait_form("f_2", Duration::from_secs(5)));
        std::thread::sleep(Duration::from_millis(100));
        assert!(q.fulfill("b", "y".into()));
        let got = waiter.join().unwrap().expect("form fulfilled");
        assert_eq!(got.len(), 2);
    }

    #[test]
    fn wait_form_cancel_and_timeout_return_none() {
        let q = PendingQueue::new();
        q.enqueue(entry("a", Some("f_3"), 0));
        assert!(q.cancel("a"));
        assert!(q.wait_form("f_3", Duration::from_secs(1)).is_none());
        // Unknown form: nothing to wait on.
        assert!(q.wait_form("f_nope", Duration::from_millis(50)).is_none());
        // Timeout: entry stays unfulfilled past the deadline.
        q.enqueue(entry("b", Some("f_4"), 0));
        assert!(q.wait_form("f_4", Duration::from_millis(50)).is_none());
        assert!(q.list().is_empty(), "timed-out form must be cleaned up");
    }
}
