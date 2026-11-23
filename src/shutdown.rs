//! Ctrl-C and `kill` for `gatefold run`: the first signal stops new work
//! (a run stops after the track or artist at hand) and the web server; a
//! second one exits right away.

use std::sync::Arc;
use std::sync::atomic::{AtomicI32, Ordering};

use tokio_util::sync::CancellationToken;

/// Shared by everything a command runs.
#[derive(Debug, Clone, Default)]
pub struct Shutdown {
    stop: CancellationToken,
    kill: CancellationToken,
    /// Exit code of the first signal: 130 (SIGINT) or 143 (SIGTERM).
    code: Arc<AtomicI32>,
    /// A child's parent's code: a signal stops the whole process.
    parent_code: Option<Arc<AtomicI32>>,
}

/// Wait for SIGINT (130) or SIGTERM (143).
async fn next_signal(term: &mut tokio::signal::unix::Signal) -> i32 {
    tokio::select! {
        _ = tokio::signal::ctrl_c() => 130,
        _ = term.recv() => 143,
    }
}

impl Shutdown {
    /// Listen for SIGINT/SIGTERM in the background.
    #[must_use]
    pub fn listen() -> Self {
        let shutdown = Self::default();
        let handle = shutdown.clone();
        tokio::spawn(async move {
            let Ok(mut term) =
                tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            else {
                return;
            };
            let code = next_signal(&mut term).await;
            eprintln!("stopping; press Ctrl-C again to stop right away");
            handle.stop(code);
            next_signal(&mut term).await;
            eprintln!("stopping right away");
            handle.kill.cancel();
        });
        shutdown
    }

    /// Start stopping, as if a signal with `code` arrived.
    pub fn stop(&self, code: i32) {
        let _ = self
            .code
            .compare_exchange(0, code, Ordering::SeqCst, Ordering::SeqCst);
        self.stop.cancel();
    }

    #[must_use]
    pub fn is_stopping(&self) -> bool {
        self.stop.is_cancelled()
    }

    /// Resolves once stopping started.
    pub async fn stopping(&self) {
        self.stop.cancelled().await;
    }

    /// Resolves once processes must be killed right away.
    pub async fn killing(&self) {
        self.kill.cancelled().await;
    }

    /// The exit code to use, once stopping started.
    #[must_use]
    pub fn code(&self) -> Option<i32> {
        let parent = || self.parent_code.as_ref().map(|c| c.load(Ordering::SeqCst));
        match self.code.load(Ordering::SeqCst) {
            0 => parent().filter(|c| *c != 0),
            code => Some(code),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_first_stop_sets_the_code() {
        let shutdown = Shutdown::default();
        assert!(!shutdown.is_stopping());
        assert_eq!(shutdown.code(), None);
        shutdown.stop(143);
        shutdown.stop(130);
        assert!(shutdown.is_stopping());
        assert_eq!(shutdown.code(), Some(143));
    }
}
