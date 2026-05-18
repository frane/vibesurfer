//! Host-side cookie save/load via `WKHTTPCookieStore`.
//!
//! `document.cookie` (the JS API used through v0.1.1) cannot see or
//! write cookies with the `HttpOnly` attribute. The HTTP cookie store
//! held by the WKWebView's data store can: the attribute is a
//! JS-visibility flag, not an engine-visibility flag. Routing through
//! the host API is what makes `vs auth save/load` correct for
//! session-cookie auth.
//!
//! `WKHTTPCookieStore::getAllCookies` and `setCookie:` are async
//! (block-based completion handlers). We drive the engine's
//! `NSRunLoop` via `run_loop_until` while waiting; same pattern as
//! `capture` and `eval_js_string`.

use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;

use block2::StackBlock;
use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2_foundation::{
    NSArray, NSDate, NSDictionary, NSHTTPCookie, NSHTTPCookieDomain, NSHTTPCookieExpires,
    NSHTTPCookieName, NSHTTPCookiePath, NSHTTPCookiePropertyKey, NSHTTPCookieSecure,
    NSHTTPCookieValue, NSString,
};
use objc2_web_kit::WKWebView;

use crate::backend::auth::CookieData;
use crate::engine::{EngineError, EngineResult};

use super::eval::run_loop_until;

const ASYNC_BUDGET: Duration = Duration::from_secs(5);

/// Read every cookie from the WKWebView's data store, including
/// `HttpOnly` entries the JS shim never saw.
pub(crate) fn get_all_cookies(web_view: &Retained<WKWebView>) -> EngineResult<Vec<CookieData>> {
    let store = unsafe {
        web_view
            .configuration()
            .websiteDataStore()
            .httpCookieStore()
    };
    let slot: Rc<RefCell<Option<Vec<CookieData>>>> = Rc::new(RefCell::new(None));
    let slot_for_block = slot.clone();
    let block = StackBlock::new(move |cookies: std::ptr::NonNull<NSArray<NSHTTPCookie>>| {
        let array: &NSArray<NSHTTPCookie> = unsafe { cookies.as_ref() };
        let count = array.count();
        let mut out: Vec<CookieData> = Vec::with_capacity(count);
        for i in 0..count {
            let cookie = array.objectAtIndex(i);
            out.push(serialize(&cookie));
        }
        *slot_for_block.borrow_mut() = Some(out);
    });
    unsafe { store.getAllCookies(&block) };
    let check = slot.clone();
    let ok = run_loop_until(move || check.borrow().is_some(), ASYNC_BUDGET);
    if !ok {
        return Err(EngineError::Timeout {
            budget: ASYNC_BUDGET,
            primitive: "save_auth (getAllCookies)",
        });
    }
    let result = slot.borrow_mut().take().unwrap_or_default();
    Ok(result)
}

/// Write each cookie back. Skips entries the API rejects (v1-migrated
/// cookies often lack a domain attribute the host store requires).
pub(crate) fn set_cookies(
    web_view: &Retained<WKWebView>,
    cookies: &[CookieData],
) -> EngineResult<()> {
    let store = unsafe {
        web_view
            .configuration()
            .websiteDataStore()
            .httpCookieStore()
    };
    for cookie in cookies {
        let Some(ns) = build_cookie(cookie) else {
            continue;
        };
        let done: Rc<RefCell<bool>> = Rc::new(RefCell::new(false));
        let done_for_block = done.clone();
        let block = StackBlock::new(move || {
            *done_for_block.borrow_mut() = true;
        });
        unsafe { store.setCookie_completionHandler(&ns, Some(&block)) };
        let check = done.clone();
        let ok = run_loop_until(move || *check.borrow(), ASYNC_BUDGET);
        if !ok {
            return Err(EngineError::Timeout {
                budget: ASYNC_BUDGET,
                primitive: "load_auth (setCookie)",
            });
        }
    }
    Ok(())
}

