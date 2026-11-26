//! Building a page's host: controller options for isolated and proxied
//! sessions, the composition-hosted web view with its handlers, and
//! popups a page opens.

use crate::backend::native_download::SharedDownloads;

use webview2_com::Microsoft::Web::WebView2::Win32::{
    ICoreWebView2, ICoreWebView2CompositionController, ICoreWebView2Controller,
    ICoreWebView2ControllerOptions, ICoreWebView2Environment, ICoreWebView2Environment10,
    ICoreWebView2Environment3, COREWEBVIEW2_MOVE_FOCUS_REASON_PROGRAMMATIC,
};
use webview2_com::{
    take_pwstr, CreateCoreWebView2CompositionControllerCompletedHandler, FrameCreatedEventHandler,
    NewWindowRequestedEventHandler, ScriptDialogOpeningEventHandler,
    WindowCloseRequestedEventHandler,
};
use windows::core::{Interface, HSTRING, PWSTR};
use windows::Win32::Foundation::{HWND, RECT};
use windows::Win32::Graphics::DirectComposition::{
    DCompositionCreateDevice2, IDCompositionDevice, IDCompositionTarget, IDCompositionVisual,
};

use crate::backend::inspector_bridge::InspectorSlots;
use crate::engine::{EngineError, EngineResult, PageHandle};

use super::capture::{install_download_capture, install_response_capture};
use super::script::{add_init_script, install_inspector, without_waiting};
use super::{new_host_hwnd, Popups, ViewOrigin, W2Page};

/// Controller options for an isolated session: InPrivate, with a
/// profile name derived from `key` so the session's pages share one
/// profile and no other session's pages do. Profile names allow only a
/// small character set, so anything else in the key becomes `_`.
pub(super) fn isolated_controller_options(
    environment: &ICoreWebView2Environment,
    key: &str,
) -> EngineResult<(ICoreWebView2Environment10, ICoreWebView2ControllerOptions)> {
    let env10: ICoreWebView2Environment10 = environment.cast().map_err(|e| {
        EngineError::Other(format!(
            "isolated sessions need WebView2 runtime with profiles: {e}"
        ))
    })?;
    let options = unsafe { env10.CreateCoreWebView2ControllerOptions() }
        .map_err(|e| EngineError::Other(format!("CreateControllerOptions: {e}")))?;
    let name: String = std::iter::once("vsiso_")
        .chain(std::iter::once(key))
        .flat_map(str::chars)
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .take(64)
        .collect();
    unsafe {
        options
            .SetProfileName(&HSTRING::from(name))
            .map_err(|e| EngineError::Other(format!("SetProfileName: {e}")))?;
        options
            .SetIsInPrivateModeEnabled(true)
            .map_err(|e| EngineError::Other(format!("SetIsInPrivateModeEnabled: {e}")))?;
    }
    Ok((env10, options))
}

/// Allow certificate errors in an intercept page (see `open`).
fn allow_proxy_certificates(web_view: &ICoreWebView2) -> EngineResult<()> {
    use webview2_com::Microsoft::Web::WebView2::Win32::{
        ICoreWebView2_14, COREWEBVIEW2_SERVER_CERTIFICATE_ERROR_ACTION_ALWAYS_ALLOW,
    };
    let wv14: ICoreWebView2_14 = web_view
        .cast()
        .map_err(|e| EngineError::Other(format!("cast ICoreWebView2_14: {e}")))?;
    let handler = webview2_com::ServerCertificateErrorDetectedEventHandler::create(Box::new(
        |_sender, args| {
            if let Some(args) = args {
                unsafe {
                    args.SetAction(COREWEBVIEW2_SERVER_CERTIFICATE_ERROR_ACTION_ALWAYS_ALLOW)
                }?;
            }
            Ok(())
        },
    ));
    let mut token: i64 = 0;
    unsafe { wv14.add_ServerCertificateErrorDetected(&handler, &raw mut token) }
        .map_err(|e| EngineError::Other(format!("add_ServerCertificateErrorDetected: {e}")))
}

/// User data folder for a proxied session's own browser process.
pub(super) fn proxied_user_data_dir(key: &str) -> std::path::PathBuf {
    let name: String = key
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect();
    std::env::temp_dir().join(format!("vibesurfer-intercept-{name}"))
}

