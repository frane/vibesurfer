//! A drop from another application, delivered through the web view's
//! own drag-destination methods (`draggingEntered:`, `draggingUpdated:`,
//! `performDragOperation:`), the calls AppKit makes for a real drag.
//! The drag's data sits on a private pasteboard: file paths under the
//! filenames type WebKit reads for `DataTransfer.files`, and text, HTML
//! and URL strings under their pasteboard types. WebKit answers the
//! page's `dragover` asynchronously, so the run loop turns between steps.

use std::cell::Cell;
use std::time::Duration;

use objc2::rc::Retained;
use objc2::runtime::{AnyObject, Bool};
use objc2::{define_class, msg_send, AllocAnyThread, DefinedClass};
use objc2_foundation::{NSArray, NSObject, NSPoint, NSString};
use objc2_web_kit::WKWebView;

use super::eval::run_loop_until;
use crate::engine::{DropData, EngineError, EngineResult};

pub(super) struct InfoIvars {
    pasteboard: Retained<AnyObject>,
    window: *mut AnyObject,
    location: Cell<NSPoint>,
    valid_items: Cell<isize>,
    formation: Cell<isize>,
    sequence: isize,
}

define_class!(
    // NSDraggingInfo, as AppKit hands it to a drag destination.
    #[unsafe(super = NSObject)]
    #[name = "VsDraggingInfo"]
    #[ivars = InfoIvars]
    pub(super) struct DraggingInfo;

    impl DraggingInfo {
        #[unsafe(method(draggingDestinationWindow))]
        fn destination_window(&self) -> *mut AnyObject {
            self.ivars().window
        }
        #[unsafe(method(draggingSourceOperationMask))]
        fn source_operation_mask(&self) -> usize {
            // Copy | Link | Generic | Private | Move | Delete.
            1 | 2 | 4 | 8 | 16 | 32
        }
        #[unsafe(method(draggingLocation))]
        fn location(&self) -> NSPoint {
            self.ivars().location.get()
        }
        #[unsafe(method(draggedImageLocation))]
        fn image_location(&self) -> NSPoint {
            self.ivars().location.get()
        }
        #[unsafe(method(draggedImage))]
        fn image(&self) -> *mut AnyObject {
            std::ptr::null_mut()
        }
        #[unsafe(method(draggingPasteboard))]
        fn pasteboard(&self) -> *mut AnyObject {
            Retained::as_ptr(&self.ivars().pasteboard).cast_mut()
        }
        #[unsafe(method(draggingSource))]
        fn source(&self) -> *mut AnyObject {
            std::ptr::null_mut()
        }
        #[unsafe(method(draggingSequenceNumber))]
        fn sequence(&self) -> isize {
            self.ivars().sequence
        }
        #[unsafe(method(slideDraggedImageTo:))]
        fn slide(&self, _to: NSPoint) {}
        #[unsafe(method(namesOfPromisedFilesDroppedAtDestination:))]
        fn promised(&self, _url: *mut AnyObject) -> *mut AnyObject {
            std::ptr::null_mut()
        }
        #[unsafe(method(draggingFormation))]
        fn formation(&self) -> isize {
            self.ivars().formation.get()
        }
        #[unsafe(method(setDraggingFormation:))]
        fn set_formation(&self, f: isize) {
            self.ivars().formation.set(f);
        }
        #[unsafe(method(animatesToDestination))]
        fn animates(&self) -> Bool {
            Bool::NO
        }
        #[unsafe(method(setAnimatesToDestination:))]
        fn set_animates(&self, _a: Bool) {}
        #[unsafe(method(numberOfValidItemsForDrop))]
        fn valid_items(&self) -> isize {
            self.ivars().valid_items.get()
        }
        #[unsafe(method(setNumberOfValidItemsForDrop:))]
        fn set_valid_items(&self, n: isize) {
            self.ivars().valid_items.set(n);
        }
        #[unsafe(method(enumerateDraggingItemsWithOptions:forView:classes:searchOptions:usingBlock:))]
        fn enumerate(
            &self,
            _opts: usize,
            _view: *mut AnyObject,
            _classes: *mut AnyObject,
            _search: *mut AnyObject,
            _block: *mut AnyObject,
        ) {
        }
        #[unsafe(method(springLoadingHighlight))]
        fn spring_loading(&self) -> isize {
            0
        }
        #[unsafe(method(resetSpringLoading))]
        fn reset_spring_loading(&self) {}
    }
);

fn ns(s: &str) -> Retained<NSString> {
    NSString::from_str(s)
}

