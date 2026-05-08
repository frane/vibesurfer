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

use std::collections::HashMap;
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
#[derive(Clone)]
pub struct InspectorSlots {
    pub console: Rc<RefCell<RingBuffer<ConsoleEntry>>>,
    pub network: Rc<RefCell<RingBuffer<NetworkEntry>>>,
    pub details: Rc<RefCell<HashMap<u64, RequestDetail>>>,
    pub pending: Rc<RefCell<NetworkPending>>,
}

impl InspectorSlots {
    #[must_use]
    pub fn new(capacity: usize) -> Self {
        Self {
            console: Rc::new(RefCell::new(RingBuffer::new(capacity))),
            network: Rc::new(RefCell::new(RingBuffer::new(capacity))),
            details: Rc::new(RefCell::new(HashMap::new())),
            pending: Rc::new(RefCell::new(NetworkPending::default())),
        }
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
#[derive(Default)]
pub struct NetworkPending {
    pub start_ms: HashMap<u64, i64>,
    pub req_headers: HashMap<u64, Vec<Header>>,
    pub req_body: HashMap<u64, Option<String>>,
}

pub struct NetworkIngestSlot<'a> {
    pub entries: &'a mut RingBuffer<NetworkEntry>,
    pub details: &'a mut HashMap<u64, RequestDetail>,
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
        slot.pending.start_ms.insert(seq, ts);
        slot.pending
            .req_headers
            .insert(seq, parse_headers(v.get("req_headers")));
        slot.pending.req_body.insert(
            seq,
            v.get("req_body")
                .and_then(|x| x.as_str())
                .map(str::to_string),
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
    let start_ms = slot.pending.start_ms.remove(&seq).unwrap_or(end_ms);
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

    slot.entries.push(NetworkEntry {
        seq,
        timestamp,
        method: method.clone(),
        url: url.clone(),
        status: status.clone(),
        size,
        latency_ms: latency,
    });

    let req_headers = slot.pending.req_headers.remove(&seq).unwrap_or_default();
    let req_body = slot.pending.req_body.remove(&seq).flatten();
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
