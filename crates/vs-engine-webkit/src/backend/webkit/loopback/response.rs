//! The main-queue side of a hop: apply worker messages to the WebKit
//! task, build its responses, follow redirects and finish downloads.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::atomic::Ordering;

use block2::RcBlock;
use objc2::rc::Retained;
use objc2::{msg_send, AllocAnyThread, Message};
use objc2_foundation::{
    NSData, NSDictionary, NSError, NSHTTPCookie, NSHTTPURLResponse, NSMutableURLRequest, NSString,
    NSURLRequest, NSURL,
};
use objc2_web_kit::WKURLSchemeTask;

use super::{finish_download, forget, send_hop, Hop, Msg, TASKS};

/// Main queue: apply one worker message to its task.
pub(super) fn on_msg(id: u64, generation: u64, msg: Msg) {
    let live = TASKS.with(|t| {
        t.borrow()
            .get(&id)
            .is_some_and(|s| s.generation == generation && !s.stopped.load(Ordering::Relaxed))
    });
    if !live {
        return;
    }
    match msg {
        Msg::Head { status, headers } => on_head(id, generation, status, headers),
        Msg::Data(d) => {
            let deliver = TASKS.with(|t| {
                let mut t = t.borrow_mut();
                let s = t.get_mut(&id)?;
                if let Some(tee) = s.tee.as_mut() {
                    tee.extend_from_slice(&d);
                }
                if s.download.is_some() {
                    return None;
                }
                if s.responded {
                    Some(s.task.clone())
                } else {
                    s.queued.push(d.clone());
                    None
                }
            });
            if let Some(task) = deliver {
                let data = NSData::with_bytes(&d);
                guard(|| unsafe { task.didReceiveData(&data) });
            }
        }
        Msg::Done => {
            if TASKS.with(|t| t.borrow().get(&id).is_some_and(|s| s.download.is_some())) {
                finish_download(id, Ok(()));
                return;
            }
            let finish = TASKS.with(|t| {
                let mut t = t.borrow_mut();
                let s = t.get_mut(&id)?;
                s.finished = true;
                s.responded.then(|| s.task.clone())
            });
            if let Some(task) = finish {
                guard(|| unsafe { task.didFinish() });
                forget(id);
            }
        }
        Msg::Fail(why) => {
            if TASKS.with(|t| t.borrow().get(&id).is_some_and(|s| s.download.is_some())) {
                finish_download(id, Err(why));
                return;
            }
            let task = TASKS.with(|t| t.borrow().get(&id).map(|s| s.task.clone()));
            if let Some(task) = task {
                let err = url_error(&why);
                guard(|| unsafe { task.didFailWithError(&err) });
            }
            forget(id);
        }
    }
}

/// Call into a task; a task WebKit already stopped raises, which must
/// not take the daemon down.
fn guard(f: impl FnOnce()) {
    let _ = objc2::exception::catch(std::panic::AssertUnwindSafe(f));
}

fn url_error(_why: &str) -> Retained<NSError> {
    let domain = NSString::from_str("NSURLErrorDomain");
    // NSURLErrorCannotConnectToHost
    unsafe { NSError::errorWithDomain_code_userInfo(&domain, -1004, None) }
}

