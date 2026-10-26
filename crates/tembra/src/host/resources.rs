//! Native handles and cooperative opener cancellation.

use super::*;

/// Cooperative cancellation never substitutes for physical opener settlement.
#[derive(Clone)]
pub struct ResourceSignal {
    pub(super) cancelled: Rc<Cell<bool>>,
    pub(super) wait: Shared<LocalBoxFuture<'static, ()>>,
}

impl ResourceSignal {
    /// Check whether the kernel requested opener cancellation.
    pub fn is_cancelled(&self) -> bool {
        self.cancelled.get()
    }

    /// Reject work after resource cancellation was requested.
    pub fn check(&self) -> Result<()> {
        if self.is_cancelled() {
            Err(Error::new(
                core::Code::Cancelled,
                "resource",
                "resource is closing",
            ))
        } else {
            Ok(())
        }
    }

    /// Wait for cancellation without ending the physical opener.
    pub async fn cancelled(&self) {
        self.wait.clone().await;
    }
}

#[derive(Clone)]
pub struct ResourceKey {
    pub(super) host: Host,
    pub(super) id: core::Id,
}

pub struct Resource<T> {
    pub(super) key: ResourceKey,
    pub(super) marker: std::marker::PhantomData<T>,
}
impl<T: 'static> Resource<T> {
    /// Read the typed resource identity for explicit dependency declarations.
    pub fn key(&self) -> ResourceKey {
        self.key.clone()
    }

    /// Resolve the opened value under this resource's ownership.
    pub fn value(&self) -> Result<Rc<T>> {
        self.key
            .host
            .value(self.key.host.kernel().resource_value(self.key.id)?)
    }
}