/// Make `comp_controller` (hosted on `parent`) a page: DirectComposition
/// wiring, bounds, visibility and focus, the inspector bridge, download
/// and response capture, and the new-window, close, dialog and frame
/// handlers. Used for pages `open` creates and for windows a page opens,
/// so a popup is the same kind of page. The caller sets `me` once the
/// page has a handle.
#[allow(clippy::too_many_lines)]
pub(super) fn host_view(
    origin: &ViewOrigin,
    parent: HWND,
    comp_controller: ICoreWebView2CompositionController,
    popups: &Popups,
) -> EngineResult<W2Page> {
    use std::cell::{Cell, RefCell};
    use std::rc::Rc;
    use webview2_com::Microsoft::Web::WebView2::Win32::ICoreWebView2_4;

    let controller: ICoreWebView2Controller = comp_controller
        .cast::<ICoreWebView2Controller>()
        .map_err(|e| EngineError::Other(format!("cast Controller: {e}")))?;

    // DirectComposition wiring. CompositionController needs a root
    // visual to render into: one bound to the hidden parent HWND, handed
    // to the controller and committed once. The device, target and
    // visual are stored on the page so their refcounts outlive this call.
    let dcomp_device: IDCompositionDevice = unsafe {
        DCompositionCreateDevice2::<_, IDCompositionDevice>(None)
            .map_err(|e| EngineError::Other(format!("DCompositionCreateDevice2: {e}")))?
    };
    let dcomp_target: IDCompositionTarget = unsafe {
        dcomp_device
            .CreateTargetForHwnd(parent, true)
            .map_err(|e| EngineError::Other(format!("CreateTargetForHwnd: {e}")))?
    };
    let dcomp_visual: IDCompositionVisual = unsafe {
        dcomp_device
            .CreateVisual()
            .map_err(|e| EngineError::Other(format!("CreateVisual: {e}")))?
    };
    unsafe {
        dcomp_target
            .SetRoot(&dcomp_visual)
            .map_err(|e| EngineError::Other(format!("SetRoot: {e}")))?;
        comp_controller
            .SetRootVisualTarget(&dcomp_visual)
            .map_err(|e| EngineError::Other(format!("SetRootVisualTarget: {e}")))?;
        dcomp_device
            .Commit()
            .map_err(|e| EngineError::Other(format!("DComposition Commit: {e}")))?;
    }

    unsafe {
        controller.SetBounds(RECT {
            left: 0,
            top: 0,
            right: 1280,
            bottom: 800,
        })
    }
    .map_err(|e| EngineError::Other(format!("SetBounds: {e}")))?;

    // Mark the (offscreen, composition-hosted) controller visible.
    // `IsVisible` defaults to FALSE, which suspends rendering, and after
    // a `viewport`/`SetBounds` an invisible controller never re-renders,
    // so the next `CapturePreview` never completes. Visible + offscreen
    // composition = continuous frames, nothing shown on screen.
    unsafe { controller.SetIsVisible(true) }
        .map_err(|e| EngineError::Other(format!("SetIsVisible: {e}")))?;
    // Give the page focus. Without it `document.hasFocus()` is false and
    // the page behaves as a backgrounded tab (`autofocus`,
    // `:focus-visible`, IME). Not fatal if it fails.
    if let Err(e) = unsafe { controller.MoveFocus(COREWEBVIEW2_MOVE_FOCUS_REASON_PROGRAMMATIC) } {
        eprintln!("vs: MoveFocus failed ({e:?}); document.hasFocus() will be false");
    }

    let web_view: ICoreWebView2 = unsafe { controller.CoreWebView2() }
        .map_err(|e| EngineError::Other(format!("CoreWebView2: {e}")))?;

    // Deliberately no User-Agent override: WebView2 is Chromium, and a
    // UA that disagrees with the engine underneath is the loudest
    // inconsistency a fingerprinter can find.

    // Inspector bridge before any navigation, so the document-start hook
    // fires on the loaded page. Download capture, same shim as macOS and
    // Linux (`AddScriptToExecuteOnDocumentCreated` covers every frame).
    let inspector = InspectorSlots::new(crate::inspector::DEFAULT_BUFFER_CAPACITY);
    let inspector_installed = install_inspector(&web_view, &inspector);
    let _ = add_init_script(&web_view, crate::backend::common::DOWNLOAD_SHIM_JS);
    // No native drags (see NO_NATIVE_DRAG_JS).
    let _ = add_init_script(&web_view, crate::backend::common::NO_NATIVE_DRAG_JS);
    let downloads = SharedDownloads::default();
    install_download_capture(&web_view, &downloads)?;
    let responses = install_response_capture(&web_view)?;

    // Dialogs go through `ScriptDialogOpening`: held for an answer in a
    // page with popups on, dismissed at once otherwise (as on macOS).
    if let Ok(settings) = unsafe { web_view.Settings() } {
        let _ = unsafe { settings.SetAreDefaultScriptDialogsEnabled(false) };
        // No native context menu: the page still gets its contextmenu
        // event, and nothing stays open to swallow the next click.
        let _ = unsafe { settings.SetAreDefaultContextMenusEnabled(false) };
        // No SmartScreen lookups: they are Edge's own requests, which
        // otherwise show up among the page's (intercept sessions) and
        // cost time on every navigation.
        if let Ok(s8) =
            settings.cast::<webview2_com::Microsoft::Web::WebView2::Win32::ICoreWebView2Settings8>()
        {
            let _ = unsafe { s8.SetIsReputationCheckingRequired(false) };
        }
    }

    let popups_on = Rc::new(Cell::new(false));
    let me: Rc<Cell<Option<PageHandle>>> = Rc::new(Cell::new(None));
    let frames: crate::backend::webview2_extras::Frames = Rc::default();

    // A new window. With popups on it becomes a page of its own in the
    // same environment and profile (`spawn_popup`). Otherwise there is
    // no second window: mark the request handled and park its URL, and
    // the input primitive that caused it navigates this page there
    // (`follow_new_window`). WebView2 does not expose the request body,
    // so a POSTed blank-target form arrives as a GET.
    let new_window: Rc<RefCell<Option<String>>> = Rc::new(RefCell::new(None));
    {
        let parked = new_window.clone();
        let on = popups_on.clone();
        let me = me.clone();
        let popups = popups.clone();
        let origin = origin.clone();
        let handler = NewWindowRequestedEventHandler::create(Box::new(move |_sender, args| {
            let Some(args) = args else { return Ok(()) };
            let mut uri = PWSTR(std::ptr::null_mut());
            let url = if unsafe { args.Uri(&raw mut uri) }.is_ok() {
                take_pwstr(uri)
            } else {
                String::new()
            };
            if on.get() {
                spawn_popup(&origin, &args, me.get(), url, &popups);
                return Ok(());
            }
            *parked.borrow_mut() = Some(url);
            unsafe { args.SetHandled(true) }?;
            Ok(())
        }));
        let mut token: i64 = 0;
        unsafe { web_view.add_NewWindowRequested(&handler, &raw mut token) }
            .map_err(|e| EngineError::Other(format!("add_NewWindowRequested: {e}")))?;
    }
    // `window.close()`: with popups on, report it; the backend releases
    // the page on its next lookup.
    {
        let on = popups_on.clone();
        let me = me.clone();
        let popups = popups.clone();
        let handler = WindowCloseRequestedEventHandler::create(Box::new(move |_, _| {
            if let (true, Some(h)) = (on.get(), me.get()) {
                let mut s = popups.borrow_mut();
                s.closed.push(h);
                s.feed.push(crate::engine::PopupEvent::Closed(h));
            }
            Ok(())
        }));
        let mut token: i64 = 0;
        unsafe { web_view.add_WindowCloseRequested(&handler, &raw mut token) }
            .map_err(|e| EngineError::Other(format!("add_WindowCloseRequested: {e}")))?;
    }
    // The renderer died: report it.
    {
        let on = popups_on.clone();
        let me = me.clone();
        let popups = popups.clone();
        let handler = webview2_com::ProcessFailedEventHandler::create(Box::new(move |_, _| {
            if let (true, Some(h)) = (on.get(), me.get()) {
                popups
                    .borrow_mut()
                    .feed
                    .push(crate::engine::PopupEvent::Crashed(h));
            }
            Ok(())
        }));
        let mut token: i64 = 0;
        unsafe { web_view.add_ProcessFailed(&handler, &raw mut token) }
            .map_err(|e| EngineError::Other(format!("add_ProcessFailed: {e}")))?;
    }
    // `alert`/`confirm`/`prompt`.
    {
        let on = popups_on.clone();
        let me = me.clone();
        let popups = popups.clone();
        let handler = ScriptDialogOpeningEventHandler::create(Box::new(move |_, args| {
            let Some(args) = args else { return Ok(()) };
            if let (true, Some(h)) = (on.get(), me.get()) {
                let feed = popups.borrow().feed.clone();
                crate::backend::webview2_extras::hold_dialog(&feed, h, &args)?;
            }
            Ok(())
        }));
        let mut token: i64 = 0;
        unsafe { web_view.add_ScriptDialogOpening(&handler, &raw mut token) }
            .map_err(|e| EngineError::Other(format!("add_ScriptDialogOpening: {e}")))?;
    }
    // Subframes, for `eval_js_in_frame`.
    if let Ok(wv4) = web_view.cast::<ICoreWebView2_4>() {
        let sink = frames.clone();
        let handler = FrameCreatedEventHandler::create(Box::new(move |_, args| {
            if let Some(args) = args {
                let frame = unsafe { args.Frame() }?;
                crate::backend::webview2_extras::track_frame(&sink, &frame)?;
            }
            Ok(())
        }));
        let mut token: i64 = 0;
        unsafe { wv4.add_FrameCreated(&handler, &raw mut token) }
            .map_err(|e| EngineError::Other(format!("add_FrameCreated: {e}")))?;
    }

    // An intercept page's TLS ends at the daemon's proxy, which verifies
    // upstream itself. Should Chromium still report a certificate error
    // for the proxy's leaves, allow it: only this session's pages get here.
    if origin.proxied {
        allow_proxy_certificates(&web_view)?;
    }

    Ok(W2Page {
        comp_controller,
        controller,
        web_view,
        parent_hwnd: parent,
        _dcomp_device: dcomp_device,
        _dcomp_target: dcomp_target,
        _dcomp_visual: dcomp_visual,
        inspector,
        inspector_installed,
        cookie_baseline: RefCell::new(None),
        cookie_next_seq: RefCell::new(0),
        last_mouse: Cell::new(vs_humanize::Point { x: 0.0, y: 0.0 }),
        new_window,
        downloads,
        responses,
        popups_on,
        me,
        frames,
    })
}

