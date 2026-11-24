//! Exact wheel deltas on WebKitGTK under X11.
//!
//! XTest has no smooth-scroll axis: a wheel turn arrives as clicks of
//! buttons 4-7, and WebKit scrolls each click by `floor(size^(2/3))`
//! px (86 for an 800 px high view). WebKit's GTK scroll handler takes
//! the click count from the `scroll` signal's arguments and the rest
//! (position, modifiers, time) from the real event being dispatched.
//!
//! So each web view's scroll controller gets a handler of ours in front
//! of WebKit's (WebKit's is blocked and called from ours): while one
//! wheel turn is pending, the next click's signal is re-emitted to
//! WebKit with `delta / step` instead of `1`, during the real event,
//! and the page sees one trusted `wheel` with the exact delta. Every
//! other scroll passes through unchanged.

#![allow(unsafe_code)]

use std::cell::Cell;

use glib::prelude::*;
use glib::translate::ToGlibPtr;
use gtk4 as gtk;
use gtk4::prelude::*;

thread_local! {
    /// The wheel turn the next scroll click stands for, in clicks of
    /// WebKit's step: (dx, dy).
    static PENDING: Cell<Option<(f64, f64)>> = const { Cell::new(None) };
}

/// Make the next wheel click on any web view stand for `(dx, dy)` CSS px,
/// for a view of `size` (width, height) px.
pub(super) fn arm(dx: f64, dy: f64, size: (i32, i32)) {
    let step = |s: i32| f64::from(s.max(1)).powf(2.0 / 3.0).floor().max(1.0);
    PENDING.with(|p| p.set(Some((dx / step(size.0), dy / step(size.1)))));
}

/// Forget an armed turn no click picked up.
pub(super) fn disarm() {
    PENDING.with(|p| p.set(None));
}

/// Put our handler in front of WebKit's on `web_view`'s scroll controller.
pub(super) fn install(web_view: &impl IsA<gtk::Widget>) {
    let model = web_view.as_ref().observe_controllers();
    let Some(ctrl) = (0..model.n_items())
        .filter_map(|i| model.item(i))
        .find_map(|o| o.downcast::<gtk::EventControllerScroll>().ok())
    else {
        return;
    };
    let instance: *mut glib::gobject_ffi::GObject =
        ctrl.upcast_ref::<glib::Object>().to_glib_none().0;
    let signal = unsafe {
        glib::gobject_ffi::g_signal_lookup(
            c"scroll".as_ptr(),
            glib::translate::IntoGlib::into_glib(gtk::EventControllerScroll::static_type()),
        )
    };
    if signal == 0 {
        return;
    }
    // WebKit's handler: the one already connected to `scroll`.
    let theirs = unsafe {
        glib::gobject_ffi::g_signal_handler_find(
            instance,
            glib::gobject_ffi::G_SIGNAL_MATCH_ID,
            signal,
            0,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };
    if theirs == 0 {
        return;
    }
    unsafe { glib::gobject_ffi::g_signal_handler_block(instance, theirs) };
    let ours = std::rc::Rc::new(Cell::new(0_u64));
    let ours_in = ours.clone();
    let id = ctrl.connect_local("scroll", false, move |args| {
        let ctrl: gtk::EventControllerScroll = args[0].get().ok()?;
        let dx: f64 = args[1].get().ok()?;
        let dy: f64 = args[2].get().ok()?;
        let (sx, sy) = PENDING.with(Cell::take).unwrap_or((dx, dy));
        let inst: *mut glib::gobject_ffi::GObject =
            ctrl.upcast_ref::<glib::Object>().to_glib_none().0;
        let mine = ours_in.get();
        unsafe {
            glib::gobject_ffi::g_signal_handler_block(inst, mine);
            glib::gobject_ffi::g_signal_handler_unblock(inst, theirs);
        }
        let handled: bool = ctrl.emit_by_name("scroll", &[&sx, &sy]);
        unsafe {
            glib::gobject_ffi::g_signal_handler_block(inst, theirs);
            glib::gobject_ffi::g_signal_handler_unblock(inst, mine);
        }
        Some(handled.to_value())
    });
    // The handler lives as long as the controller; nothing disconnects it.
    ours.set(unsafe { id.as_raw() });
}
