//! Native handles and cooperative opener cancellation.

use super::*;

/// A cooperative cancellation signal. Cancellation does not acknowledge physical settlement.
#[derive(Clone)]
pub struct Cancellation(pub(super) Rc<CancellationState>);

pub(super) struct CancellationState {
    cancelled: Cell<bool>,
    reason: &'static str,
    send: RefCell<Option<oneshot::Sender<()>>>,
    wait: Shared<LocalBoxFuture<'static, ()>>,
}

/// Resource openers use the same cooperative signal as contexts and declared tasks.
pub type ResourceSignal = Cancellation;

impl Cancellation {
    pub(super) fn new(reason: &'static str) -> Self {
        let (send, receive) = oneshot::channel();
        Self(Rc::new(CancellationState {
            cancelled: Cell::new(false),
            reason,
            send: RefCell::new(Some(send)),
            wait: async move {
                let _ = receive.await;
            }
            .boxed_local()
            .shared(),
        }))
    }

    pub(super) fn cancel(&self) {
        self.0.cancelled.set(true);
        if let Some(send) = self.0.send.borrow_mut().take() {
            let _ = send.send(());
        }
    }

    /// Check whether cancellation has been requested.
    pub fn is_cancelled(&self) -> bool {
        self.0.cancelled.get()
    }

    /// Reject work after cancellation was requested.
    pub fn check(&self) -> Result<()> {
        if self.is_cancelled() {
            Err(Error::new(
                core::Code::Cancelled,
                self.0.reason,
                "operation was cancelled",
            ))
        } else {
            Ok(())
        }
    }

    /// Wait for cancellation without ending physical work.
    pub async fn cancelled(&self) {
        self.0.wait.clone().await;
    }
}

#[derive(Clone)]
pub struct ResourceKey {
    pub(super) host: Host,
    pub(super) id: core::Id,
    pub(super) version: u32,
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

impl<T> Clone for Resource<T> {
    fn clone(&self) -> Self {
        Self {
            key: self.key.clone(),
            marker: std::marker::PhantomData,
        }
    }
}
impl<T> Resource<T> {
    /// Read the declared resource contract version.
    pub fn version(&self) -> u32 {
        self.key.version
    }
}
