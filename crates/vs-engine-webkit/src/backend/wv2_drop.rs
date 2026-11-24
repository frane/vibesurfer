//! A drop from another application on WebView2: the composition
//! controller's `DragEnter`, `DragOver` and `Drop`, the calls a host
//! makes when an OLE drag reaches the web view. The data rides in a
//! shell data object: files as `CF_HDROP`, text as `CF_UNICODETEXT`, a
//! URL as `UniformResourceLocatorW`, HTML as `HTML Format`.

use std::mem::ManuallyDrop;
use std::time::Duration;

use webview2_com::Microsoft::Web::WebView2::Win32::{
    ICoreWebView2CompositionController, ICoreWebView2CompositionController3,
};
use windows::core::{Interface, PCWSTR};
use windows::Win32::Foundation::{HGLOBAL, POINT};
use windows::Win32::System::Com::{
    IDataObject, DVASPECT_CONTENT, FORMATETC, STGMEDIUM, STGMEDIUM_0, TYMED_HGLOBAL,
};
use windows::Win32::System::DataExchange::RegisterClipboardFormatW;
use windows::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE};
use windows::Win32::System::Ole::{CF_HDROP, CF_UNICODETEXT};
use windows::Win32::UI::Shell::{SHCreateDataObject, DROPFILES};
use windows::Win32::UI::WindowsAndMessaging::{DispatchMessageW, PeekMessageW, MSG, PM_REMOVE};

use crate::engine::{DropData, EngineError, EngineResult};

/// MK_LBUTTON: the drag holds the left button.
const LEFT_BUTTON: u32 = 1;
/// DROPEFFECT_COPY | DROPEFFECT_MOVE | DROPEFFECT_LINK.
const ANY_EFFECT: u32 = 1 | 2 | 4;

fn err(what: &str, e: impl std::fmt::Display) -> EngineError {
    EngineError::Other(format!("drop: {what}: {e}"))
}

