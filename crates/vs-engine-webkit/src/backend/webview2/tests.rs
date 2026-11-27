//! Real-WebView2 checks of page lifetime. Run by the Windows
//! engine-tests job.

use windows::core::{HSTRING, PCWSTR};
use windows::Win32::Foundation::HWND;
use windows::Win32::UI::WindowsAndMessaging::{FindWindowExW, IsWindow, HWND_MESSAGE};

use std::net::TcpListener;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use super::wait::pump_until;
use super::{new_host_hwnd, Webview2Backend};
use crate::engine::{DataStore, Engine, EngineError};

/// Message-only windows of the class every page's host window uses.
fn host_windows() -> usize {
    let class = HSTRING::from("vs-webview2-host");
    let mut n = 0;
    let mut after: Option<HWND> = None;
    // SAFETY: plain window enumeration on this thread.
    while let Ok(h) = unsafe { FindWindowExW(Some(HWND_MESSAGE), after, &class, PCWSTR::null()) } {
        if h.is_invalid() {
            break;
        }
        n += 1;
        after = Some(h);
    }
    n
}

#[test]
fn dropping_a_host_window_destroys_it() {
    let host = new_host_hwnd().expect("host window");
    let hwnd = host.hwnd();
    // SAFETY: IsWindow accepts any handle value.
    assert!(unsafe { IsWindow(Some(hwnd)) }.as_bool());
    drop(host);
    assert!(!unsafe { IsWindow(Some(hwnd)) }.as_bool());
}

#[test]
fn closing_pages_leaves_no_host_window() {
    let mut engine = Webview2Backend::new();
    let before = host_windows();
    for _ in 0..3 {
        let page = engine
            .open("about:blank", &DataStore::Shared)
            .expect("open");
        assert_eq!(host_windows(), before + 1, "one host window per open page");
        engine.close(page).expect("close");
    }
    assert_eq!(
        host_windows(),
        before,
        "closed pages left host windows behind"
    );
}

#[test]
fn a_completion_that_never_comes_times_out() {
    let (_tx, rx) = mpsc::channel::<()>();
    let started = Instant::now();
    let got = pump_until(&rx, Duration::from_millis(300), "test", "never sent");
    assert!(matches!(got, Err(EngineError::Timeout { .. })), "{got:?}");
    assert!(started.elapsed() < Duration::from_secs(5));
}

/// A server that accepts and never answers: the navigation neither
/// commits nor completes. `open` used to wait for it forever, and every
/// later call to the daemon queued behind it.
#[test]
fn open_on_a_server_that_never_answers_times_out() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let url = format!("http://{}/", listener.local_addr().expect("addr"));
    // Keep accepted connections open without answering.
    std::thread::spawn(move || {
        let held: Vec<_> = listener.incoming().collect();
        drop(held);
    });
    std::env::set_var("VS_NAV_BUDGET_MS", "2000");
    let mut engine = Webview2Backend::new();
    let windows_before = host_windows();
    let started = Instant::now();
    let got = engine.open(&url, &DataStore::Shared);
    std::env::remove_var("VS_NAV_BUDGET_MS");
    assert!(
        matches!(got, Err(EngineError::Timeout { .. })),
        "{:?}",
        got.map(|_| ())
    );
    // The browser process start is not charged to the 2 s budget.
    assert!(started.elapsed() < Duration::from_secs(60));
    assert_eq!(
        host_windows(),
        windows_before,
        "the failed page left its window"
    );
}

#[test]
fn pages_share_one_environment() {
    let mut engine = Webview2Backend::new();
    let started = Instant::now();
    let a = engine
        .open("about:blank", &DataStore::Shared)
        .expect("open");
    let first = started.elapsed();
    let again = Instant::now();
    let b = engine
        .open("about:blank", &DataStore::Isolated("s1".into()))
        .expect("open isolated");
    eprintln!("open: first {first:?}, second {:?}", again.elapsed());
    assert_eq!(
        engine.environments.len(),
        1,
        "isolation is a profile, not a browser"
    );
    engine.close(a).expect("close");
    engine.close(b).expect("close");
    engine.drop_store("s1").expect("drop_store");
    assert_eq!(engine.environments.len(), 1, "the shared environment stays");
}
