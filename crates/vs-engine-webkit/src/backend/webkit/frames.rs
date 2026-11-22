//! Subframe registry, for evaluating in a frame the page's own script
//! cannot reach (a cross-origin iframe).
//!
//! WebKit has no public call that lists a page's frames, but every
//! script message carries the `WKFrameInfo` of the frame that sent it,
//! and that is what `evaluateJavaScript:inFrame:` takes. A
//! document-start script in every subframe posts its URL once; the
//! handler keeps the newest frame info per report.

use std::cell::RefCell;
use std::rc::Rc;

use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2::{define_class, msg_send, DefinedClass, MainThreadMarker, MainThreadOnly};
use objc2_foundation::{NSObject, NSObjectProtocol, NSString};
use objc2_web_kit::{
    WKFrameInfo, WKScriptMessage, WKScriptMessageHandler, WKUserContentController, WKUserScript,
    WKUserScriptInjectionTime, WKWebView,
};

/// Subframes that reported in: URL and frame info, oldest first.
pub(super) type FrameList = Rc<RefCell<Vec<(String, Retained<WKFrameInfo>)>>>;

const HANDLER: &str = "vsFrame";

/// Posted from every subframe at document start.
const SCRIPT: &str = "(function(){if(window===window.top)return;try{webkit.messageHandlers.vsFrame.postMessage(String(location.href));}catch(e){}})();";

/// Reports kept per content controller. Old ones are dropped first;
/// a page with more live frames than this is not a case worth the
/// memory.
const MAX: usize = 256;

define_class!(
    #[unsafe(super = NSObject)]
    #[thread_kind = MainThreadOnly]
    #[name = "VsFrameMessageHandler"]
    #[ivars = FrameList]
    pub(super) struct FrameMsgHandler;

    impl FrameMsgHandler {
        #[unsafe(method(userContentController:didReceiveScriptMessage:))]
        fn did_receive(&self, _ucc: &WKUserContentController, message: &WKScriptMessage) {
            let body = unsafe { message.body() };
            let Ok(url) = body.downcast::<NSString>() else {
                return;
            };
            let info = unsafe { message.frameInfo() };
            if unsafe { info.isMainFrame() } {
                return;
            }
            let mut list = self.ivars().borrow_mut();
            if list.len() >= MAX {
                list.remove(0);
            }
            list.push((url.to_string(), info));
        }
    }

    unsafe impl NSObjectProtocol for FrameMsgHandler {}
    unsafe impl WKScriptMessageHandler for FrameMsgHandler {}
);

/// Install the reporting script and its handler on `ucc`.
pub(super) fn install(mtm: MainThreadMarker, ucc: &WKUserContentController, list: &FrameList) {
    let source = NSString::from_str(SCRIPT);
    let script = unsafe {
        WKUserScript::initWithSource_injectionTime_forMainFrameOnly(
            WKUserScript::alloc(mtm),
            &source,
            WKUserScriptInjectionTime::AtDocumentStart,
            false,
        )
    };
    unsafe { ucc.addUserScript(&script) };
    let handler = FrameMsgHandler::alloc(mtm).set_ivars(list.clone());
    let handler: Retained<FrameMsgHandler> = unsafe { msg_send![super(handler), init] };
    let proto: &ProtocolObject<dyn WKScriptMessageHandler> = ProtocolObject::from_ref(&*handler);
    unsafe { ucc.addScriptMessageHandler_name(proto, &NSString::from_str(HANDLER)) };
}

/// The newest frame of `web_view` that reported `url` (fragment
/// ignored). `url` may also be `#N`: the Nth frame to report, newest
/// first, from 0.
pub(super) fn find(
    list: &FrameList,
    web_view: &WKWebView,
    url: &str,
) -> Option<Retained<WKFrameInfo>> {
    let strip = |u: &str| u.split('#').next().unwrap_or("").to_string();
    let mine = |info: &WKFrameInfo| {
        unsafe { info.webView() }.is_some_and(|v| std::ptr::eq(Retained::as_ptr(&v), web_view))
    };
    let list = list.borrow();
    let mut candidates = list.iter().rev().filter(|(_, info)| mine(info));
    if let Some(n) = url.strip_prefix('#').and_then(|n| n.parse::<usize>().ok()) {
        return candidates.nth(n).map(|(_, info)| info.clone());
    }
    let want = strip(url);
    candidates
        .find(|(u, _)| strip(u) == want)
        .map(|(_, info)| info.clone())
}