/// A uniquely named pasteboard holding `data`.
fn pasteboard_for(data: &DropData) -> Retained<AnyObject> {
    let pb: Retained<AnyObject> =
        unsafe { msg_send![objc2::class!(NSPasteboard), pasteboardWithUniqueName] };
    let mut types: Vec<Retained<NSString>> = Vec::new();
    let files: Vec<Retained<NSString>> = data
        .files
        .iter()
        .map(|p| {
            ns(&std::fs::canonicalize(p)
                .unwrap_or_else(|_| p.clone())
                .to_string_lossy())
        })
        .collect();
    if !files.is_empty() {
        types.push(ns("NSFilenamesPboardType"));
    }
    let mut strings: Vec<(Retained<NSString>, Retained<NSString>)> = Vec::new();
    // With files the page sees only `Files`; strings go by script.
    let items: &[(String, String)] = if files.is_empty() { &data.items } else { &[] };
    for (mime, value) in items {
        let ty = match mime.to_ascii_lowercase().as_str() {
            "text/plain" => "public.utf8-plain-text",
            "text/html" => "public.html",
            "text/uri-list" => "public.url",
            _ => continue,
        };
        // A uri-list may hold several lines and comments; the URL type holds one.
        let value = if ty == "public.url" {
            value
                .lines()
                .find(|l| !l.trim().is_empty() && !l.starts_with('#'))
                .unwrap_or("")
                .trim()
                .to_string()
        } else {
            value.clone()
        };
        types.push(ns(ty));
        strings.push((ns(ty), ns(&value)));
    }
    let types = NSArray::from_retained_slice(&types);
    unsafe {
        let _: isize =
            msg_send![&*pb, declareTypes: &*types, owner: std::ptr::null_mut::<AnyObject>()];
        if !files.is_empty() {
            let list = NSArray::from_retained_slice(&files);
            let _: Bool =
                msg_send![&*pb, setPropertyList: &*list, forType: &*ns("NSFilenamesPboardType")];
        }
        for (ty, value) in &strings {
            let _: Bool = msg_send![&*pb, setString: &**value, forType: &**ty];
        }
    }
    pb
}

/// Drop `data` at view point (`x`, `y`); `height` is the view's height
/// (window coordinates start at the bottom). Returns whether the page
/// accepted it.
pub(super) fn drop_data(
    web_view: &WKWebView,
    height: f64,
    x: f64,
    y: f64,
    data: &DropData,
) -> EngineResult<bool> {
    static SEQUENCE: std::sync::atomic::AtomicIsize = std::sync::atomic::AtomicIsize::new(1);
    let window: *mut AnyObject = unsafe { msg_send![web_view, window] };
    if window.is_null() {
        return Err(EngineError::Other(
            "drop: the web view has no window".into(),
        ));
    }
    let pasteboard = pasteboard_for(data);
    let width = web_view.frame().size.width;
    let at = NSPoint::new(
        x.clamp(0.5, (width - 0.5).max(0.5)),
        height - y.clamp(0.5, (height - 0.5).max(0.5)),
    );
    let items = isize::try_from(data.files.len().max(1)).unwrap_or(1);
    let info = DraggingInfo::alloc().set_ivars(InfoIvars {
        pasteboard: pasteboard.clone(),
        window,
        location: Cell::new(NSPoint::new(at.x, at.y + 4.0)),
        valid_items: Cell::new(items),
        formation: Cell::new(0),
        sequence: SEQUENCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
    });
    let info: Retained<DraggingInfo> = unsafe { msg_send![super(info), init] };
    let pump = |ms| {
        let _ = run_loop_until(|| false, Duration::from_millis(ms));
    };
    // Enter a few px off the point, then move onto it: the page sees
    // dragenter, then dragover where the drop lands.
    let _: usize = unsafe { msg_send![web_view, draggingEntered: &*info] };
    pump(30);
    info.ivars().location.set(at);
    let mut op: usize = 0;
    for _ in 0..4 {
        op = unsafe { msg_send![web_view, draggingUpdated: &*info] };
        pump(30);
    }
    // WebKit returns the operation the page's last dragover chose; ask
    // once more now that its answer is in.
    op = op.max(unsafe { msg_send![web_view, draggingUpdated: &*info] });
    if op == 0 {
        let _: () = unsafe { msg_send![web_view, draggingExited: &*info] };
        pump(30);
        clear(&pasteboard);
        return Ok(false);
    }
    let ready: Bool = unsafe { msg_send![web_view, prepareForDragOperation: &*info] };
    let done: Bool = if ready.as_bool() {
        unsafe { msg_send![web_view, performDragOperation: &*info] }
    } else {
        Bool::NO
    };
    let _: () = unsafe { msg_send![web_view, concludeDragOperation: &*info] };
    // The web process reads the pasteboard while it handles the drop.
    pump(200);
    clear(&pasteboard);
    Ok(done.as_bool())
}

fn clear(pasteboard: &AnyObject) {
    unsafe {
        let _: () = msg_send![pasteboard, releaseGlobally];
    }
}