fn on_head(id: u64, generation: u64, status: u16, headers: Vec<(String, String)>) {
    // A document may turn out to be a download (the delegate decides
    // once WebKit has the response): keep a copy of its body.
    TASKS.with(|t| {
        if let Some(s) = t.borrow_mut().get_mut(&id) {
            let dest = s
                .hop
                .headers
                .iter()
                .find(|(k, _)| k.eq_ignore_ascii_case("sec-fetch-dest"))
                .map(|(_, v)| v.to_ascii_lowercase());
            let attachment = headers.iter().any(|(k, v)| {
                k.eq_ignore_ascii_case("content-disposition")
                    && crate::backend::native_download::is_attachment(v)
            });
            if attachment || matches!(dest.as_deref(), Some("document" | "iframe" | "frame")) {
                s.tee = Some(Vec::new());
            }
        }
    });
    let Some((url, cookies)) = TASKS.with(|t| {
        t.borrow()
            .get(&id)
            .map(|s| (s.hop.url.clone(), s.cookies.clone()))
    }) else {
        return;
    };
    let Some(ns_url) = NSURL::URLWithString(&NSString::from_str(&url)) else {
        return;
    };
    // Every Set-Cookie on its own, so a comma in one (an Expires date)
    // cannot split it.
    let mut jar: Vec<Retained<NSHTTPCookie>> = Vec::new();
    for (_, v) in headers
        .iter()
        .filter(|(k, _)| k.eq_ignore_ascii_case("set-cookie"))
    {
        let k = NSString::from_str("Set-Cookie");
        let v = NSString::from_str(v);
        let dict = NSDictionary::<NSString, NSString>::from_retained_objects(&[&*k], &[v]);
        jar.extend(NSHTTPCookie::cookiesWithResponseHeaderFields_forURL(
            &dict, &ns_url,
        ));
    }
    let location = headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("location"))
        .map(|(_, v)| v.clone());
    let response = http_response(&ns_url, status, &headers);
    let next = move || {
        let redirect = matches!(status, 301 | 302 | 303 | 307 | 308);
        match (
            redirect,
            location.as_deref().and_then(|l| resolve(&ns_url, l)),
        ) {
            (true, Some(to)) => redirect_to(id, generation, status, &response, &to),
            _ => respond(id, &response),
        }
    };
    if jar.is_empty() {
        next();
        return;
    }
    // One `setCookie:` each: `setCookies:` (all at once) is macOS 26+.
    // Answer WebKit once the last one is stored.
    let left = Rc::new(Cell::new(jar.len()));
    let next = Rc::new(RefCell::new(Some(next)));
    for cookie in &jar {
        let (left, next) = (left.clone(), next.clone());
        let block = RcBlock::new(move || {
            left.set(left.get() - 1);
            if left.get() == 0 {
                if let Some(f) = next.borrow_mut().take() {
                    f();
                }
            }
        });
        unsafe { cookies.setCookie_completionHandler(cookie, Some(&block)) };
    }
}

/// `location` resolved against `base`, as an absolute URL.
fn resolve(base: &NSURL, location: &str) -> Option<String> {
    let u = NSURL::URLWithString_relativeToURL(&NSString::from_str(location), Some(base))?;
    u.absoluteURL()
        .and_then(|a| a.absoluteString())
        .map(|s| s.to_string())
}

fn http_response(
    url: &NSURL,
    status: u16,
    headers: &[(String, String)],
) -> Retained<NSHTTPURLResponse> {
    // WebKit folds repeated fields into one, comma-joined.
    let mut merged: Vec<(String, String)> = Vec::new();
    for (k, v) in headers {
        if let Some(m) = merged.iter_mut().find(|(mk, _)| mk.eq_ignore_ascii_case(k)) {
            m.1.push_str(", ");
            m.1.push_str(v);
        } else {
            merged.push((k.clone(), v.clone()));
        }
    }
    let keys: Vec<Retained<NSString>> = merged.iter().map(|(k, _)| NSString::from_str(k)).collect();
    let key_refs: Vec<&NSString> = keys.iter().map(|k| &**k).collect();
    let values: Vec<Retained<NSString>> =
        merged.iter().map(|(_, v)| NSString::from_str(v)).collect();
    let dict = NSDictionary::<NSString, NSString>::from_retained_objects(&key_refs, &values);
    let version = NSString::from_str("HTTP/1.1");
    NSHTTPURLResponse::initWithURL_statusCode_HTTPVersion_headerFields(
        NSHTTPURLResponse::alloc(),
        url,
        isize::try_from(status).unwrap_or(200),
        Some(&version),
        Some(&dict),
    )
    .expect("NSHTTPURLResponse")
}

