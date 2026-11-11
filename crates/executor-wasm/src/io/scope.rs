//! The callback epoch of one installed Orb revision.
//!
//! Each admitted guest callback opens an epoch. Standard WASI calls are admitted only while it is
//! active, and its end (completion, abandonment or a caller's cancellation) revokes the callback's
//! pending standard work. Epochs are never reused, so a stale owner can never end a later one.
use std::sync::{Arc, Mutex};
use tokio::sync::watch;

/// The callback epoch shared by an Orb's Store and its embedding host.
#[derive(Clone)]
pub(crate) struct CallbackScope(Arc<Shared>);

struct Shared {
    /// Last opened epoch.
    epoch: Mutex<u64>,
    /// The active epoch, or `None` between callbacks.
    active: watch::Sender<Option<u64>>,
}

impl Default for CallbackScope {
    fn default() -> Self {
        Self(Arc::new(Shared {
            epoch: Mutex::new(0),
            active: watch::Sender::new(None),
        }))
    }
}

impl CallbackScope {
    /// Open the epoch of a callback that has just been admitted on this Orb's Store.
    pub(crate) fn begin_callback(&self) {
        let mut epoch = self
            .0
            .epoch
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        *epoch += 1;
        self.0.active.send_replace(Some(*epoch));
    }

    /// End the active epoch. Idempotent.
    pub(crate) fn end_callback(&self) {
        self.0.active.send_replace(None);
    }

    /// Whether a callback is active (tests).
    #[cfg(test)]
    pub(crate) fn active(&self) -> bool {
        self.0.active.borrow().is_some()
    }

    /// Capture this callback's epoch now and wake when its owner ends it.
    pub(crate) fn cancelled(&self) -> impl std::future::Future<Output = ()> + Send + 'static {
        let mut active = self.0.active.subscribe();
        let epoch = *active.borrow();
        async move {
            if epoch.is_some() {
                let _ = active.wait_for(|active| *active != epoch).await;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_ended_epoch_is_never_reopened_by_a_stale_owner() {
        let scope = CallbackScope::default();
        assert!(!scope.active());
        scope.begin_callback();
        let first = scope.cancelled();
        scope.end_callback();
        futures::executor::block_on(first);
        scope.begin_callback();
        let second = scope.cancelled();
        assert!(scope.active());
        // The first epoch's watcher resolved; the second is still pending.
        assert!(futures::FutureExt::now_or_never(second).is_none());
    }
}
