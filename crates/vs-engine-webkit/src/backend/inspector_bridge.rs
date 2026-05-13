//! Host-side decode of inspector_capture.js messages.
//!
//! Both the macOS WKWebView backend and the Linux WebKitGTK backend
//! inject `inspector_capture.js` and receive JSON-encoded events on
//! two channels: `vsConsole` and `vsNetwork`. This module decodes a
//! message body into the right [`crate::inspector`] type and pushes
//! into a buffer on the page.
//!
//! The decode is intentionally lenient — a malformed event is dropped
//! silently rather than crashing the host. Page JS is hostile by
//! default; the buffers are best-effort observability.

#![allow(
    clippy::cast_sign_loss,
    clippy::cast_possible_truncation,
    clippy::similar_names
)]

use std::collections::{HashMap, VecDeque};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::inspector::{
    ConsoleEntry, ConsoleLevel, Header, NetworkEntry, NetworkStatus, RequestDetail, RingBuffer,
};
use std::cell::RefCell;
use std::rc::Rc;

/// Shared inspector state for one page. Both the macOS and Linux
/// backends embed this in their per-page struct; the JS bridge
/// captures it via clone-of-Rc and pushes into the buffers from the
/// engine main thread.
///
/// Every buffer in here is bounded. A long-running page (SPA, chatty
/// fetch loop) cannot grow this state past `2 * capacity` entries
/// across all maps combined. v0.1.0 had unbounded `details` and
/// `pending`; that landed as a serious memory leak on real workloads
/// and is fixed here.
#[derive(Clone)]
pub struct InspectorSlots {
    pub console: Rc<RefCell<RingBuffer<ConsoleEntry>>>,
    pub network: Rc<RefCell<RingBuffer<NetworkEntry>>>,
    pub details: Rc<RefCell<RequestDetailStore>>,
    pub pending: Rc<RefCell<NetworkPending>>,
}

impl InspectorSlots {
    #[must_use]
    pub fn new(capacity: usize) -> Self {
        Self {
            console: Rc::new(RefCell::new(RingBuffer::new(capacity))),
            network: Rc::new(RefCell::new(RingBuffer::new(capacity))),
            details: Rc::new(RefCell::new(RequestDetailStore::new(capacity))),
            pending: Rc::new(RefCell::new(NetworkPending::new(capacity))),
        }
    }
}

/// Bounded request-detail store. The `vs inspect request <seq>`
/// primitive reads from here; entries are inserted on every network
/// `end` event in lockstep with [`NetworkEntry`] pushes into the
/// network ring buffer, and evicted in lockstep when that ring
/// buffer drops the oldest entry. Capacity matches the ring buffer's
/// so the two stores stay aligned.
pub struct RequestDetailStore {
    by_seq: HashMap<u64, RequestDetail>,
    /// Insertion order; oldest at front. FIFO-evicted when `by_seq`
    /// would exceed `capacity`.
    order: VecDeque<u64>,
    capacity: usize,
}

#[allow(clippy::map_entry)] // false positive: we evict an _unrelated_ key from the map first
impl RequestDetailStore {
    #[must_use]
    pub fn new(capacity: usize) -> Self {
        let capacity = capacity.max(1);
        Self {
            by_seq: HashMap::with_capacity(capacity),
            order: VecDeque::with_capacity(capacity),
            capacity,
        }
    }

    pub fn insert(&mut self, seq: u64, detail: RequestDetail) {
        if self.by_seq.contains_key(&seq) {
            // Duplicate-seq insert just overwrites; don't double-track.
            self.by_seq.insert(seq, detail);
            return;
        }
        if self.by_seq.len() == self.capacity {
            if let Some(old) = self.order.pop_front() {
                self.by_seq.remove(&old);
            }
        }
        self.order.push_back(seq);
        self.by_seq.insert(seq, detail);
    }

    #[cfg(test)]
    pub fn remove(&mut self, seq: u64) -> Option<RequestDetail> {
        let entry = self.by_seq.remove(&seq)?;
        if let Some(pos) = self.order.iter().position(|s| *s == seq) {
            self.order.remove(pos);
        }
        Some(entry)
    }

    #[must_use]
    pub fn get(&self, seq: u64) -> Option<&RequestDetail> {
        self.by_seq.get(&seq)
    }

