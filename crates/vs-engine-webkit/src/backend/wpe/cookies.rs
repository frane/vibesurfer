//! Host-side cookies through `WebKitCookieManager`. Mirror of the macOS
//! `webkit::cookie_store` module: `document.cookie` can't see or write
//! `HttpOnly` cookies, the cookie manager can, so auth save and load go
//! through here on every platform.

use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;

use webkit6::gio;
use webkit6::glib;
use webkit6::prelude::*;
use webkit6::soup;
use webkit6::WebView;

use crate::backend::auth::CookieData;
use crate::engine::{EngineError, EngineResult};

use super::run_loop_until;

const ASYNC_BUDGET: Duration = Duration::from_secs(5);

fn cookie_manager(web_view: &WebView) -> EngineResult<webkit6::CookieManager> {
    let session = WebViewExt::network_session(web_view).ok_or_else(|| {
        EngineError::Other("WebView has no NetworkSession; cookies unavailable".into())
    })?;
    session
        .cookie_manager()
        .ok_or_else(|| EngineError::Other("NetworkSession has no CookieManager".into()))
}

pub(super) fn get_all_cookies(web_view: &WebView) -> EngineResult<Vec<CookieData>> {
    let manager = cookie_manager(web_view)?;
    let slot: Rc<RefCell<Option<Vec<CookieData>>>> = Rc::new(RefCell::new(None));
    let slot_cb = slot.clone();
    manager.all_cookies(None::<&gio::Cancellable>, move |result| {
        let cookies = match result {
            Ok(v) => v.into_iter().map(|mut c| serialize(&mut c)).collect(),
            Err(_) => Vec::new(),
        };
        *slot_cb.borrow_mut() = Some(cookies);
    });
    let check = slot.clone();
    let ok = run_loop_until(move || check.borrow().is_some(), ASYNC_BUDGET);
    if !ok {
        return Err(EngineError::Timeout {
            budget: ASYNC_BUDGET,
            primitive: "save_auth (all_cookies)",
            detail: "",
        });
    }
    let result = slot.borrow_mut().take().unwrap_or_default();
    Ok(result)
}

pub(super) fn set_cookies(web_view: &WebView, cookies: &[CookieData]) -> EngineResult<()> {
    let manager = cookie_manager(web_view)?;
    for c in cookies {
        if c.name.is_empty() || c.domain.is_empty() {
            continue;
        }
        let path = if c.path.is_empty() {
            "/"
        } else {
            c.path.as_str()
        };
        // max_age=-1: session cookie (no expiration). We set
        // expires explicitly below if the saved blob carried it.
        let mut cookie = soup::Cookie::new(&c.name, &c.value, &c.domain, path, -1);
        cookie.set_secure(c.secure);
        cookie.set_http_only(c.http_only);
        if let Some(unix) = c.expires_unix {
            if let Ok(dt) = glib::DateTime::from_unix_utc(unix) {
                cookie.set_expires(&dt);
            }
        }
        if let Some(ss) = c.same_site.as_deref() {
            let policy = match ss {
                "Strict" => soup::SameSitePolicy::Strict,
                "None" => soup::SameSitePolicy::None,
                _ => soup::SameSitePolicy::Lax,
            };
            cookie.set_same_site_policy(policy);
        }

        let done: Rc<RefCell<bool>> = Rc::new(RefCell::new(false));
        let done_cb = done.clone();
        manager.add_cookie(&cookie, None::<&gio::Cancellable>, move |_| {
            *done_cb.borrow_mut() = true;
        });
        let check = done.clone();
        let ok = run_loop_until(move || *check.borrow(), ASYNC_BUDGET);
        if !ok {
            return Err(EngineError::Timeout {
                budget: ASYNC_BUDGET,
                primitive: "load_auth (add_cookie)",
                detail: "",
            });
        }
    }
    Ok(())
}

/// Delete every cookie of the web view's network session.
pub(super) fn delete_all(web_view: &WebView) -> EngineResult<()> {
    let manager = cookie_manager(web_view)?;
    let slot: Rc<RefCell<Option<Vec<soup::Cookie>>>> = Rc::new(RefCell::new(None));
    let slot_cb = slot.clone();
    manager.all_cookies(None::<&gio::Cancellable>, move |result| {
        *slot_cb.borrow_mut() = Some(result.unwrap_or_default());
    });
    let check = slot.clone();
    if !run_loop_until(move || check.borrow().is_some(), ASYNC_BUDGET) {
        return Err(EngineError::Timeout {
            budget: ASYNC_BUDGET,
            primitive: "cookies clear (all_cookies)",
            detail: "",
        });
    }
    let all = slot.borrow_mut().take().unwrap_or_default();
    let left = Rc::new(RefCell::new(all.len()));
    for c in &all {
        let left_cb = left.clone();
        manager.delete_cookie(c, None::<&gio::Cancellable>, move |_| {
            *left_cb.borrow_mut() -= 1;
        });
    }
    let check = left.clone();
    if !run_loop_until(move || *check.borrow() == 0, ASYNC_BUDGET) {
        return Err(EngineError::Timeout {
            budget: ASYNC_BUDGET,
            primitive: "cookies clear (delete_cookie)",
            detail: "",
        });
    }
    Ok(())
}

fn serialize(c: &mut soup::Cookie) -> CookieData {
    CookieData {
        name: c.name().map(|s| s.to_string()).unwrap_or_default(),
        value: c.value().map(|s| s.to_string()).unwrap_or_default(),
        domain: c.domain().map(|s| s.to_string()).unwrap_or_default(),
        path: c.path().map(|s| s.to_string()).unwrap_or_default(),
        expires_unix: c.expires().map(|dt| dt.to_unix()),
        secure: c.is_secure(),
        http_only: c.is_http_only(),
        same_site: match c.same_site_policy() {
            soup::SameSitePolicy::Strict => Some("Strict".to_string()),
            soup::SameSitePolicy::None => Some("None".to_string()),
            soup::SameSitePolicy::Lax => Some("Lax".to_string()),
            _ => None,
        },
    }
}
