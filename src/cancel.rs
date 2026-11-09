//! A cancellation flag shared by whoever stops the work (a signal handler)
//! and the work itself (the agent loop, the task runner).

use std::sync::Arc;

use tokio::sync::watch;

/// Cheap to clone; every clone sees the same flag.
#[derive(Clone)]
pub struct Cancel(Arc<watch::Sender<bool>>);

impl Default for Cancel {
    fn default() -> Self {
        Self(Arc::new(watch::channel(false).0))
    }
}

impl Cancel {
    pub fn new() -> Self {
        Self::default()
    }

    /// Asks the work to stop.
    pub fn cancel(&self) {
        self.0.send_replace(true);
    }

    /// Clears the flag, so the next piece of work runs (a new turn).
    pub fn reset(&self) {
        self.0.send_replace(false);
    }

    pub fn is_cancelled(&self) -> bool {
        *self.0.borrow()
    }

    /// Resolves once the flag is set (at once if it already is).
    pub async fn cancelled(&self) {
        let mut flag = self.0.subscribe();
        // The sender lives in `self`, so this cannot fail while awaited.
        let _ = flag.wait_for(|&cancelled| cancelled).await;
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    #[tokio::test]
    async fn clones_share_the_flag_and_reset_clears_it() {
        let cancel = Cancel::new();
        let other = cancel.clone();
        assert!(!other.is_cancelled());
        let waiter = tokio::spawn(async move { other.cancelled().await });
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(!waiter.is_finished());
        cancel.cancel();
        tokio::time::timeout(Duration::from_secs(2), waiter)
            .await
            .unwrap()
            .unwrap();
        // Already set: resolves at once.
        cancel.cancelled().await;
        cancel.reset();
        assert!(!cancel.is_cancelled());
    }
}
