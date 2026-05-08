//! Mac script-message handler — connects WKWebView's
//! `webkit.messageHandlers.<name>.postMessage(...)` JS bridge to the
//! per-page ring buffers in `crate::inspector`.
//!
//! Two channels: `vsConsole` and `vsNetwork`. Each gets its own
//! `WKScriptMessageHandler` instance; the body is a JSON string that
//! `inspector_bridge::ingest_*` decodes.
//!
//! The handler captures `Rc<RefCell<...>>` slots so it can mutate the
//! buffers from the message callback, which is delivered on the main
//! thread (same thread that pumps the WKWebView event loop).

use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2::{define_class, msg_send, DefinedClass, MainThreadMarker, MainThreadOnly};
use objc2_foundation::{NSObject, NSObjectProtocol, NSString};
use objc2_web_kit::{
    WKScriptMessage, WKScriptMessageHandler, WKUserContentController, WKUserScript,
    WKUserScriptInjectionTime,
};

use crate::backend::inspector_bridge::{
    self, InspectorSlots, NetworkIngestSlot, CONSOLE_HANDLER, NETWORK_HANDLER,
};

#[derive(Copy, Clone, Debug)]
enum Channel {
    Console,
    Network,
}

pub(super) struct HandlerIvars {
    slots: InspectorSlots,
    channel: Channel,
}

define_class!(
    #[unsafe(super = NSObject)]
    #[thread_kind = MainThreadOnly]
    #[name = "VsInspectorMessageHandler"]
    #[ivars = HandlerIvars]
    pub(super) struct InspectorMsgHandler;

    impl InspectorMsgHandler {
        #[unsafe(method(userContentController:didReceiveScriptMessage:))]
        fn did_receive(
            &self,
            _ucc: &WKUserContentController,
            message: &WKScriptMessage,
        ) {
            let body = unsafe { message.body() };
            let Ok(ns_str) = body.downcast::<NSString>() else {
                return;
            };
            let json = ns_str.to_string();
            let ivars = self.ivars();
            match ivars.channel {
                Channel::Console => {
                    let mut buf = ivars.slots.console.borrow_mut();
                    inspector_bridge::ingest_console(&mut buf, &json);
                }
                Channel::Network => {
                    let mut entries = ivars.slots.network.borrow_mut();
                    let mut details = ivars.slots.details.borrow_mut();
                    let mut pending = ivars.slots.pending.borrow_mut();
                    inspector_bridge::ingest_network(
                        NetworkIngestSlot {
                            entries: &mut entries,
                            details: &mut details,
                            pending: &mut pending,
                        },
                        &json,
                    );
                }
            }
        }
    }

    unsafe impl NSObjectProtocol for InspectorMsgHandler {}
    unsafe impl WKScriptMessageHandler for InspectorMsgHandler {}
);

impl InspectorMsgHandler {
    fn new(mtm: MainThreadMarker, slots: InspectorSlots, channel: Channel) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(HandlerIvars { slots, channel });
        unsafe { msg_send![super(this), init] }
    }
}

/// Install the inspector capture script + both message handlers on the
/// given user content controller. Call once per WKWebView, before
/// `loadRequest:`. Returns `true` only if every step succeeded — the
/// caller stamps that bool into the page's capability state, and the
/// daemon's `vs_inspect` gate reads it back to decide whether to
/// dispatch or return `! ENGINE_UNSUPPORTED`.
///
/// Forced-failure mode: setting `VS_DISABLE_INSPECTOR=1` in the
/// environment makes this function short-circuit to `false` without
/// touching the controller. Used by the
/// `cell_engine_unsupported_when_install_disabled` integration test
/// to prove the capability-flag pipeline is honest end-to-end.
pub(super) fn install(
    mtm: MainThreadMarker,
    ucc: &WKUserContentController,
    slots: &InspectorSlots,
) -> bool {
    if std::env::var_os("VS_DISABLE_INSPECTOR").is_some() {
        return false;
    }
    let source = NSString::from_str(inspector_bridge::SCRIPT);
    let user_script = unsafe {
        WKUserScript::initWithSource_injectionTime_forMainFrameOnly(
            WKUserScript::alloc(mtm),
            &source,
            WKUserScriptInjectionTime::AtDocumentStart,
            false,
        )
    };
    unsafe { ucc.addUserScript(&user_script) };

    let console_h = InspectorMsgHandler::new(mtm, slots.clone(), Channel::Console);
    let network_h = InspectorMsgHandler::new(mtm, slots.clone(), Channel::Network);
    let console_proto: &ProtocolObject<dyn WKScriptMessageHandler> =
        ProtocolObject::from_ref(&*console_h);
    let network_proto: &ProtocolObject<dyn WKScriptMessageHandler> =
        ProtocolObject::from_ref(&*network_h);
    let console_name = NSString::from_str(CONSOLE_HANDLER);
    let network_name = NSString::from_str(NETWORK_HANDLER);
    unsafe { ucc.addScriptMessageHandler_name(console_proto, &console_name) };
    unsafe { ucc.addScriptMessageHandler_name(network_proto, &network_name) };
    true
}
