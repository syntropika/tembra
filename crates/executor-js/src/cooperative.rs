//! A local queue shared by the native background pump and the embedded runtime.
use futures::{
    StreamExt,
    channel::mpsc,
    stream::FuturesUnordered,
    task::{ArcWake, AtomicWaker, LocalFutureObj, LocalSpawn, SpawnError, waker},
};
use std::{
    cell::{Cell, RefCell},
    future::{Future, poll_fn},
    pin::Pin,
    rc::{Rc, Weak},
    sync::Arc,
    task::{Context, Poll},
};
use tokio::task::LocalSet;

const POLL_BUDGET: usize = 64;

#[derive(Default)]
struct Wake {
    background: AtomicWaker,
    runtime: AtomicWaker,
}
impl ArcWake for Wake {
    fn wake_by_ref(this: &Arc<Self>) {
        this.background.wake();
        this.runtime.wake();
    }
}

struct Task {
    future: LocalFutureObj<'static, ()>,
    budget: Rc<Cell<usize>>,
}
impl Future for Task {
    type Output = ();
    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        let remaining = self.budget.get();
        if remaining == 0 {
            cx.waker().wake_by_ref();
            return Poll::Pending;
        }
        self.budget.set(remaining - 1);
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            Pin::new(&mut self.future).poll(cx)
        }))
        .unwrap_or(Poll::Ready(()))
    }
}

struct Queue {
    incoming: RefCell<mpsc::UnboundedReceiver<LocalFutureObj<'static, ()>>>,
    running: RefCell<FuturesUnordered<Task>>,
    local: Rc<LocalSet>,
    wake: Arc<Wake>,
    budget: Rc<Cell<usize>>,
    disconnected: Cell<bool>,
    queued: Cell<usize>,
    polling: Cell<bool>,
}

impl Queue {
    /// Poll each task in the same local context, independent of the caller of Runtime.
    fn drain(&self) -> Poll<()> {
        if self.polling.replace(true) {
            return Poll::Pending;
        }
        struct Reset<'a>(&'a Cell<bool>);
        impl Drop for Reset<'_> {
            fn drop(&mut self) {
                self.0.set(false);
            }
        }
        let _reset = Reset(&self.polling);
        let _entered = self.local.enter();
        let wake = waker(self.wake.clone());
        let mut cx = Context::from_waker(&wake);
        let mut running = self.running.borrow_mut();
        let mut incoming = self.incoming.borrow_mut();
        self.budget.set(POLL_BUDGET);
        for _ in 0..POLL_BUDGET {
            match incoming.poll_next_unpin(&mut cx) {
                Poll::Ready(Some(future)) => {
                    self.queued.set(self.queued.get() - 1);
                    running.push(Task {
                        future,
                        budget: self.budget.clone(),
                    });
                }
                Poll::Ready(None) => {
                    self.disconnected.set(true);
                    break;
                }
                Poll::Pending => break,
            }
        }
        while self.budget.get() != 0 {
            match running.poll_next_unpin(&mut cx) {
                Poll::Ready(Some(())) => {}
                _ => break,
            }
        }
        if self.disconnected.get() && running.is_empty() {
            Poll::Ready(())
        } else {
            if self.budget.get() == 0 {
                ArcWake::wake_by_ref(&self.wake);
            }
            Poll::Pending
        }
    }
}

/// Local native execution with an independent background pump and bounded runtime polling.
/// Construct inside a running Tokio LocalSet and keep that set running through cleanup.
#[derive(Clone)]
pub struct CooperativeExecutor {
    send: mpsc::UnboundedSender<LocalFutureObj<'static, ()>>,
    queue: Weak<Queue>,
    wake: Arc<Wake>,
}
impl CooperativeExecutor {
    /// Start the background pump. Runtime can also poll the same queue without transferring work.
    pub fn new() -> Self {
        let (send, incoming) = mpsc::unbounded();
        let queue = Rc::new(Queue {
            incoming: RefCell::new(incoming),
            running: RefCell::new(FuturesUnordered::new()),
            local: Rc::new(LocalSet::new()),
            wake: Arc::default(),
            budget: Rc::new(Cell::new(0)),
            disconnected: Cell::new(false),
            queued: Cell::new(0),
            polling: Cell::new(false),
        });
        let background = queue.clone();
        tokio::task::spawn_local(async move {
            let local = background.local.clone();
            local
                .run_until(poll_fn(|cx| {
                    background.wake.background.register(cx.waker());
                    background.drain()
                }))
                .await;
        });
        Self {
            send,
            queue: Rc::downgrade(&queue),
            wake: queue.wake.clone(),
        }
    }

    pub(crate) fn has_pending(&self) -> bool {
        self.queue
            .upgrade()
            .is_some_and(|queue| !queue.running.borrow().is_empty() || queue.queued.get() != 0)
    }

    pub(crate) fn poll_ready(&self, cx: &mut Context<'_>) {
        self.wake.runtime.register(cx.waker());
        if let Some(queue) = self.queue.upgrade() {
            let _ = queue.drain();
        }
    }

    pub(crate) fn wake_runtime(&self) {
        self.wake.runtime.wake();
    }
}
impl Default for CooperativeExecutor {
    fn default() -> Self {
        Self::new()
    }
}
impl LocalSpawn for CooperativeExecutor {
    fn spawn_local_obj(&self, future: LocalFutureObj<'static, ()>) -> Result<(), SpawnError> {
        let queue = self.queue.upgrade().ok_or_else(SpawnError::shutdown)?;
        queue.queued.set(queue.queued.get() + 1);
        if self.send.unbounded_send(future).is_err() {
            queue.queued.set(queue.queued.get() - 1);
            return Err(SpawnError::shutdown());
        }
        Ok(())
    }
}
