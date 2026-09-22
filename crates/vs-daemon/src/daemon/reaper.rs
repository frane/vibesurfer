//! Idle reaping for sessions and pages.
//!
//! The daemon is auto-spawned and long-lived, and its callers are
//! agents: an agent that crashes, times out, or is killed mid-task
//! never reaches `vs_session_close`, and that is the ordinary way an
//! agent run ends. Until this existed the only thing that ever brought
//! the page count down was a human noticing the fans — one host
//! reached 48 sessions and 162 open pages over four days, with 173
//! WebKit processes and around 8.6 GB of memory, on a machine nobody
//! could work on any more.
//!
//! Two stages, because they cost different things:
//!
//! 1. **An idle page goes dormant.** The engine page is closed and the
//!    row keeps its url, tree and token. This is where the memory and
//!    the processes are, and it is already reversible: a dormant page
//!    is exactly what a session resurrected from SQLite holds, and
//!    [`Daemon::engine_handle_for`](super::Daemon::engine_handle_for)
//!    recreates it on the next call. The agent sees a re-baselined
//!    view, nothing more.
//! 2. **An idle session closes.** After a much longer stretch with
//!    nothing addressing it at all, the session is closed the way
//!    `vs_session_close` closes it. This is not reversible, so its
//!    budget is a day rather than half an hour.
//!
//! Both windows are env-tunable, and `VS_REAP=0` turns the whole thing
//! off for anyone who wants the old behaviour.

use std::time::{Duration, Instant};

use super::Daemon;

/// How long a page may sit untouched before its engine page is closed.
const PAGE_IDLE_DEFAULT: Duration = Duration::from_secs(30 * 60);
/// How long a session may sit untouched before it is closed outright.
const SESSION_TTL_DEFAULT: Duration = Duration::from_secs(24 * 60 * 60);
/// How often the sweep runs.
const SWEEP_DEFAULT: Duration = Duration::from_secs(60);

/// The three windows, resolved once at startup.
#[derive(Debug, Clone, Copy)]
pub(crate) struct ReapConfig {
    pub page_idle: Duration,
    pub session_ttl: Duration,
    pub sweep: Duration,
}

impl ReapConfig {
    /// Read the env overrides. All three are seconds, because the
    /// cells that prove this works cannot wait half an hour.
    pub(crate) fn from_env() -> Option<Self> {
        if std::env::var("VS_REAP").is_ok_and(|v| v == "0") {
            return None;
        }
        let secs = |key: &str, default: Duration| {
            std::env::var(key)
                .ok()
                .and_then(|v| v.parse::<u64>().ok())
                .filter(|s| *s > 0)
                .map_or(default, Duration::from_secs)
        };
        Some(Self {
            page_idle: secs("VS_PAGE_IDLE_SECS", PAGE_IDLE_DEFAULT),
            session_ttl: secs("VS_SESSION_TTL_SECS", SESSION_TTL_DEFAULT),
            sweep: secs("VS_REAP_SWEEP_SECS", SWEEP_DEFAULT),
        })
    }
}

/// What one sweep decided to do. Kept separate from doing it so the
/// decision can be tested without an engine, and so the engine calls
/// happen outside the session-map lock.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct ReapPlan {
    /// Pages whose engine page should be closed, as (session, page).
    pub dormant: Vec<(String, String)>,
    /// Sessions to close outright.
    pub closed: Vec<String>,
}

impl ReapPlan {
    pub(crate) fn is_empty(&self) -> bool {
        self.dormant.is_empty() && self.closed.is_empty()
    }
}