/// Hand WebKit the response, then whatever body already came.
fn respond(id: u64, response: &NSHTTPURLResponse) {
    let Some(task) = TASKS.with(|t| t.borrow().get(&id).map(|s| s.task.clone())) else {
        return;
    };
    guard(|| unsafe { task.didReceiveResponse(response) });
    let (queued, finished) = TASKS.with(|t| {
        let mut t = t.borrow_mut();
        let Some(s) = t.get_mut(&id) else {
            return (Vec::new(), false);
        };
        s.responded = true;
        (std::mem::take(&mut s.queued), s.finished)
    });
    for d in queued {
        let data = NSData::with_bytes(&d);
        guard(|| unsafe { task.didReceiveData(&data) });
    }
    if finished {
        guard(|| unsafe { task.didFinish() });
        forget(id);
    }
}

/// Report a redirect and send the next hop on the same task. WebKit
/// answers with the request it will follow, or nil when the page does
/// not follow (`fetch(…, { redirect: 'manual' })`), which makes the
/// redirect itself the response.
fn redirect_to(id: u64, generation: u64, status: u16, response: &NSHTTPURLResponse, to: &str) {
    let Some((task, hop, redirects)) = TASKS.with(|t| {
        t.borrow()
            .get(&id)
            .map(|s| (s.task.clone(), s.hop.clone(), s.redirects))
    }) else {
        return;
    };
    if redirects >= 20 {
        on_msg(id, generation, Msg::Fail("too many redirects".into()));
        return;
    }
    // 303, and 301/302 after a POST, turn into a GET without a body.
    let to_get =
        status == 303 || (matches!(status, 301 | 302) && hop.method.eq_ignore_ascii_case("POST"));
    let mut next = Hop {
        method: if to_get {
            "GET".into()
        } else {
            hop.method.clone()
        },
        url: to.to_string(),
        headers: hop.headers.clone(),
        body: if to_get { Vec::new() } else { hop.body.clone() },
    };
    if to_get {
        next.headers.retain(|(k, _)| {
            let k = k.to_ascii_lowercase();
            !k.starts_with("content-")
        });
    }
    let Some(ns_to) = NSURL::URLWithString(&NSString::from_str(to)) else {
        return;
    };
    let req = NSMutableURLRequest::requestWithURL(&ns_to);
    req.setHTTPMethod(&NSString::from_str(&next.method));
    for (k, v) in &next.headers {
        req.setValue_forHTTPHeaderField(Some(&NSString::from_str(v)), &NSString::from_str(k));
    }
    let response_for_manual = response.retain();
    let next = RefCell::new(Some(next));
    let block = RcBlock::new(move |chosen: *mut NSURLRequest| {
        let Some(mut hop) = next.borrow_mut().take() else {
            return;
        };
        if chosen.is_null() {
            respond(id, &response_for_manual);
            on_msg(id, generation, Msg::Done);
            return;
        }
        let chosen: &NSURLRequest = unsafe { &*chosen };
        if let Some(u) = chosen.URL().and_then(|u| u.absoluteString()) {
            hop.url = u.to_string();
        }
        let advanced = TASKS.with(|t| {
            let mut t = t.borrow_mut();
            let Some(s) = t.get_mut(&id) else {
                return false;
            };
            if s.generation != generation {
                return false;
            }
            s.generation += 1;
            s.redirects += 1;
            s.hop = hop.clone();
            s.queued.clear();
            s.finished = false;
            true
        });
        if advanced {
            send_hop(id);
        }
    });
    let r = objc2::exception::catch(std::panic::AssertUnwindSafe(|| unsafe {
        let _: () = msg_send![
            &*task,
            _willPerformRedirection: response,
            newRequest: &*req,
            completionHandler: &*block
        ];
    }));
    if r.is_err() {
        // A stopped task; nothing more to do.
        forget(id);
    }
}