    // Introspection used by tests; gated so production builds don't
    // pull these into the symbol table.
    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.by_seq.len()
    }

    #[cfg(test)]
    #[allow(dead_code)]
    pub fn is_empty(&self) -> bool {
        self.by_seq.is_empty()
    }

    #[cfg(test)]
    pub fn capacity(&self) -> usize {
        self.capacity
    }
}

pub const SCRIPT: &str = include_str!("inspector_capture.js");
pub const CONSOLE_HANDLER: &str = "vsConsole";
pub const NETWORK_HANDLER: &str = "vsNetwork";

fn ts_from_ms(ms: i64) -> SystemTime {
    if ms <= 0 {
        return UNIX_EPOCH;
    }
    UNIX_EPOCH + Duration::from_millis(ms as u64)
}

fn parse_level(s: &str) -> ConsoleLevel {
    match s {
        "error" => ConsoleLevel::Error,
        "warn" => ConsoleLevel::Warn,
        "info" => ConsoleLevel::Info,
        "debug" => ConsoleLevel::Debug,
        _ => ConsoleLevel::Log,
    }
}

/// Parse a `vsConsole` body and push into the buffer. No-op on
/// malformed input.
pub fn ingest_console(buf: &mut RingBuffer<ConsoleEntry>, body: &str) {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(body) else {
        return;
    };
    let level = parse_level(v.get("level").and_then(|x| x.as_str()).unwrap_or("log"));
    let message = v
        .get("message")
        .and_then(|x| x.as_str())
        .unwrap_or("")
        .to_string();
    let stack = v.get("stack").and_then(|x| x.as_str()).map(str::to_string);
    let ts = v
        .get("ts_ms")
        .and_then(serde_json::Value::as_i64)
        .map_or_else(SystemTime::now, ts_from_ms);
    buf.push(ConsoleEntry {
        timestamp: ts,
        level,
        message,
        stack,
    });
}

fn parse_headers(v: Option<&serde_json::Value>) -> Vec<Header> {
    let Some(arr) = v.and_then(serde_json::Value::as_array) else {
        return Vec::new();
    };
    arr.iter()
        .filter_map(|pair| {
            let p = pair.as_array()?;
            let name = p.first()?.as_str()?.to_string();
            let value = p.get(1)?.as_str()?.to_string();
            Some(Header { name, value })
        })
        .collect()
}

/// In-flight network state keyed by seq, used to fold a `start` and a
/// later `end` event into one `NetworkEntry` + one `RequestDetail`.
///
/// Bounded by `capacity`: requests that start but never end (page
/// navigated away mid-flight, fetch aborted by a Promise rejection
/// the JS shim didn't observe) eventually get evicted FIFO. v0.1.0
/// had unbounded `HashMap`s here and leaked.
pub struct NetworkPending {
    by_seq: HashMap<u64, PendingEntry>,
    /// Insertion order of currently-tracked seqs; oldest at front.
    order: VecDeque<u64>,
    capacity: usize,
}

#[derive(Clone)]
pub struct PendingEntry {
    pub start_ms: i64,
    pub req_headers: Vec<Header>,
    pub req_body: Option<String>,
}

#[allow(clippy::map_entry)] // false positive: we evict an _unrelated_ key from the map first
impl NetworkPending {
    #[must_use]
    pub fn new(capacity: usize) -> Self {
        let capacity = capacity.max(1);
        Self {
            by_seq: HashMap::with_capacity(capacity),
            order: VecDeque::with_capacity(capacity),
            capacity,
        }
    }

    pub fn insert(&mut self, seq: u64, entry: PendingEntry) {
        if self.by_seq.contains_key(&seq) {
            // A duplicate `start` for the same seq (engine-shim race
            // or replay) just overwrites; don't double-track in
            // `order`.
            self.by_seq.insert(seq, entry);
            return;
        }
        if self.by_seq.len() == self.capacity {
            if let Some(old) = self.order.pop_front() {
                self.by_seq.remove(&old);
            }
        }
        self.order.push_back(seq);
        self.by_seq.insert(seq, entry);
    }

    /// Remove and return the entry for `seq`, scrubbing it from
    /// `order` as well. O(n) on the order deque but n is bounded by
    /// `capacity`.
    pub fn take(&mut self, seq: u64) -> Option<PendingEntry> {
        let entry = self.by_seq.remove(&seq)?;
        if let Some(pos) = self.order.iter().position(|s| *s == seq) {
            self.order.remove(pos);
        }
        Some(entry)
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.by_seq.len()
    }