fn serialize(cookie: &NSHTTPCookie) -> CookieData {
    let name = cookie.name().to_string();
    let value = cookie.value().to_string();
    let domain = cookie.domain().to_string();
    let path = cookie.path().to_string();
    let secure = cookie.isSecure();
    let http_only = cookie.isHTTPOnly();
    let expires_unix = cookie.expiresDate().map(|d| {
        // Unix seconds fit cleanly in f64's 53-bit mantissa for any
        // year humans will encounter; round to whole seconds.
        #[allow(clippy::cast_possible_truncation, clippy::cast_precision_loss)]
        let s = d.timeIntervalSince1970().round() as i64;
        s
    });
    CookieData {
        name,
        value,
        domain,
        path,
        expires_unix,
        secure,
        http_only,
        same_site: None,
    }
}

/// Build an `NSHTTPCookie` from our wire shape. `cookieWithProperties:`
/// is documented; the `"HttpOnly"` key is a well-known internal
/// property that the parser recognizes and that survives the round
/// trip into the cookie's `isHTTPOnly` getter. If a future macOS
/// removes that recognition, our regression test catches it.
fn build_cookie(cookie: &CookieData) -> Option<Retained<NSHTTPCookie>> {
    if cookie.name.is_empty() || cookie.domain.is_empty() {
        return None;
    }

    // Allocate every dynamic NSString up-front. The dictionary
    // construction below borrows each by reference; the binding lives
    // until the end of this function so the refs stay valid.
    let name_ns = NSString::from_str(&cookie.name);
    let value_ns = NSString::from_str(&cookie.value);
    let domain_ns = NSString::from_str(&cookie.domain);
    let path = if cookie.path.is_empty() {
        "/"
    } else {
        cookie.path.as_str()
    };
    let path_ns = NSString::from_str(path);
    let truthy_secure = NSString::from_str("TRUE");
    let truthy_http_only = NSString::from_str("TRUE");
    let same_site_value_ns = cookie.same_site.as_deref().map(NSString::from_str);
    let http_only_key_ns = NSString::from_str("HttpOnly");
    let same_site_key_ns = NSString::from_str("SameSite");
    let expires_ns = cookie.expires_unix.map(|unix| {
        // Unix seconds fit in f64 mantissa precisely.
        #[allow(clippy::cast_precision_loss)]
        let f = unix as f64;
        NSDate::dateWithTimeIntervalSince1970(f)
    });

    let mut keys: Vec<&NSHTTPCookiePropertyKey> = Vec::with_capacity(8);
    let mut vals: Vec<&AnyObject> = Vec::with_capacity(8);

    let push_static = |keys: &mut Vec<&NSHTTPCookiePropertyKey>,
                       vals: &mut Vec<&AnyObject>,
                       k: &'static NSHTTPCookiePropertyKey,
                       v: &'static NSString| {
        keys.push(k);
        vals.push(v.as_ref());
    };
    let _ = push_static; // type sketch only; we use the closures inline below

    keys.push(unsafe { NSHTTPCookieName });
    vals.push(name_ns.as_ref());
    keys.push(unsafe { NSHTTPCookieValue });
    vals.push(value_ns.as_ref());
    keys.push(unsafe { NSHTTPCookieDomain });
    vals.push(domain_ns.as_ref());
    keys.push(unsafe { NSHTTPCookiePath });
    vals.push(path_ns.as_ref());

    if cookie.secure {
        keys.push(unsafe { NSHTTPCookieSecure });
        vals.push(truthy_secure.as_ref());
    }

    if let Some(date) = expires_ns.as_ref() {
        keys.push(unsafe { NSHTTPCookieExpires });
        vals.push(date.as_ref() as &AnyObject);
    }

    if cookie.http_only {
        // Private property the NSHTTPCookie parser recognizes; the
        // resulting cookie's `isHTTPOnly` reads `YES`. No public key
        // exists for this; round-trip regression test pins the
        // behavior.
        keys.push(http_only_key_ns.as_ref());
        vals.push(truthy_http_only.as_ref());
    }

    if let Some(ss) = same_site_value_ns.as_ref() {
        keys.push(same_site_key_ns.as_ref());
        vals.push(ss.as_ref());
    }

    let dict: Retained<NSDictionary<NSHTTPCookiePropertyKey, AnyObject>> =
        NSDictionary::from_slices(&keys, &vals);
    unsafe { NSHTTPCookie::cookieWithProperties(&dict) }
}
