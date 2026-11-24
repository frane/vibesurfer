//! Point a `WKWebsiteDataStore` at the daemon's local HTTP proxy.
//!
//! `WKWebsiteDataStore.proxyConfigurations` (macOS 14+) takes an array
//! of Network framework `nw_proxy_config_t` objects. The only proxy
//! kind that fits is HTTP CONNECT: WebKit then tunnels every
//! connection, plain `http://` included, through `CONNECT host:port`,
//! and the proxy reads the request inside the tunnel.
//!
//! WebKit never proxies loopback: `127.0.0.1`, `localhost`, `::1` and
//! the host's own interface addresses go direct whatever the config
//! says (match domains, `prohibit_direct` and the legacy proxy
//! dictionary do not change that). Pages served from loopback get a
//! scheme-handler web view instead; see `loopback.rs`.

use std::ffi::{c_char, CString};

use objc2::rc::Retained;
use objc2::runtime::{AnyObject, Bool};
use objc2::{msg_send, sel};
use objc2_foundation::NSArray;
use objc2_web_kit::WKWebsiteDataStore;

use crate::engine::{EngineError, EngineResult};

#[allow(non_camel_case_types)]
type nw_object = *mut AnyObject;

#[link(name = "Network", kind = "framework")]
unsafe extern "C" {
    fn nw_endpoint_create_host(hostname: *const c_char, port: *const c_char) -> nw_object;
    fn nw_proxy_config_create_http_connect(
        endpoint: nw_object,
        tls_options: nw_object,
    ) -> nw_object;
    fn nw_proxy_config_set_failover_allowed(config: nw_object, allowed: bool);
}

/// Route `store`'s connections through `127.0.0.1:<port>`. Fails on
/// macOS older than 14, which has no `proxyConfigurations`.
pub(super) fn set_http_proxy(store: &WKWebsiteDataStore, port: u16) -> EngineResult<()> {
    let responds: Bool =
        unsafe { msg_send![store, respondsToSelector: sel!(setProxyConfigurations:)] };
    if !responds.as_bool() {
        return Err(EngineError::Other(
            "intercept needs macOS 14 or later (WKWebsiteDataStore.proxyConfigurations)".into(),
        ));
    }
    let port = CString::new(port.to_string()).expect("digits have no NUL");
    // Both create functions return +1 retained OS objects, which are
    // Objective-C objects; `Retained::from_raw` takes that reference.
    let config = unsafe {
        let endpoint = nw_endpoint_create_host(c"127.0.0.1".as_ptr(), port.as_ptr());
        let endpoint = Retained::from_raw(endpoint)
            .ok_or_else(|| EngineError::Other("nw_endpoint_create_host failed".into()))?;
        let raw = nw_proxy_config_create_http_connect(
            Retained::as_ptr(&endpoint).cast_mut(),
            std::ptr::null_mut(),
        );
        Retained::from_raw(raw).ok_or_else(|| {
            EngineError::Other("nw_proxy_config_create_http_connect failed".into())
        })?
    };
    // A dead proxy must fail the load, not silently go direct and
    // drop requests from the record.
    unsafe { nw_proxy_config_set_failover_allowed(Retained::as_ptr(&config).cast_mut(), false) };
    let configs = NSArray::from_retained_slice(&[config]);
    let _: () = unsafe { msg_send![store, setProxyConfigurations: &*configs] };
    Ok(())
}
