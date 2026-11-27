//! The daemon's first recovery step (§5.3 step 2).

use openchef_core::Session;
use openchef_procs::{Identity, kill_if_alive};

use crate::engine::{Engine, Error};

/// Kills a session's process if it is still the recorded one (pid and start token
/// both match, so a recycled pid is never hit).
pub(crate) fn kill_leftover(s: &Session) -> Result<bool, Error> {
    match kill_if_alive(&Identity {
        pid: s.pid,
        start_token: s.start_token,
    }) {
        Ok(alive) => Ok(alive),
        Err(openchef_procs::Error::Unsupported) => Ok(false),
        Err(e) => Err(Error::Session(format!(
            "kill leftover pid {} of session {}: {e}",
            s.pid, s.id
        ))),
    }
}

impl Engine {
    /// Every session the store still records as live belonged to a previous daemon: its
    /// process is killed if it is still the same process, and the session is marked
    /// closed. Turns and attempts stay as they are; `run` resumes them. Returns how many
    /// processes were still alive.
    ///
    /// # Errors
    ///
    /// The first failure; the other sessions are still handled.
    pub async fn kill_leftovers(&self) -> Result<usize, Error> {
        let live = self.cfg.store.live_sessions().await?;
        let mut killed = 0;
        let mut first: Option<Error> = None;
        for s in &live {
            match kill_leftover(s) {
                Ok(alive) => killed += usize::from(alive),
                Err(e) => {
                    first.get_or_insert(e);
                    continue;
                }
            }
            if let Err(e) = self.cfg.store.close_session(s.id).await {
                first.get_or_insert(e.into());
            }
        }
        first.map_or(Ok(killed), Err)
    }
}