    #[cfg(test)]
    pub fn is_empty(&self) -> bool {
        self.by_seq.is_empty()
    }

    #[cfg(test)]
    pub fn capacity(&self) -> usize {
        self.capacity
    }
}

pub struct NetworkIngestSlot<'a> {
    pub entries: &'a mut RingBuffer<NetworkEntry>,
    pub details: &'a mut RequestDetailStore,
    pub pending: &'a mut NetworkPending,
}

/// Parse a `vsNetwork` body and update buffers. Two phases:
/// `start` records the request side; `end` finalizes status/latency
/// and writes the [`NetworkEntry`] + [`RequestDetail`].
#[allow(clippy::needless_pass_by_value)] // slot bundles &mut borrows; moving is intentional
pub fn ingest_network(slot: NetworkIngestSlot<'_>, body: &str) {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(body) else {
        return;
    };
    let Some(seq) = v.get("seq").and_then(serde_json::Value::as_u64) else {
        return;
    };
    let phase = v.get("phase").and_then(|x| x.as_str()).unwrap_or("");

    if phase == "start" {
        let ts = v
            .get("ts_ms")
            .and_then(serde_json::Value::as_i64)
            .unwrap_or(0);
        slot.pending.insert(
            seq,
            PendingEntry {
                start_ms: ts,
                req_headers: parse_headers(v.get("req_headers")),
                req_body: v
                    .get("req_body")
                    .and_then(|x| x.as_str())
                    .map(str::to_string),
            },
        );
        return;
    }
    if phase != "end" {
        return;
    }

    let method = v
        .get("method")
        .and_then(|x| x.as_str())
        .unwrap_or("GET")
        .to_string();
    let url = v
        .get("url")
        .and_then(|x| x.as_str())
        .unwrap_or("")
        .to_string();
    let status_code = v
        .get("status")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0);
    let status = if status_code == 0 {
        NetworkStatus::Abort
    } else {
        NetworkStatus::Code(status_code as u16)
    };
    let end_ms = v
        .get("ts_ms")
        .and_then(serde_json::Value::as_i64)
        .unwrap_or(0);
    let pending = slot.pending.take(seq);
    let start_ms = pending.as_ref().map_or(end_ms, |p| p.start_ms);
    let latency = if end_ms >= start_ms {
        Some((end_ms - start_ms) as u64)
    } else {
        None
    };
    let size = v
        .get("size")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0);
    let timestamp = ts_from_ms(end_ms);

    // `entries` is the ring buffer (bounded by capacity). `details`
    // self-bounds at the same capacity via FIFO eviction. Both pushes
    // here grow the two stores in lockstep so a `vs inspect request`
    // lookup never resolves to a seq that's been evicted from
    // entries (or vice versa) except across a single transient gap
    // at eviction time.
    let _evicted = slot.entries.push(NetworkEntry {
        seq,
        timestamp,
        method: method.clone(),
        url: url.clone(),
        status: status.clone(),
        size,
        latency_ms: latency,
    });

    let (req_headers, req_body) = pending
        .map(|p| (p.req_headers, p.req_body))
        .unwrap_or_default();
    let res_headers = parse_headers(v.get("res_headers"));
    let res_body = v
        .get("res_body")
        .and_then(|x| x.as_str())
        .map(str::to_string);

    slot.details.insert(
        seq,
        RequestDetail {
            seq,
            method,
            url,
            status,
            request_headers: req_headers,
            request_body: req_body,
            response_headers: res_headers,
            response_body: res_body,
        },
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::inspector::DEFAULT_BUFFER_CAPACITY;

    fn make_slots(cap: usize) -> InspectorSlots {
        InspectorSlots::new(cap)
    }

    /// Helper: emit start+end events for a fake request seq.
    fn ingest_pair(slots: &InspectorSlots, seq: u64) {
        let start = format!(
            r#"{{"seq":{seq},"phase":"start","ts_ms":1000,"req_headers":[["X-Test","y"]],"req_body":"hi"}}"#
        );
        let end = format!(
            r#"{{"seq":{seq},"phase":"end","ts_ms":1100,"method":"GET","url":"http://x/{seq}","status":200,"size":42}}"#
        );
        {
            let mut entries = slots.network.borrow_mut();
            let mut details = slots.details.borrow_mut();
            let mut pending = slots.pending.borrow_mut();
            ingest_network(
                NetworkIngestSlot {
                    entries: &mut entries,
                    details: &mut details,
                    pending: &mut pending,
                },
                &start,
            );
        }
        {
            let mut entries = slots.network.borrow_mut();
            let mut details = slots.details.borrow_mut();
            let mut pending = slots.pending.borrow_mut();
            ingest_network(
                NetworkIngestSlot {
                    entries: &mut entries,
                    details: &mut details,
                    pending: &mut pending,
                },
                &end,
            );
        }
    }

    #[test]
    fn details_store_evicts_oldest_at_capacity() {
        let slots = make_slots(3);
        for seq in 1..=5 {
            ingest_pair(&slots, seq);
        }
        let details = slots.details.borrow();
        assert_eq!(details.len(), 3, "details store must stay bounded");
        assert_eq!(details.capacity(), 3);
        assert!(
            details.get(1).is_none(),
            "oldest seq 1 should have been evicted"
        );
        assert!(
            details.get(2).is_none(),
            "second-oldest seq 2 should have been evicted"
        );
        assert!(details.get(3).is_some(), "seq 3 still in window");
        assert!(details.get(5).is_some(), "seq 5 still in window");
    }

    #[test]
    fn pending_evicts_oldest_when_starts_pile_up_without_ends() {
        // start-only events (no matching end) — simulates page that
        // navigates away mid-fetch, or fetch that throws.
        let slots = make_slots(3);
        for seq in 1..=5 {
            let body = format!(
                r#"{{"seq":{seq},"phase":"start","ts_ms":{},"req_headers":[],"req_body":null}}"#,
                seq * 100
            );
            let mut entries = slots.network.borrow_mut();
            let mut details = slots.details.borrow_mut();
            let mut pending = slots.pending.borrow_mut();
            ingest_network(
                NetworkIngestSlot {
                    entries: &mut entries,
                    details: &mut details,
                    pending: &mut pending,
                },
                &body,
            );
        }
        let pending = slots.pending.borrow();
        assert_eq!(pending.len(), 3, "pending must stay bounded by capacity");
        assert_eq!(pending.capacity(), 3);
        assert!(!pending.is_empty());
    }

    #[test]
    fn end_event_drains_pending_back_to_empty() {
        let slots = make_slots(DEFAULT_BUFFER_CAPACITY);
        ingest_pair(&slots, 42);
        assert!(slots.pending.borrow().is_empty());
        let details = slots.details.borrow();
        assert_eq!(details.len(), 1);
        let detail = details.get(42).expect("seq 42 present");
        assert_eq!(detail.method, "GET");
        assert_eq!(detail.url, "http://x/42");
    }

    #[test]
    fn details_remove_clears_order_too() {
        let mut store = RequestDetailStore::new(4);
        for seq in 1..=3 {
            store.insert(
                seq,
                RequestDetail {
                    seq,
                    method: "GET".into(),
                    url: format!("http://x/{seq}"),
                    status: NetworkStatus::Code(200),
                    request_headers: vec![],
                    request_body: None,
                    response_headers: vec![],
                    response_body: None,
                },
            );
        }
        assert_eq!(store.len(), 3);
        store.remove(2);
        assert_eq!(store.len(), 2);
        // Push a 4th and 5th: with capacity 4 and one removed, both
        // should fit without evicting the rest.
        for seq in 4..=5 {
            store.insert(
                seq,
                RequestDetail {
                    seq,
                    method: "GET".into(),
                    url: format!("http://x/{seq}"),
                    status: NetworkStatus::Code(200),
                    request_headers: vec![],
                    request_body: None,
                    response_headers: vec![],
                    response_body: None,
                },
            );
        }
        assert_eq!(store.len(), 4);
        assert!(store.get(1).is_some(), "seq 1 should not have been evicted");
        assert!(store.get(2).is_none(), "seq 2 was removed");
        assert!(store.get(5).is_some());
    }

    #[test]
    fn console_ring_buffer_evicts_at_capacity() {
        let slots = make_slots(3);
        for i in 0..5 {
            let body = format!(r#"{{"level":"log","message":"m{i}","ts_ms":{}}}"#, 1000 + i);
            let mut buf = slots.console.borrow_mut();
            ingest_console(&mut buf, &body);
        }
        let buf = slots.console.borrow();
        let snap = buf.snapshot();
        assert_eq!(snap.len(), 3);
        assert_eq!(snap[0].message, "m2", "oldest two should have been evicted");
        assert_eq!(snap[2].message, "m4");
    }
}