/// Decide what to reap. `idle` answers "how long since anything
/// touched this", which is all the policy needs.
pub(crate) fn plan<'a, S, P>(sessions: S, now: Instant, cfg: ReapConfig) -> ReapPlan
where
    S: IntoIterator<Item = (&'a str, Instant, P)>,
    P: IntoIterator<Item = (&'a str, Instant, bool)>,
{
    let mut out = ReapPlan::default();
    for (session_id, session_touched, pages) in sessions {
        let mut newest = session_touched;
        let mut dormant_here = Vec::new();
        for (page_id, touched, live) in pages {
            newest = newest.max(touched);
            // Already dormant pages cost nothing; leave them.
            if live && now.saturating_duration_since(touched) >= cfg.page_idle {
                dormant_here.push((session_id.to_string(), page_id.to_string()));
            }
        }
        if now.saturating_duration_since(newest) >= cfg.session_ttl {
            // Closing the session takes its pages with it, so there is
            // no point also reporting them as dormant.
            out.closed.push(session_id.to_string());
        } else {
            out.dormant.append(&mut dormant_here);
        }
    }
    out
}

/// Start the sweep thread. Returns without doing anything when
/// reaping is switched off.
pub(crate) fn spawn(daemon: &Daemon) {
    let Some(cfg) = ReapConfig::from_env() else {
        tracing::info!("session reaping disabled (VS_REAP=0)");
        return;
    };
    let daemon = daemon.clone();
    let _ = std::thread::Builder::new()
        .name("vs-reaper".into())
        .spawn(move || loop {
            std::thread::sleep(cfg.sweep);
            daemon.reap_once(cfg);
        });
    tracing::info!(
        page_idle_secs = cfg.page_idle.as_secs(),
        session_ttl_secs = cfg.session_ttl.as_secs(),
        "session reaping armed"
    );
}

impl Daemon {
    /// One sweep: plan under the lock, act outside it.
    pub(crate) fn reap_once(&self, cfg: ReapConfig) {
        let now = Instant::now();
        let plan = {
            let sessions = self.inner.sessions.lock().expect("poisoned");
            plan(
                sessions.iter().map(|(sid, s)| {
                    let pages: Vec<(&str, Instant, bool)> = s
                        .pages
                        .iter()
                        .map(|(pid, p)| (pid.as_str(), p.last_touched, p.engine_handle.is_some()))
                        .collect();
                    (sid.as_str(), s.last_touched, pages)
                }),
                now,
                cfg,
            )
        };
        if plan.is_empty() {
            return;
        }
        for (session_id, page_id) in &plan.dormant {
            // Take the handle under the lock, close it outside: engine
            // calls dispatch to the platform main thread and must not
            // run while holding the session map.
            let handle = {
                let mut sessions = self.inner.sessions.lock().expect("poisoned");
                let Some(page) = sessions
                    .get_mut(session_id)
                    .and_then(|s| s.pages.get_mut(page_id))
                else {
                    continue;
                };
                // The next view must be a fresh full tree: the page the
                // agent comes back to is a new web view.
                page.invalidate_baseline();
                page.engine_handle.take()
            };
            if let Some(h) = handle {
                let _ = self.inner.engine.close(h);
                tracing::info!(session = %session_id, page = %page_id, "reaped idle engine page");
            }
        }
        for session_id in &plan.closed {
            match self.session_close(session_id) {
                Ok(_) => tracing::info!(session = %session_id, "reaped idle session"),
                Err(e) => tracing::warn!(session = %session_id, error = %e, "reap failed"),
            }
        }
    }

    /// Record that `session_id` (and `page_id`, when given) was just
    /// addressed. Called from the two choke points every primitive
    /// passes through, so nothing has to remember to call it.
    pub(crate) fn touch(&self, session_id: &str, page_id: Option<&str>) {
        let mut sessions = self.inner.sessions.lock().expect("poisoned");
        let Some(session) = sessions.get_mut(session_id) else {
            return;
        };
        let now = Instant::now();
        session.last_touched = now;
        if let Some(page_id) = page_id {
            if let Some(page) = session.pages.get_mut(page_id) {
                page.last_touched = now;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(page_idle: u64, session_ttl: u64) -> ReapConfig {
        ReapConfig {
            page_idle: Duration::from_secs(page_idle),
            session_ttl: Duration::from_secs(session_ttl),
            sweep: Duration::from_secs(1),
        }
    }

    /// A page nobody has touched loses its web view; a busy one keeps
    /// it, and a page that is already dormant is not reported twice.
    #[test]
    fn idle_pages_go_dormant_and_busy_ones_stay() {
        let now = Instant::now();
        let old = now.checked_sub(Duration::from_secs(120)).expect("test clock");
        let plan = plan(
            vec![(
                "s_1",
                old,
                vec![
                    ("p_idle", old, true),
                    ("p_busy", now, true),
                    ("p_already_dormant", old, false),
                ],
            )],
            now,
            cfg(60, 3600),
        );
        assert_eq!(plan.dormant, [("s_1".to_string(), "p_idle".to_string())]);
        assert!(plan.closed.is_empty());
    }

    /// A session is kept alive by its busiest page: an agent working
    /// one tab for hours must not have the session closed under it.
    #[test]
    fn a_busy_page_keeps_its_session() {
        let now = Instant::now();
        let ancient = now.checked_sub(Duration::from_secs(10_000)).expect("test clock");
        let plan = plan(
            vec![("s_1", ancient, vec![("p_busy", now, true)])],
            now,
            cfg(60, 600),
        );
        assert!(plan.closed.is_empty(), "session still in use");
        assert!(plan.dormant.is_empty(), "its page is not idle either");
    }

    /// The empty sessions are the ones that piled up: 48 of them, most
    /// holding nothing at all, because a session outlives the agent
    /// that opened it and nothing else ever closed it.
    #[test]
    fn an_empty_idle_session_is_closed() {
        let now = Instant::now();
        let old = now.checked_sub(Duration::from_secs(10_000)).expect("test clock");
        let plan = plan(
            vec![("s_empty", old, Vec::new()), ("s_fresh", now, Vec::new())],
            now,
            cfg(60, 600),
        );
        assert_eq!(plan.closed, ["s_empty"]);
    }

    /// A closing session takes its pages with it, so the plan must not
    /// also ask for them to be made dormant first.
    #[test]
    fn a_closing_session_does_not_also_report_its_pages() {
        let now = Instant::now();
        let old = now.checked_sub(Duration::from_secs(10_000)).expect("test clock");
        let plan = plan(
            vec![("s_1", old, vec![("p_1", old, true)])],
            now,
            cfg(60, 600),
        );
        assert_eq!(plan.closed, ["s_1"]);
        assert!(plan.dormant.is_empty());
    }
}
