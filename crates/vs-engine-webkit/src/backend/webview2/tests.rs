//! Real-WebView2 checks of page lifetime. Run by the Windows
//! engine-tests job.

use windows::core::{HSTRING, PCWSTR};
use windows::Win32::Foundation::HWND;
use windows::Win32::UI::WindowsAndMessaging::{FindWindowExW, IsWindow, HWND_MESSAGE};

use super::{new_host_hwnd, Webview2Backend};
use crate::engine::{DataStore, Engine};

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
