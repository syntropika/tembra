//! Local sources feed kernel-owned bounded queues; callbacks receive managed owner contexts.
use crate::Result;
use std::rc::Rc;

pub type Detach = Box<dyn FnOnce()>;
pub type EventHandler<T> = Rc<dyn Fn(Rc<T>)>;
type Attach<T> = Rc<dyn Fn(EventHandler<T>) -> Result<Detach>>;

/// A local subscription contract. Source attachment returns its physical detach callback.
pub struct EventSource<T>(pub(crate) Attach<T>);
impl<T> Clone for EventSource<T> {
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}
impl<T> EventSource<T> {
    /// Adapt a native event producer without serializing its values.
    pub fn new(subscribe: impl Fn(EventHandler<T>) -> Result<Detach> + 'static) -> Self {
        Self(Rc::new(subscribe))
    }
}

pub(crate) type Size<T> = Rc<dyn Fn(&T) -> Result<usize>>;

/// Admission limits for a managed event connection.
pub struct EventOptions<T> {
    pub(crate) max_pending: usize,
    pub(crate) max_bytes: Option<usize>,
    pub(crate) size: Option<Size<T>>,
    pub(crate) coalesce: bool,
}
impl<T> Default for EventOptions<T> {
    fn default() -> Self {
        Self {
            max_pending: 64,
            max_bytes: None,
            size: None,
            coalesce: false,
        }
    }
}
impl<T> EventOptions<T> {
    /// Bound pending values. The running handler has its own physical ownership.
    pub fn pending(mut self, maximum: usize) -> Self {
        self.max_pending = maximum;
        self
    }
    /// Bound queued bytes using an explicit local sizing function.
    pub fn bytes(mut self, maximum: usize, size: impl Fn(&T) -> Result<usize> + 'static) -> Self {
        self.max_bytes = Some(maximum);
        self.size = Some(Rc::new(size));
        self
    }
    /// Retain only the latest queued value while a handler is running.
    pub fn coalesce(mut self) -> Self {
        self.coalesce = true;
        self
    }
}
