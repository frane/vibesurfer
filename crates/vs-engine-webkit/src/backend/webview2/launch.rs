//! Starting a page's browser: the environment (the browser process a
//! page runs in) and the composition controller hosted on the page's
//! window.

use webview2_com::Microsoft::Web::WebView2::Win32::{
    CreateCoreWebView2Environment, CreateCoreWebView2EnvironmentWithOptions,
    ICoreWebView2CompositionController, ICoreWebView2Environment, ICoreWebView2Environment3,
    ICoreWebView2EnvironmentOptions,
};
use webview2_com::{
    CoreWebView2EnvironmentOptions, CreateCoreWebView2CompositionControllerCompletedHandler,
    CreateCoreWebView2EnvironmentCompletedHandler,
};
use windows::core::{Interface, HSTRING, PCWSTR};
use windows::Win32::Foundation::HWND;

use crate::engine::{DataStore, EngineError, EngineResult};

use super::host::{isolated_controller_options, proxied_user_data_dir};
use super::wait::{failed, run_async, SETUP_BUDGET};

/// The environment for a page of `store`.
///
/// A proxied session needs its own browser process: proxy flags are
/// browser arguments, and every environment on one user data folder
/// must pass the same ones. So it gets a per-session folder under the
/// temp dir. `<-loopback>` stops Chromium's implicit loopback bypass, so
/// local test servers are proxied too.
pub(super) fn create_environment(store: &DataStore) -> EngineResult<ICoreWebView2Environment> {
    let proxied = match store {
        DataStore::Proxied {
            key,
            port,
            spki_pins,
            ..
        } => Some((proxied_user_data_dir(key), *port, spki_pins.clone())),
        _ => None,
    };
    run_async(
        "CreateCoreWebView2Environment",
        SETUP_BUDGET,
        |tx| {
            CreateCoreWebView2EnvironmentCompletedHandler::create(Box::new(move |error, env| {
                let _ = tx.send(created("CreateCoreWebView2Environment", error, env));
                Ok(())
            }))
        },
        |h| unsafe {
            match proxied {
                None => CreateCoreWebView2Environment(h),
                Some((dir, port, pins)) => {
                    // The proxy answers TLS with leaves from the daemon's
                    // CA. Chromium accepts a chain with any of these
                    // public keys despite the unknown root; the flag
                    // applies to this session's browser process only.
                    // Edge's own background traffic (component updates,
                    // telemetry) would otherwise reach the proxy and the
                    // session's event stream as if the page had made it.
                    let options = CoreWebView2EnvironmentOptions::default();
                    options.set_additional_browser_arguments(format!(
                        "--proxy-server=http://127.0.0.1:{port} --proxy-bypass-list=<-loopback> --ignore-certificate-errors-spki-list={pins} --disable-background-networking --disable-component-update --disable-domain-reliability"
                    ));
                    let options: ICoreWebView2EnvironmentOptions = options.into();
                    CreateCoreWebView2EnvironmentWithOptions(
                        PCWSTR::null(),
                        &HSTRING::from(dir.as_os_str()),
                        &options,
                        h,
                    )
                }
            }
        },
    )
}

/// A composition controller on `parent`.
///
/// The regular `CreateCoreWebView2Controller` has no input-injection
/// API; `CompositionController` is the variant Microsoft documents for
/// hosted rendering where the embedder drives `SendMouseInput`. An
/// isolated session's pages open in an InPrivate profile named after
/// the session: pages of the session share it, nothing else does, and
/// WebView2 deletes its data once the last page using it closes.
pub(super) fn create_controller(
    environment: &ICoreWebView2Environment,
    store: &DataStore,
    parent: HWND,
) -> EngineResult<ICoreWebView2CompositionController> {
    let env3: ICoreWebView2Environment3 = environment
        .cast()
        .map_err(|e| EngineError::Other(format!("cast Environment3: {e}")))?;
    let isolated = store
        .key()
        .map(|key| isolated_controller_options(environment, key))
        .transpose()?;
    run_async(
        "CreateCoreWebView2CompositionController",
        SETUP_BUDGET,
        |tx| {
            CreateCoreWebView2CompositionControllerCompletedHandler::create(Box::new(
                move |error, controller| {
                    let _ = tx.send(created(
                        "CreateCoreWebView2CompositionController",
                        error,
                        controller,
                    ));
                    Ok(())
                },
            ))
        },
        |h| unsafe {
            match &isolated {
                None => env3.CreateCoreWebView2CompositionController(parent, h),
                Some((env10, options)) => {
                    env10.CreateCoreWebView2CompositionControllerWithOptions(parent, options, h)
                }
            }
        },
    )
}

/// A creation call's result: its error, or the object it made.
fn created<T>(
    primitive: &'static str,
    error: windows::core::Result<()>,
    made: Option<T>,
) -> EngineResult<T> {
    error.map_err(|e| failed(primitive, &e))?;
    made.ok_or_else(|| EngineError::Other(format!("{primitive}: completed without a result")))
}
