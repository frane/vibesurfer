//! `capture` for the Cocoa backend: WKWebView snapshot →
//! NSImage → TIFF → NSBitmapImageRep → PNG → on-disk file.

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::Duration;

use block2::RcBlock;
use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::MainThreadMarker;
use objc2_app_kit::{NSBitmapImageFileType, NSBitmapImageRep, NSImage};
use objc2_foundation::{NSData, NSDictionary, NSError};
use objc2_web_kit::{WKSnapshotConfiguration, WKWebView};

use super::eval::run_loop_until;
use crate::engine::{EngineError, EngineResult, PageHandle};

pub(super) fn capture_to_png(
    web_view: &WKWebView,
    page: PageHandle,
    captures_dir: Option<&Path>,
    mtm: MainThreadMarker,
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

    // NSImage → TIFF → NSBitmapImageRep → PNG.
    let tiff = image
        .TIFFRepresentation()
        .ok_or_else(|| EngineError::Other("NSImage has no TIFF representation".into()))?;
    let bitmap = NSBitmapImageRep::imageRepWithData(&tiff)
        .ok_or_else(|| EngineError::Other("imageRepWithData returned nil".into()))?;
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
