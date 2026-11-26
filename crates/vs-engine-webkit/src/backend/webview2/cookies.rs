//! Host-side cookies through `ICoreWebView2CookieManager`. Mirror of the
//! macOS `webkit::cookie_store` and Linux `wpe::cookies` modules:
//! `document.cookie` can't see or write `HttpOnly` cookies, the cookie
//! manager can, so auth save and load go through here on every backend.

use std::sync::mpsc;

use webview2_com::Microsoft::Web::WebView2::Win32::{
    ICoreWebView2, ICoreWebView2Cookie, ICoreWebView2CookieList, ICoreWebView2CookieManager,
    ICoreWebView2_2, COREWEBVIEW2_COOKIE_SAME_SITE_KIND_LAX,
    COREWEBVIEW2_COOKIE_SAME_SITE_KIND_NONE, COREWEBVIEW2_COOKIE_SAME_SITE_KIND_STRICT,
};
use webview2_com::{pwstr_from_str, take_pwstr, GetCookiesCompletedHandler};
use windows::core::{Interface, BOOL, HSTRING, PCWSTR, PWSTR};

use crate::backend::auth::CookieData;
use crate::engine::{EngineError, EngineResult};

fn cookie_manager(web_view: &ICoreWebView2) -> EngineResult<ICoreWebView2CookieManager> {
    // CookieManager is on ICoreWebView2_2, not the base ICoreWebView2.
    // Cast (QueryInterface) then call.
    let v2: ICoreWebView2_2 = web_view
        .cast()
        .map_err(|e| EngineError::Other(format!("cast to ICoreWebView2_2: {e}")))?;
    unsafe { v2.CookieManager() }.map_err(|e| EngineError::Other(format!("CookieManager: {e}")))
}

pub(super) fn get_all_cookies(web_view: &ICoreWebView2) -> EngineResult<Vec<CookieData>> {
    let manager = cookie_manager(web_view)?;
    let (tx, rx) = mpsc::channel();
    let empty: HSTRING = HSTRING::new();
    let manager_for_init = manager.clone();
    GetCookiesCompletedHandler::wait_for_async_operation(
        Box::new(move |handler| {
            unsafe { manager_for_init.GetCookies(PCWSTR(empty.as_ptr()), &handler) }
                .map_err(webview2_com::Error::WindowsError)
        }),
        Box::new(move |hr, list| {
            hr?;
            let mut out: Vec<CookieData> = Vec::new();
            if let Some(list) = list {
                if let Ok(count) = read_count(&list) {
                    for i in 0..count {
                        if let Ok(c) = unsafe { list.GetValueAtIndex(i) } {
                            out.push(serialize(&c));
                        }
                    }
                }
            }
            let _ = tx.send(out);
            Ok(())
        }),
    )
    .map_err(|e| EngineError::Other(format!("GetCookies: {e:?}")))?;
    rx.recv()
        .map_err(|_| EngineError::Other("GetCookies: channel closed".into()))
}

/// Delete every cookie of the web view's profile.
pub(super) fn delete_all(web_view: &ICoreWebView2) -> EngineResult<()> {
    let manager = cookie_manager(web_view)?;
    unsafe { manager.DeleteAllCookies() }
        .map_err(|e| EngineError::Other(format!("DeleteAllCookies: {e}")))
}

