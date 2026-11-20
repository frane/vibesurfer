//! Session lookup for the `vs_net_*` primitives. The primitives
//! themselves are thin calls into [`crate::net::SessionNet`]; see
//! `server/net_ops.rs`.

use std::sync::Arc;

use super::{Daemon, SessionState};
use crate::error::{DaemonError, Result};
use crate::net::SessionNet;

impl SessionState {
    /// The session's proxy, started on first use: at open, or for a
    /// session rebuilt after a restart, when a page or primitive first
    /// needs it.
    pub(crate) fn running_net(&mut self) -> Result<Arc<SessionNet>> {
        if let Some(net) = &self.net {
            return Ok(net.clone());
        }
        let net = SessionNet::start()
            .map_err(|e| DaemonError::Other(anyhow::anyhow!("intercept proxy: {e}")))?;
        self.net = Some(net.clone());
        Ok(net)
    }
}

impl Daemon {
    /// The intercept proxy of `session_id`.
    pub fn net_for(&self, session_id: &str) -> Result<Arc<SessionNet>> {
        self.touch(session_id, None);
        let mut sessions = self.inner.sessions.lock().expect("poisoned");
        let s = sessions
            .get_mut(session_id)
            .ok_or_else(|| DaemonError::UnknownSession(session_id.to_string()))?;
        if !s.intercept {
            return Err(DaemonError::BadRequest(
                "session does not intercept; open it with --intercept".into(),
            ));
        }
        s.running_net()
    }
}
