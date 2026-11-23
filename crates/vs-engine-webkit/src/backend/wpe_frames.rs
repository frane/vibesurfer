//! Evaluate in one subframe, cross-origin ones included, on WebKitGTK.
//! WebKitGTK's UI-process API evaluates in the main frame only, so every
//! subframe runs a small agent: a document-start user script, in an
//! isolated world of its own (the page's CSP does not apply there),
//! that keeps one message to the UI pending at all times. The reply to
//! that message is the next piece of code to run; the agent runs it,
//! sends the result, and waits again. A frame is picked the way the
//! macOS backend picks one: by URL (the newest frame with it), or `#N`
//! for the Nth subframe, newest first.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use webkit6::javascriptcore;
use webkit6::prelude::*;
use webkit6::{
    ScriptMessageReply, UserContentInjectedFrames, UserContentManager, UserScript,
    UserScriptInjectionTime, WebView,
};

const WORLD: &str = "vs-frames";
const HANDLER: &str = "vsframe";

thread_local! {
    /// The registry of each content manager the agent is installed on.
    /// A popup's view shares its opener's content manager, so it shares
    /// the registry too. Entries for freed managers are dropped as found.
    static INSTALLED: RefCell<Vec<(glib::WeakRef<UserContentManager>, FrameAgents)>> =
        const { RefCell::new(Vec::new()) };
}

/// The agent every subframe runs (not the top document).
const AGENT_JS: &str = r"(function () {
  if (window.top === window) return;
  var h = window.webkit && window.webkit.messageHandlers && window.webkit.messageHandlers.vsframe;
  if (!h) return;
  var aid = Math.random().toString(36).slice(2) + Date.now().toString(36);
  function send(m) {
    m.aid = aid;
    m.url = location.href;
    h.postMessage(m).then(function (job) {
      if (!job || job.id === undefined) { send({ k: 'poll' }); return; }
      var ok = true, v;
      try { v = String((0, eval)(job.code)); } catch (e) { ok = false; v = String((e && e.message) || e); }
      send({ k: 'result', id: job.id, ok: ok, v: v });
    }, function () { setTimeout(function () { send({ k: 'poll' }); }, 50); });
  }
  send({ k: 'poll' });
})();";

/// One subframe document's agent.
struct Agent {
    url: String,
    /// When the agent first reported, for "newest first".
    born: u64,
    /// Its waiting message: replying hands it code.
    pending: Option<(ScriptMessageReply, javascriptcore::Context)>,
}

#[derive(Default)]
pub(super) struct FrameAgentsState {
    agents: HashMap<String, Agent>,
    seq: u64,
    next_job: u64,
    results: HashMap<u64, Result<String, String>>,
}

pub(super) type FrameAgents = Rc<RefCell<FrameAgentsState>>;

/// Install the agent on `web_view`'s content manager (once: a popup
/// shares its opener's) and return the registry its messages fill.
pub(super) fn install(web_view: &WebView) -> Option<FrameAgents> {
    let ucm = web_view.user_content_manager()?;
    let known = INSTALLED.with(|list| {
        let mut list = list.borrow_mut();
        list.retain(|(w, _)| w.upgrade().is_some());
        list.iter()
            .find(|(w, _)| w.upgrade().as_ref() == Some(&ucm))
            .map(|(_, a)| a.clone())
    });
    if known.is_some() {
        return known;
    }
    let agents = FrameAgents::default();
    if !ucm.register_script_message_handler_with_reply(HANDLER, Some(WORLD)) {
        return None;
    }
    let sink = agents.clone();
    ucm.connect_script_message_with_reply_received(Some(HANDLER), move |_, value, reply| {
        on_message(&sink, value, reply);
        true
    });
    ucm.add_script(&UserScript::for_world(
        AGENT_JS,
        UserContentInjectedFrames::AllFrames,
        UserScriptInjectionTime::Start,
        WORLD,
        &[],
        &[],
    ));
    INSTALLED.with(|list| list.borrow_mut().push((ucm.downgrade(), agents.clone())));
    Some(agents)
}

fn on_message(agents: &FrameAgents, value: &javascriptcore::Value, reply: &ScriptMessageReply) {
    let text = |k: &str| value.object_get_property(k).map(|v| v.to_str().to_string());
    let (Some(aid), Some(url), Some(kind)) = (text("aid"), text("url"), text("k")) else {
        return;
    };
    let Some(ctx) = value.context() else {
        return;
    };
    let mut s = agents.borrow_mut();
    if kind == "result" {
        let id = text("id").and_then(|v| v.parse::<u64>().ok());
        let ok = value
            .object_get_property("ok")
            .is_some_and(|v| v.to_boolean());
        if let Some(id) = id {
            let v = text("v").unwrap_or_default();
            s.results.insert(id, if ok { Ok(v) } else { Err(v) });
        }
    }
    let born = if let Some(a) = s.agents.get(&aid) {
        a.born
    } else {
        s.seq += 1;
        s.seq
    };
    s.agents.insert(
        aid,
        Agent {
            url,
            born,
            pending: Some((reply.clone(), ctx)),
        },
    );
}

/// Hand `js` to the agent of `frame` (URL, or `#N`). Returns the job id
/// to wait for, or `None` when no agent of that frame is waiting.
pub(super) fn dispatch(agents: &FrameAgents, frame: &str, js: &str) -> Option<u64> {
    let mut s = agents.borrow_mut();
    let mut waiting: Vec<(&String, &Agent)> = s
        .agents
        .iter()
        .filter(|(_, a)| a.pending.is_some())
        .collect();
    waiting.sort_by_key(|a| std::cmp::Reverse(a.1.born));
    let aid = match frame.strip_prefix('#') {
        Some(n) => waiting
            .get(n.parse::<usize>().ok()?)
            .map(|(k, _)| (*k).clone()),
        None => waiting
            .iter()
            .find(|(_, a)| a.url == frame)
            .map(|(k, _)| (*k).clone()),
    }?;
    s.next_job += 1;
    let id = s.next_job;
    let (reply, ctx) = s.agents.get_mut(&aid)?.pending.take()?;
    let job = serde_json::json!({ "id": id, "code": js }).to_string();
    reply.return_value(&javascriptcore::Value::from_json(&ctx, &job));
    Some(id)
}

/// The result of job `id`, once its agent sent it.
pub(super) fn take_result(agents: &FrameAgents, id: u64) -> Option<Result<String, String>> {
    agents.borrow_mut().results.remove(&id)
}

/// Whether an agent of `frame` is waiting for code.
pub(super) fn has_agent(agents: &FrameAgents, frame: &str) -> bool {
    let s = agents.borrow();
    let waiting = s.agents.values().filter(|a| a.pending.is_some());
    match frame.strip_prefix('#') {
        Some(n) => n.parse::<usize>().is_ok_and(|n| waiting.count() > n),
        None => s
            .agents
            .values()
            .any(|a| a.pending.is_some() && a.url == frame),
    }
}