pub(super) fn set_cookies(web_view: &ICoreWebView2, cookies: &[CookieData]) -> EngineResult<()> {
    let manager = cookie_manager(web_view)?;
    for c in cookies {
        if c.name.is_empty() || c.domain.is_empty() {
            continue;
        }
        let name = pwstr_from_str(&c.name);
        let value = pwstr_from_str(&c.value);
        let domain = pwstr_from_str(&c.domain);
        let path_str = if c.path.is_empty() {
            "/"
        } else {
            c.path.as_str()
        };
        let path = pwstr_from_str(path_str);
        let cookie: ICoreWebView2Cookie = unsafe {
            manager.CreateCookie(
                PCWSTR(name.0),
                PCWSTR(value.0),
                PCWSTR(domain.0),
                PCWSTR(path.0),
            )
        }
        .map_err(|e| EngineError::Other(format!("CreateCookie: {e}")))?;
        unsafe { cookie.SetIsHttpOnly(c.http_only) }
            .map_err(|e| EngineError::Other(format!("SetIsHttpOnly: {e}")))?;
        unsafe { cookie.SetIsSecure(c.secure) }
            .map_err(|e| EngineError::Other(format!("SetIsSecure: {e}")))?;
        #[allow(clippy::cast_precision_loss)]
        if let Some(unix) = c.expires_unix {
            unsafe { cookie.SetExpires(unix as f64) }
                .map_err(|e| EngineError::Other(format!("SetExpires: {e}")))?;
        }
        if let Some(ss) = c.same_site.as_deref() {
            let kind = match ss {
                "Strict" => COREWEBVIEW2_COOKIE_SAME_SITE_KIND_STRICT,
                "None" => COREWEBVIEW2_COOKIE_SAME_SITE_KIND_NONE,
                _ => COREWEBVIEW2_COOKIE_SAME_SITE_KIND_LAX,
            };
            unsafe { cookie.SetSameSite(kind) }
                .map_err(|e| EngineError::Other(format!("SetSameSite: {e}")))?;
        }
        unsafe { manager.AddOrUpdateCookie(&cookie) }
            .map_err(|e| EngineError::Other(format!("AddOrUpdateCookie: {e}")))?;
        // Free the PWSTR buffers we passed in (LocalAlloc'd by
        // pwstr_from_str; take_pwstr frees on drop).
        let _ = take_pwstr(name);
        let _ = take_pwstr(value);
        let _ = take_pwstr(domain);
        let _ = take_pwstr(path);
    }
    Ok(())
}

fn read_count(list: &ICoreWebView2CookieList) -> windows::core::Result<u32> {
    let mut count: u32 = 0;
    unsafe { list.Count(&raw mut count) }?;
    Ok(count)
}

fn read_pwstr_getter<F>(call: F) -> String
where
    F: FnOnce(&mut PWSTR) -> windows::core::Result<()>,
{
    let mut out: PWSTR = PWSTR::null();
    match call(&mut out) {
        Ok(()) if !out.is_null() => take_pwstr(out),
        _ => String::new(),
    }
}

fn read_bool_getter<F>(call: F) -> bool
where
    F: FnOnce(&mut BOOL) -> windows::core::Result<()>,
{
    let mut out: BOOL = BOOL(0);
    match call(&mut out) {
        Ok(()) => out.as_bool(),
        Err(_) => false,
    }
}

fn serialize(c: &ICoreWebView2Cookie) -> CookieData {
    let name = read_pwstr_getter(|p| unsafe { c.Name(&raw mut *p) });
    let value = read_pwstr_getter(|p| unsafe { c.Value(&raw mut *p) });
    let domain = read_pwstr_getter(|p| unsafe { c.Domain(&raw mut *p) });
    let path = read_pwstr_getter(|p| unsafe { c.Path(&raw mut *p) });
    let secure = read_bool_getter(|b| unsafe { c.IsSecure(&raw mut *b) });
    let http_only = read_bool_getter(|b| unsafe { c.IsHttpOnly(&raw mut *b) });
    let expires_unix = {
        let mut d: f64 = 0.0;
        match unsafe { c.Expires(&raw mut d) } {
            Ok(()) if d > 0.0 => {
                #[allow(clippy::cast_possible_truncation, clippy::cast_precision_loss)]
                let i = d.round() as i64;
                Some(i)
            }
            _ => None,
        }
    };
    let same_site = {
        let mut k = COREWEBVIEW2_COOKIE_SAME_SITE_KIND_LAX;
        match unsafe { c.SameSite(&raw mut k) } {
            Ok(()) => match k {
                COREWEBVIEW2_COOKIE_SAME_SITE_KIND_STRICT => Some("Strict".to_string()),
                COREWEBVIEW2_COOKIE_SAME_SITE_KIND_NONE => Some("None".to_string()),
                COREWEBVIEW2_COOKIE_SAME_SITE_KIND_LAX => Some("Lax".to_string()),
                _ => None,
            },
            _ => None,
        }
    };
    CookieData {
        name,
        value,
        domain,
        path,
        expires_unix,
        secure,
        http_only,
        same_site,
    }
}
