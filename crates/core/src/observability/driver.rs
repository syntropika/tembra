use super::{Observations, Operation, Outcome};
use crate::Driver;
use std::{rc::Rc, time::Duration};

/// Wraps only Host-owned spawning. Sleep/clock semantics stay with the original adapter.
pub(crate) struct ObservedDriver {
    pub(crate) inner: Rc<dyn Driver>,
    pub(crate) observations: Observations,
}
impl Driver for ObservedDriver {
    fn spawn(&self, future: futures::future::LocalBoxFuture<'static, ()>) {
        self.inner.spawn(Box::pin(ObservedFuture {
            future,
            observations: self.observations.clone(),
        }));
    }
    fn sleep(&self, duration: Duration) -> futures::future::LocalBoxFuture<'static, ()> {
        self.inner.sleep(duration)
    }
    fn now(&self) -> Duration {
        self.inner.now()
    }
}
struct ObservedFuture {
    future: futures::future::LocalBoxFuture<'static, ()>,
    observations: Observations,
}
impl std::future::Future for ObservedFuture {
    type Output = ();
    fn poll(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<()> {
        let this = self.get_mut();
        let mut observation =
            this.observations
                .start(Operation::Executor, "host_task_poll", None, None, None);
        let result = this.future.as_mut().poll(cx);
        if let Some(observation) = &mut observation {
            observation.finish(if result.is_ready() {
                Outcome::Ok
            } else {
                Outcome::Pending
            });
        }
        result
    }
}
