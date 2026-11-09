//! Isolate blocking source I/O from Node's libuv pool and bound native worker growth.
use crate::failure;
use std::sync::OnceLock;
use tembra::Result;

type Job = Box<dyn FnOnce(Option<String>) + Send>;
const WORKERS: usize = 4;
const QUEUE: usize = 128;

struct Pool(async_channel::Sender<Job>);
impl Pool {
    fn new(workers: usize, capacity: usize) -> std::io::Result<Self> {
        let (send, receive) = async_channel::bounded::<Job>(capacity);
        for index in 0..workers {
            let receive = receive.clone();
            std::thread::Builder::new()
                .name(format!("tembra-io-{index}"))
                .spawn(move || {
                    while let Ok(job) = receive.recv_blocking() {
                        let _ =
                            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| job(None)));
                    }
                })?;
        }
        Ok(Self(send))
    }
}

/// Every accepted job retains its original capabilities and completion callback until it settles.
pub(super) fn submit(
    operation: super::IoOperation,
    complete: impl FnOnce(Result<serde_json::Value>) + Send + 'static,
) {
    static POOL: OnceLock<std::result::Result<Pool, String>> = OnceLock::new();
    let pool = POOL.get_or_init(|| Pool::new(WORKERS, QUEUE).map_err(|error| error.to_string()));
    let job: Job = Box::new(move |rejected: Option<String>| {
        let outcome = if let Some(error) = rejected {
            Err(failure(error))
        } else {
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| operation.run()))
                .unwrap_or_else(|_| Err(failure("host I/O worker panicked")))
        };
        complete(outcome);
    });
    match pool {
        Ok(pool) => {
            if let Err(error) = pool.0.try_send(job) {
                error.into_inner()(Some("host I/O capacity is exhausted".into()));
            }
        }
        Err(error) => job(Some(format!("host I/O pool is unavailable: {error}"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex, mpsc};
    use std::time::Duration;

    #[test]
    fn workers_and_queued_work_are_bounded() {
        let pool = Pool::new(2, 1).unwrap();
        let (started, observed) = mpsc::channel();
        let (release, wait) = mpsc::channel();
        let wait = Arc::new(Mutex::new(wait));
        for _ in 0..2 {
            let started = started.clone();
            let wait = wait.clone();
            pool.0
                .try_send(Box::new(move |_| {
                    started.send(()).unwrap();
                    let _ = wait.lock().unwrap().recv();
                }))
                .unwrap();
            observed.recv_timeout(Duration::from_secs(2)).unwrap();
        }
        pool.0.try_send(Box::new(|_| {})).unwrap();
        assert!(
            pool.0
                .try_send(Box::new(|_| panic!("must not execute")))
                .is_err()
        );
        release.send(()).unwrap();
        release.send(()).unwrap();
        drop(pool);
    }
}
