use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;
use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::task::{Context, Poll, Waker};
use std::time::Duration;

use futures::executor::{LocalPool, LocalSpawner};
use futures::future::LocalBoxFuture;
use futures::task::{ArcWake, LocalSpawnExt, waker};

/// Platform adapters supply local scheduling and a monotonic clock.
pub trait Driver {
    fn spawn(&self, future: LocalBoxFuture<'static, ()>);
    fn sleep(&self, duration: Duration) -> LocalBoxFuture<'static, ()>;
    fn now(&self) -> Duration;
}

/// A deterministic local executor. `run_until` advances to pending timers when idle.
pub struct ManualDriver {
    pool: RefCell<Option<LocalPool>>,
    spawner: LocalSpawner,
    clock: Rc<Clock>,
}

#[derive(Default)]
struct Clock {
    now: Cell<Duration>,
    next_timer: Cell<u64>,
    timers: RefCell<BTreeMap<u64, Timer>>,
}

struct Timer {
    deadline: Duration,
    waker: Waker,
}

impl Clock {
    fn advance_to(&self, deadline: Duration) {
        self.now.set(self.now.get().max(deadline));
        let wake = {
            let mut timers = self.timers.borrow_mut();
            let due: Vec<_> = timers
                .iter()
                .filter_map(|(id, timer)| (timer.deadline <= self.now.get()).then_some(*id))
                .collect();
            due.into_iter()
                .filter_map(|id| timers.remove(&id))
                .map(|timer| timer.waker)
                .collect::<Vec<_>>()
        };
        for waker in wake {
            waker.wake();
        }
    }

    fn next_deadline(&self) -> Option<Duration> {
        self.timers
            .borrow()
            .values()
            .map(|timer| timer.deadline)
            .min()
    }
}

impl ManualDriver {
    pub fn new() -> Rc<Self> {
        let pool = LocalPool::new();
        let spawner = pool.spawner();
        Rc::new(Self {
            pool: RefCell::new(Some(pool)),
            spawner,
            clock: Rc::default(),
        })
    }

    /// Drives local work; when idle, jumps to the next manual deadline.
    /// Panics if the future cannot progress through local work or registered timers.
    pub fn run_until<F: Future>(&self, future: F) -> F::Output {
        let mut pool = PoolLease::take(&self.pool);
        let wake = Arc::new(RootWake(AtomicBool::new(true)));
        let root_waker = waker(wake.clone());
        let mut context = Context::from_waker(&root_waker);
        let mut future = std::pin::pin!(future);
        loop {
            wake.0.store(false, Ordering::Relaxed);
            if let Poll::Ready(value) = future.as_mut().poll(&mut context) {
                return value;
            }
            pool.pool().run_until_stalled();
            if wake.0.load(Ordering::Relaxed) {
                continue;
            }
            if let Some(deadline) = self.clock.next_deadline() {
                self.clock.advance_to(deadline);
                continue;
            }
            panic!("manual driver stalled without runnable work or a registered timer");
        }
    }

    pub fn run_until_stalled(&self) {
        PoolLease::take(&self.pool).pool().run_until_stalled();
    }

    /// Moves manual time and wakes due sleepers; poll with `run_until_stalled` afterward.
    pub fn advance(&self, duration: Duration) {
        self.clock.advance_to(self.now().saturating_add(duration));
    }
}

impl Driver for ManualDriver {
    fn spawn(&self, future: LocalBoxFuture<'static, ()>) {
        self.spawner
            .spawn_local(future)
            .expect("manual executor is alive");
    }

    fn sleep(&self, duration: Duration) -> LocalBoxFuture<'static, ()> {
        Box::pin(Sleep {
            clock: self.clock.clone(),
            deadline: self.now().saturating_add(duration),
            timer: None,
        })
    }

    fn now(&self) -> Duration {
        self.clock.now.get()
    }
}

struct RootWake(AtomicBool);

impl ArcWake for RootWake {
    fn wake_by_ref(arc_self: &Arc<Self>) {
        arc_self.0.store(true, Ordering::Relaxed);
    }
}

/// Move the pool out while polling so callbacks can use the independent spawner.
struct PoolLease<'a> {
    slot: &'a RefCell<Option<LocalPool>>,
    pool: Option<LocalPool>,
}

impl<'a> PoolLease<'a> {
    fn take(slot: &'a RefCell<Option<LocalPool>>) -> Self {
        let pool = slot
            .borrow_mut()
            .take()
            .expect("manual executor is already running");
        Self {
            slot,
            pool: Some(pool),
        }
    }

    fn pool(&mut self) -> &mut LocalPool {
        self.pool.as_mut().expect("pool lease owns executor")
    }
}

impl Drop for PoolLease<'_> {
    fn drop(&mut self) {
        *self.slot.borrow_mut() = self.pool.take();
    }
}

struct Sleep {
    clock: Rc<Clock>,
    deadline: Duration,
    timer: Option<u64>,
}

impl Future for Sleep {
    type Output = ();

    fn poll(self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<()> {
        let this = self.get_mut();
        if this.clock.now.get() >= this.deadline {
            if let Some(timer) = this.timer.take() {
                this.clock.timers.borrow_mut().remove(&timer);
            }
            return Poll::Ready(());
        }
        let id = *this.timer.get_or_insert_with(|| {
            let next = this.clock.next_timer.get();
            this.clock
                .next_timer
                .set(next.checked_add(1).expect("timer ID overflow"));
            next
        });
        this.clock.timers.borrow_mut().insert(
            id,
            Timer {
                deadline: this.deadline,
                waker: context.waker().clone(),
            },
        );
        Poll::Pending
    }
}

impl Drop for Sleep {
    fn drop(&mut self) {
        if let Some(timer) = self.timer {
            self.clock.timers.borrow_mut().remove(&timer);
        }
    }
}

#[cfg(test)]
mod tests {
    use futures::FutureExt;

    use super::*;

    #[test]
    fn dropping_pending_sleep_removes_its_registered_waker() {
        let driver = ManualDriver::new();
        let mut sleep = driver.sleep(Duration::from_secs(60));
        assert!(sleep.as_mut().now_or_never().is_none());
        assert_eq!(driver.clock.timers.borrow().len(), 1);
        drop(sleep);
        assert!(driver.clock.timers.borrow().is_empty());
        assert_eq!(driver.now(), Duration::ZERO);
    }

    #[test]
    fn selecting_cancellation_drops_the_losing_timer() {
        let driver = ManualDriver::new();
        let cancellation = crate::context::CancellationToken::new();
        let stop = cancellation.clone();
        driver.spawn(
            async move {
                stop.cancel("requested");
            }
            .boxed_local(),
        );
        let reason = driver.run_until(async {
            let cancelled = cancellation.cancelled();
            let sleep = driver.sleep(Duration::from_secs(60));
            match futures::future::select(cancelled, sleep).await {
                futures::future::Either::Left((reason, _timer)) => reason,
                futures::future::Either::Right(_) => panic!("cancellation should win"),
            }
        });
        assert_eq!(reason.reason, "requested");
        assert!(driver.clock.timers.borrow().is_empty());
        assert_eq!(driver.now(), Duration::ZERO);
    }

    #[test]
    fn a_panicking_polled_future_does_not_lose_the_executor() {
        let driver = ManualDriver::new();
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            driver.run_until(async { panic!("callback failed") });
        }));
        assert!(outcome.is_err());
        assert_eq!(driver.run_until(async { 42 }), 42);
        driver.run_until_stalled();
    }
}
