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

/// Why a [`PendingQueue::wait_form`] call came back. The three
/// not-ready cases are kept apart because they need opposite things
/// from the caller: `StillPending` means park again, `Cancelled` and
/// `Unknown` mean stop.
#[derive(Debug)]
pub enum FormWait {
    /// Every field fulfilled, sorted by `form_index`.
    Ready(Vec<(PendingEntry, String)>),
    /// The wait budget elapsed with fields still open. The form is
    /// untouched and a later waiter can still collect it.
    StillPending,
    /// A field was cancelled; the form's entries are gone.
    Cancelled,
    /// No entries carry this form id: never enqueued, already
    /// collected, or reaped by [`ORPHAN_TTL`].
    Unknown,
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

    /// Cancel any pending form that a fresh, identical ask makes
    /// dead: same page, same refs in the same order. Returns the form
    /// ids cancelled.
    ///
    /// A form outlives the waiter that enqueued it, so an agent whose
    /// `vs_prompt_form_wait` ran out of budget and asked again left
    /// the first one queued for [`ORPHAN_TTL`]. Nobody could ever
    /// collect it — the only caller who knew its id had moved on —
    /// but it still showed in `vs pending list` and on the unscoped
    /// `vs pending url` page, where the human saw the same fields
    /// twice and could not tell which pair was live.
    ///
    /// A form with any fulfilled field is never superseded. Those
    /// values are the human's typing, and [`Self::restore`] exists
    /// precisely so that a caller that cannot use them hands them
    /// back rather than destroying them.
    pub fn supersede(&self, page: &str, refs: &[u32]) -> Vec<String> {
        let mut guard = self.inner.lock().unwrap();
        let mut forms: HashMap<String, Vec<(u32, u32)>> = HashMap::new();
        let mut touched: Vec<String> = Vec::new();
        for (e, s) in guard.values() {
            let Some(form) = e.form.clone() else { continue };
            if e.page != page {
                continue;
            }
            match s {
                FulfillState::Pending => forms.entry(form).or_default().push((e.form_index, e.r)),
                // Fulfilled or already cancelled: leave the form be.
                _ => touched.push(form),
            }
        }
        let dead: Vec<String> = forms
            .into_iter()
            .filter(|(form, fields)| {
                if touched.contains(form) {
                    return false;
                }
                let mut fields = fields.clone();
                fields.sort_unstable();
                fields.iter().map(|(_, r)| *r).eq(refs.iter().copied())
            })
            .map(|(form, _)| form)
            .collect();
        for (e, s) in guard.values_mut() {
            if e.form
                .as_deref()
                .is_some_and(|f| dead.iter().any(|d| d == f))
            {
                *s = FulfillState::Cancelled;
            }
        }
        if !dead.is_empty() {
            self.cv.notify_all();
        }
        dead
    }