fn pump(ms: u64) {
    let until = std::time::Instant::now() + Duration::from_millis(ms);
    while std::time::Instant::now() < until {
        let mut msg = MSG::default();
        unsafe {
            while PeekMessageW(&raw mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                DispatchMessageW(&raw const msg);
            }
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Movable global memory holding `bytes`, for a `STGMEDIUM`.
fn hglobal(bytes: &[u8]) -> EngineResult<HGLOBAL> {
    unsafe {
        let h =
            GlobalAlloc(GMEM_MOVEABLE, bytes.len().max(1)).map_err(|e| err("GlobalAlloc", e))?;
        let p = GlobalLock(h).cast::<u8>();
        if p.is_null() {
            return Err(err("GlobalLock", "null"));
        }
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), p, bytes.len());
        let _ = GlobalUnlock(h);
        Ok(h)
    }
}

fn set(obj: &IDataObject, format: u16, bytes: &[u8]) -> EngineResult<()> {
    let fmt = FORMATETC {
        cfFormat: format,
        ptd: std::ptr::null_mut(),
        dwAspect: DVASPECT_CONTENT.0,
        lindex: -1,
        tymed: TYMED_HGLOBAL.0 as u32,
    };
    let medium = STGMEDIUM {
        tymed: TYMED_HGLOBAL.0 as u32,
        u: STGMEDIUM_0 {
            hGlobal: hglobal(bytes)?,
        },
        pUnkForRelease: ManuallyDrop::new(None),
    };
    // fRelease: the data object owns the memory from here on.
    unsafe { obj.SetData(&raw const fmt, &raw const medium, true) }.map_err(|e| err("SetData", e))
}

fn registered(name: &str) -> u16 {
    let w = wide(name);
    // Registered formats are 0xC000..=0xFFFF.
    u16::try_from(unsafe { RegisterClipboardFormatW(PCWSTR(w.as_ptr())) }).unwrap_or(0)
}

/// `CF_HDROP`: a DROPFILES header, then NUL-terminated wide paths and
/// one more NUL.
fn hdrop(files: &[std::path::PathBuf]) -> Vec<u8> {
    let header = std::mem::size_of::<DROPFILES>();
    let mut out = vec![0u8; header];
    let df = DROPFILES {
        pFiles: u32::try_from(header).unwrap_or(20),
        pt: POINT::default(),
        fNC: false.into(),
        fWide: true.into(),
    };
    unsafe { std::ptr::write_unaligned(out.as_mut_ptr().cast::<DROPFILES>(), df) };
    for f in files {
        let path = std::fs::canonicalize(f).unwrap_or_else(|_| f.clone());
        // canonicalize gives `\\?\C:\...`; Explorer hands out plain paths.
        let s = path
            .to_string_lossy()
            .trim_start_matches(r"\\?\")
            .to_string();
        for u in wide(&s) {
            out.extend_from_slice(&u.to_le_bytes());
        }
    }
    out.extend_from_slice(&[0, 0]);
    out
}

/// `HTML Format`: a header of byte offsets around the fragment.
fn cf_html(fragment: &str) -> Vec<u8> {
    const HEAD: &str = "Version:0.9\r\nStartHTML:0000000000\r\nEndHTML:0000000000\r\nStartFragment:0000000000\r\nEndFragment:0000000000\r\n";
    let pre = "<html><body><!--StartFragment-->";
    let post = "<!--EndFragment--></body></html>";
    let start_html = HEAD.len();
    let start_frag = start_html + pre.len();
    let end_frag = start_frag + fragment.len();
    let end_html = end_frag + post.len();
    format!(
        "Version:0.9\r\nStartHTML:{start_html:010}\r\nEndHTML:{end_html:010}\r\nStartFragment:{start_frag:010}\r\nEndFragment:{end_frag:010}\r\n{pre}{fragment}{post}\0"
    )
    .into_bytes()
}

fn data_object(data: &DropData) -> EngineResult<IDataObject> {
    let obj: IDataObject = unsafe { SHCreateDataObject(None, None, None) }
        .map_err(|e| err("SHCreateDataObject", e))?;
    if !data.files.is_empty() {
        set(&obj, CF_HDROP.0, &hdrop(&data.files))?;
        // With files the page sees only `Files`; strings go by script.
        return Ok(obj);
    }
    for (mime, value) in &data.items {
        match mime.to_ascii_lowercase().as_str() {
            "text/plain" => {
                let bytes: Vec<u8> = wide(value).iter().flat_map(|u| u.to_le_bytes()).collect();
                set(&obj, CF_UNICODETEXT.0, &bytes)?;
            }
            "text/uri-list" => {
                let url = value
                    .lines()
                    .find(|l| !l.trim().is_empty() && !l.starts_with('#'))
                    .unwrap_or("")
                    .trim();
                let bytes: Vec<u8> = wide(url).iter().flat_map(|u| u.to_le_bytes()).collect();
                set(&obj, registered("UniformResourceLocatorW"), &bytes)?;
            }
            "text/html" => set(&obj, registered("HTML Format"), &cf_html(value))?,
            _ => {}
        }
    }
    Ok(obj)
}

/// Drop `data` at `at` (view client coordinates, as `SendMouseInput`
/// takes them). WebView2 learns the page's answer to `dragover`
/// asynchronously and reports no effect from these calls, so the drop
/// always goes ahead and Chromium decides: the page gets `drop` only if
/// its `dragover` accepted. Whether it did is read from the page.
pub(super) fn drop_data(
    comp: &ICoreWebView2CompositionController,
    at: POINT,
    data: &DropData,
) -> EngineResult<()> {
    let comp3: ICoreWebView2CompositionController3 = comp
        .cast()
        .map_err(|e| err("ICoreWebView2CompositionController3", e))?;
    let obj = data_object(data)?;
    let near = POINT {
        x: at.x,
        y: at.y + 4,
    };
    let mut effect = ANY_EFFECT;
    unsafe { comp3.DragEnter(&obj, LEFT_BUTTON, near, &raw mut effect) }
        .map_err(|e| err("DragEnter", e))?;
    pump(30);
    for _ in 0..5 {
        effect = ANY_EFFECT;
        unsafe { comp3.DragOver(LEFT_BUTTON, at, &raw mut effect) }
            .map_err(|e| err("DragOver", e))?;
        pump(40);
    }
    effect = ANY_EFFECT;
    unsafe { comp3.Drop(&obj, 0, at, &raw mut effect) }.map_err(|e| err("Drop", e))?;
    pump(200);
    Ok(())
}
