//! Lets queued scheduler jobs run from inside an example's own async callback.

use std::{
    future::Future,
    pin::Pin,
    task::{Context as TaskContext, Poll},
};

/// Yields to the scheduler once: pending, then ready on the next poll.
struct YieldNow(bool);

impl Future for YieldNow {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, context: &mut TaskContext<'_>) -> Poll<()> {
        if self.0 {
            return Poll::Ready(());
        }
        self.0 = true;
        context.waker().wake_by_ref();
        Poll::Pending
    }
}

/// Yields repeatedly so that queued jobs, such as a pending object disposal, can complete.
pub async fn let_scheduler_run() {
    for _ in 0..20 {
        YieldNow(false).await;
    }
}