    /// Block until every entry of `form` is fulfilled, all are
    /// cancelled, or `timeout` elapses. The four outcomes are
    /// distinct (see [`FormWait`]) because a timeout and a dead form
    /// call for opposite handling by the caller.
    ///
    /// Fulfillment and cancellation take the form's entries out of the
    /// queue. A **timeout does not** — the waiter's budget is not the
    /// form's lifetime, and callers routinely park again on the same
    /// form after their transport cut the first wait short. Entries
    /// nobody ever comes back for are reaped by [`ORPHAN_TTL`].
    #[must_use]
    pub fn wait_form(&self, form: &str, timeout: Duration) -> FormWait {
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
            if cancelled {
                guard.retain(|_, (e, _)| e.form.as_deref() != Some(form));
                return FormWait::Cancelled;
            }
            if total == 0 {
                return FormWait::Unknown;
            }
            if open == 0 {
                guard.retain(|_, (e, _)| e.form.as_deref() != Some(form));
                done.sort_by_key(|(e, _)| e.form_index);
                return FormWait::Ready(done);
            }
            let remaining = match deadline.checked_duration_since(Instant::now()) {
                Some(r) if !r.is_zero() => r,
                // Timed out. Leave the form's entries in the queue.
                //
                // The waiter's deadline is not the form's lifetime:
                // an MCP host caps a tool call well below the wait
                // budget, so the first `vs_prompt_form_wait` routinely
                // dies while the human is still typing. Purging here
                // meant that call took the form down with it — the
                // human's submit landed on nothing and the agent's
                // retry got "cancelled, timed out, or unknown" for a
                // form that was still perfectly live. A form now
                // outlives any number of waiters and is reaped only by
                // cancel, by completion, or by ORPHAN_TTL.
                _ => return FormWait::StillPending,
            };
            let (g, _) = self.cv.wait_timeout(guard, remaining).unwrap();
            guard = g;
        }
    }

    /// Put a collected form's entries back, values and all.
    ///
    /// [`Self::wait_form`] takes a fulfilled form out of the queue, so
    /// until this existed a caller that failed to *use* the values
    /// destroyed them: the human's password was gone and their only
    /// way back was to type it again. The values are the one thing in
    /// this system that cannot be recreated, so a caller that cannot
    /// complete hands them back instead.
    ///
    /// Entries come back `Fulfilled`, so the next `wait_form` on that
    /// form returns them immediately rather than parking. They are
    /// still subject to [`ORPHAN_TTL`], which bounds how long a form
    /// nobody can complete keeps a secret in memory.
    pub fn restore(&self, entries: Vec<(PendingEntry, String)>) {
        let mut guard = self.inner.lock().unwrap();
        for (entry, value) in entries {
            guard.insert(entry.id.clone(), (entry, FulfillState::Fulfilled(value)));
        }
        self.cv.notify_all();
    }

    /// Drop entries past [`ORPHAN_TTL`]. This is the only thing that
    /// reaps a form nobody fulfilled or cancelled — a waiter timing
    /// out deliberately leaves the entries alone (see [`Self::wait_form`]).
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

    /// The collected values of a `Ready` wait, or a panic naming the
    /// outcome we got instead.
    fn ready(w: FormWait) -> Vec<(PendingEntry, String)> {
        match w {
            FormWait::Ready(v) => v,
            other => panic!("expected Ready, got {other:?}"),
        }
    }

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
        let got = ready(q.wait_form("f_1", Duration::from_secs(1)));
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
        let got = ready(waiter.join().unwrap());
        assert_eq!(got.len(), 2);
    }

    /// The three not-ready outcomes are told apart. They read the
    /// same to a human and mean opposite things to a caller: park
    /// again, or give up.
    #[test]
    fn wait_form_distinguishes_cancel_timeout_and_unknown() {
        let q = PendingQueue::new();
        q.enqueue(entry("a", Some("f_3"), 0));
        assert!(q.cancel("a"));
        assert!(matches!(
            q.wait_form("f_3", Duration::from_secs(1)),
            FormWait::Cancelled
        ));
        assert!(q.list().is_empty(), "cancelled form must be cleaned up");

        // Unknown form: nothing to wait on.
        assert!(matches!(
            q.wait_form("f_nope", Duration::from_millis(50)),
            FormWait::Unknown
        ));

        // Live form, waiter out of budget: still pending, not dead.
        q.enqueue(entry("b", Some("f_4"), 0));
        assert!(matches!(
            q.wait_form("f_4", Duration::from_millis(50)),
            FormWait::StillPending
        ));
        assert_eq!(q.list().len(), 1);
    }

    /// An identical re-ask kills the form nobody can collect any
    /// more, and leaves alone one the human has already typed into.
    #[test]
    fn supersede_kills_the_dead_twin_and_spares_a_typed_one() {
        let q = PendingQueue::new();
        q.enqueue(entry("a", Some("f_7"), 0));
        q.enqueue(entry("b", Some("f_7"), 1));

        // A different page is a different ask.
        let mut elsewhere = entry("c", Some("f_8"), 0);
        elsewhere.page = "p_2".into();
        elsewhere.r = 0;
        q.enqueue(elsewhere);

        assert_eq!(q.supersede("p_1", &[0, 1]), ["f_7"]);
        assert_eq!(
            q.list().len(),
            1,
            "only the other page's form is still pending"
        );
        assert!(matches!(
            q.wait_form("f_7", Duration::from_millis(50)),
            FormWait::Cancelled
        ));

        // Same shape, but the human has typed into it: untouchable.
        q.enqueue(entry("d", Some("f_9"), 0));
        q.enqueue(entry("e", Some("f_9"), 1));
        assert!(q.fulfill("d", "typed".into()));
        assert!(
            q.supersede("p_1", &[0, 1]).is_empty(),
            "a form with a fulfilled field is never superseded"
        );

        // A different field set is a different ask.
        q.enqueue(entry("f", Some("f_10"), 0));
        assert!(q.supersede("p_1", &[0, 1, 2]).is_empty());
    }

    /// A caller that collects a form and then fails to use the
    /// values hands them back. Losing them means the human types
    /// their password a second time because the agent addressed the
    /// wrong session, which is not a cost they should pay.
    #[test]
    fn restored_form_is_collectable_again() {
        let q = PendingQueue::new();
        q.enqueue(entry("a", Some("f_6"), 0));
        q.enqueue(entry("b", Some("f_6"), 1));
        assert!(q.fulfill("a", "one".into()));
        assert!(q.fulfill("b", "two".into()));

        let got = ready(q.wait_form("f_6", Duration::from_secs(1)));
        assert!(q.list().is_empty(), "collected form leaves the queue");

        // The caller could not use them: put them back.
        q.restore(got);

        // A second waiter gets the same values, in the same order,
        // without parking and without the human doing anything.
        let again = ready(q.wait_form("f_6", Duration::from_millis(50)));
        let values: Vec<_> = again.iter().map(|(_, v)| v.as_str()).collect();
        assert_eq!(values, ["one", "two"]);
    }

    /// A waiter timing out must not take the form down with it.
    ///
    /// An MCP host caps a tool call (60s is common) far below the
    /// wait budget, so the first `vs_prompt_form_wait` regularly dies
    /// while the human is still typing into the form. When the timeout
    /// purged the entries, the human's submit landed on nothing and
    /// the agent's retry was told the form was "cancelled, timed out,
    /// or unknown" — for a form that was still live.
    #[test]
    fn form_survives_a_waiter_timeout_and_a_later_waiter_still_collects() {
        let q = PendingQueue::new();
        q.enqueue(entry("a", Some("f_5"), 0));
        q.enqueue(entry("b", Some("f_5"), 1));

        // First waiter gives up before the human submits.
        assert!(matches!(
            q.wait_form("f_5", Duration::from_millis(50)),
            FormWait::StillPending
        ));
        assert_eq!(
            q.list().len(),
            2,
            "a timed-out waiter must leave the form pending"
        );

        // The human submits; a second waiter collects everything.
        assert!(q.fulfill("a", "one".into()));
        assert!(q.fulfill("b", "two".into()));
        let got = ready(q.wait_form("f_5", Duration::from_secs(1)));
        let values: Vec<_> = got.iter().map(|(_, v)| v.as_str()).collect();
        assert_eq!(values, ["one", "two"]);
        assert!(q.list().is_empty(), "collected form must leave the queue");
    }
}