/// A window `opener` asked for (`NewWindowRequested`), as a page of its
/// own: a composition controller in the opener's environment and profile,
/// handed to WebView2 as the new window (so `window.opener` works) and
/// reported on the feed. The controller is created asynchronously, under
/// a deferral of the request.
fn spawn_popup(
    origin: &ViewOrigin,
    args: &webview2_com::Microsoft::Web::WebView2::Win32::ICoreWebView2NewWindowRequestedEventArgs,
    opener: Option<PageHandle>,
    url: String,
    popups: &Popups,
) {
    let Ok(deferral) = (unsafe { args.GetDeferral() }) else {
        return;
    };
    let finish = {
        let deferral = deferral.clone();
        move || {
            let _ = unsafe { deferral.Complete() };
        }
    };
    let (Ok(parent), Ok(env3)) = (
        new_host_hwnd(),
        origin.environment.cast::<ICoreWebView2Environment3>(),
    ) else {
        finish();
        return;
    };
    let Ok(iso) = origin
        .profile_key
        .as_deref()
        .map(|key| isolated_controller_options(&origin.environment, key))
        .transpose()
    else {
        finish();
        return;
    };
    let (origin, args, popups) = (origin.clone(), args.clone(), popups.clone());
    let done = CreateCoreWebView2CompositionControllerCompletedHandler::create(Box::new(
        move |error_code, controller| {
            let adopted = (|| -> EngineResult<()> {
                error_code.map_err(|e| EngineError::Other(format!("popup controller: {e}")))?;
                let comp = controller
                    .ok_or_else(|| EngineError::Other("popup controller: none".into()))?;
                let page = without_waiting(|| host_view(&origin, parent, comp, &popups))?;
                page.popups_on.set(true);
                unsafe { args.SetNewWindow(&page.web_view) }
                    .map_err(|e| EngineError::Other(format!("SetNewWindow: {e}")))?;
                let mut s = popups.borrow_mut();
                let handle = s.alloc();
                page.me.set(Some(handle));
                s.born
                    .push(crate::backend::webview2_extras::Born { handle, page });
                s.feed
                    .push(crate::engine::PopupEvent::Opened(crate::engine::Popup {
                        opener,
                        page: handle,
                        url: url.clone(),
                    }));
                Ok(())
            })();
            if let Err(e) = adopted {
                eprintln!("vs: popup failed: {e}");
            }
            finish();
            Ok(())
        },
    ));
    let started = unsafe {
        match iso {
            None => env3.CreateCoreWebView2CompositionController(parent, &done),
            Some((env10, options)) => {
                env10.CreateCoreWebView2CompositionControllerWithOptions(parent, &options, &done)
            }
        }
    };
    if started.is_err() {
        let _ = unsafe { deferral.Complete() };
    }
}
