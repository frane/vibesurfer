//! `capture` for the Cocoa backend: WKWebView snapshot →
//! NSImage → CGImage → NSBitmapImageRep → PNG → on-disk file.

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::Duration;

use block2::RcBlock;
use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::{AllocAnyThread, MainThreadMarker};
use objc2_app_kit::{NSBitmapImageFileType, NSBitmapImageRep, NSImage, NSImageCacheMode};
use objc2_foundation::{NSData, NSDictionary, NSError};
use objc2_web_kit::{WKSnapshotConfiguration, WKWebView};

use super::eval::run_loop_until;
use crate::engine::{EngineError, EngineResult, PageHandle};

pub(super) fn capture_to_png(
    web_view: &WKWebView,
    page: PageHandle,
    captures_dir: Option<&Path>,
    mtm: MainThreadMarker,
    snapshot_width: Option<f64>,
) -> EngineResult<PathBuf> {
    // Wrap the whole capture in a dedicated autorelease pool. Each
    // snapshot builds a large *uncompressed* TIFF (`TIFFRepresentation`,
    // ~w*h*4 bytes, tens of MB at retina) plus a PNG NSData, both
    // autoreleased. Under recording this runs many times a second; the
    // serve loop's per-iteration pool doesn't drain between captures, so
    // those buffers piled up (~100 MB/s leak, measured). Draining per
    // capture keeps recording memory flat.
    objc2::rc::autoreleasepool(|_| {
        capture_to_png_inner(web_view, page, captures_dir, mtm, snapshot_width)
    })
}

fn capture_to_png_inner(
    web_view: &WKWebView,
    page: PageHandle,
    captures_dir: Option<&Path>,
    mtm: MainThreadMarker,
    snapshot_width: Option<f64>,
) -> EngineResult<PathBuf> {
    let slot: Rc<RefCell<Option<Result<Retained<NSImage>, String>>>> = Rc::new(RefCell::new(None));
    let slot_for_block = slot.clone();
    let block = RcBlock::new(move |image: *mut NSImage, error: *mut NSError| {
        if !error.is_null() {
            let err = unsafe { &*error };
            *slot_for_block.borrow_mut() = Some(Err(err.localizedDescription().to_string()));
            return;
        }
        if image.is_null() {
            *slot_for_block.borrow_mut() = Some(Err("null NSImage".into()));
            return;
        }
        let img: Retained<NSImage> = unsafe { Retained::retain(image).expect("non-null NSImage") };
        *slot_for_block.borrow_mut() = Some(Ok(img));
    });

    // `afterScreenUpdates` defaults to YES, which makes the snapshot
    // wait for the next *on-screen* rendering update before firing the
    // completion handler. Our WKWebView is hosted in an offscreen
    // NSWindow that is never ordered on-screen, so no screen update is
    // ever scheduled and the handler can wedge until the timeout (seen
    // under sequential automation; isolated runs occasionally win the
    // race against a pending paint). Setting it to NO captures the
    // currently-rendered layer tree immediately, which is exactly what
    // a headless snapshot wants.
    let config = unsafe { WKSnapshotConfiguration::new(mtm) };
    unsafe { config.setAfterScreenUpdates(false) };
    // For live frames (watch / record) WebKit renders the snapshot at
    // this width instead of the full device backing scale — cheaper to
    // produce and encode. Screenshots pass None to keep full resolution.
    if let Some(w) = snapshot_width {
        unsafe { config.setSnapshotWidth(Some(&objc2_foundation::NSNumber::new_f64(w))) };
    }

    unsafe {
        web_view.takeSnapshotWithConfiguration_completionHandler(Some(&config), &block);
    }

    let slot_check = slot.clone();
    let ok = run_loop_until(
        move || slot_check.borrow().is_some(),
        Duration::from_secs(10),
    );
    if !ok {
        return Err(EngineError::Timeout {
            budget: Duration::from_secs(10),
            primitive: "capture",
        });
    }
    let result = slot.borrow_mut().take();
    let image = match result {
        Some(Ok(img)) => img,
        Some(Err(msg)) => return Err(EngineError::Other(format!("snapshot failed: {msg}"))),
        None => unreachable!(),
    };

    image.setCacheMode(NSImageCacheMode::Never);

    // NSImage → CGImage → NSBitmapImageRep → PNG. The older
    // TIFFRepresentation path leaked a full uncompressed bitmap
    // (~16 MB) per frame under recording — the rendered TIFF buffer was
    // never reclaimed. Rendering to a CGImage once and wrapping it
    // directly keeps memory flat.
    let cg =
        unsafe { image.CGImageForProposedRect_context_hints(std::ptr::null_mut(), None, None) }
            .ok_or_else(|| EngineError::Other("snapshot has no CGImage".into()))?;
    let bitmap = NSBitmapImageRep::initWithCGImage(NSBitmapImageRep::alloc(), &cg);
    let empty: Retained<NSDictionary<objc2_foundation::NSString, AnyObject>> = NSDictionary::new();
    let png_data: Retained<NSData> =
        unsafe { bitmap.representationUsingType_properties(NSBitmapImageFileType::PNG, &empty) }
            .ok_or_else(|| EngineError::Other("PNG encoding returned nil".into()))?;

    let dir = captures_dir
        .map(Path::to_path_buf)
        .unwrap_or_else(|| std::env::temp_dir().join("vibesurfer-webkit-captures"));
    std::fs::create_dir_all(&dir).map_err(|e| EngineError::Other(e.to_string()))?;
    let path = dir.join(format!(
        "wk-{}-{}.png",
        page.0,
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0)
    ));
    let bytes: &[u8] = unsafe { png_data.as_bytes_unchecked() };
    std::fs::write(&path, bytes).map_err(|e| EngineError::Other(e.to_string()))?;
    Ok(path)
}
