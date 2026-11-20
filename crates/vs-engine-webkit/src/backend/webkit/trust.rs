//! Server-trust challenges for intercept pages.
//!
//! An intercept page's TLS ends at the daemon's proxy, which answers
//! with leaves signed by the daemon's CA. The navigation delegate gets
//! a server-trust challenge for every TLS connection of the page
//! (documents and subresources alike); for intercept pages the CA is
//! added as an extra anchor and the trust evaluated again. The system
//! roots stay in place. Other pages never reach this code.

use std::ffi::c_void;

use objc2::encode::{Encoding, RefEncode};
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, Sel};
use objc2::{msg_send, sel, ClassType};
use objc2_foundation::{
    NSURLAuthenticationChallenge, NSURLAuthenticationMethodServerTrust, NSURLCredential,
    NSURLProtectionSpace,
};

/// `SecTrustRef` target.
#[repr(C)]
pub(super) struct SecTrust {
    _p: [u8; 0],
}

unsafe impl RefEncode for SecTrust {
    const ENCODING_REF: Encoding = Encoding::Pointer(&Encoding::Struct("__SecTrust", &[]));
}

type CFTypeRef = *const c_void;

#[link(name = "Security", kind = "framework")]
unsafe extern "C" {
    fn SecCertificateCreateWithData(allocator: CFTypeRef, data: CFTypeRef) -> CFTypeRef;
    fn SecTrustSetAnchorCertificates(trust: *mut SecTrust, certs: CFTypeRef) -> i32;
    fn SecTrustSetAnchorCertificatesOnly(trust: *mut SecTrust, only: u8) -> i32;
    fn SecTrustEvaluateWithError(trust: *mut SecTrust, error: *mut CFTypeRef) -> bool;
}

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    static kCFTypeArrayCallBacks: [u8; 0];
    fn CFDataCreate(allocator: CFTypeRef, bytes: *const u8, len: isize) -> CFTypeRef;
    fn CFArrayCreate(
        allocator: CFTypeRef,
        values: *const CFTypeRef,
        n: isize,
        callbacks: *const c_void,
    ) -> CFTypeRef;
    fn CFRelease(cf: CFTypeRef);
}

/// A credential accepting the server trust of `challenge` if it
/// evaluates with `ca_der` as an extra anchor. `None` for other
/// challenges and for trust that still fails.
pub(super) fn credential_for(
    challenge: &NSURLAuthenticationChallenge,
    ca_der: &[u8],
) -> Option<Retained<NSURLCredential>> {
    // WebKit hands over a `WKNSURLAuthenticationChallenge`, a proxy
    // that forwards to the real challenge. objc2's checked send (debug
    // builds) looks the method up on the proxy class and panics, so
    // send this one unchecked.
    let space: Retained<NSURLProtectionSpace> = unsafe {
        let send: unsafe extern "C-unwind" fn(*const AnyObject, Sel) -> *mut NSURLProtectionSpace =
            std::mem::transmute(objc2::ffi::objc_msgSend as unsafe extern "C-unwind" fn());
        let raw = send(std::ptr::from_ref(challenge).cast(), sel!(protectionSpace));
        Retained::retain(raw)?
    };
    let method = space.authenticationMethod();
    if !method.isEqualToString(unsafe { NSURLAuthenticationMethodServerTrust }) {
        return None;
    }
    let trust: *mut SecTrust = unsafe { msg_send![&*space, serverTrust] };
    if trust.is_null() {
        return None;
    }
    let len = isize::try_from(ca_der.len()).ok()?;
    unsafe {
        let data = CFDataCreate(std::ptr::null(), ca_der.as_ptr(), len);
        if data.is_null() {
            return None;
        }
        let cert = SecCertificateCreateWithData(std::ptr::null(), data);
        CFRelease(data);
        if cert.is_null() {
            return None;
        }
        let arr = CFArrayCreate(
            std::ptr::null(),
            &raw const cert,
            1,
            (&raw const kCFTypeArrayCallBacks).cast(),
        );
        CFRelease(cert);
        if arr.is_null() {
            return None;
        }
        let set = SecTrustSetAnchorCertificates(trust, arr);
        CFRelease(arr);
        if set != 0 || SecTrustSetAnchorCertificatesOnly(trust, 0) != 0 {
            return None;
        }
        let mut err: CFTypeRef = std::ptr::null();
        if !SecTrustEvaluateWithError(trust, &raw mut err) {
            if !err.is_null() {
                CFRelease(err);
            }
            return None;
        }
        msg_send![NSURLCredential::class(), credentialForTrust: trust]
    }
}
