//! A compiled export's `stream<u8>` result consumed by native Rust as a `ByteStream`.
//!
//! The fixture is a real component built from `rust-stream-producer` against `tests/wit/streams`,
//! supplied as `TEMBRA_WASM_STREAM_PRODUCER`. Every case runs on Cranelift and on Pulley, through
//! the real host, kernel references and roots. The caller receives the stream end as soon as the
//! guest returned it; the producer keeps writing in the same admitted call, which ends only after
//! its task group and its pipe have ended.
use futures::{FutureExt, StreamExt, executor::LocalPool, task::LocalSpawnExt};
use std::{
    cell::{Cell, RefCell},
    rc::Rc,
    time::{Duration, Instant},
};
use tembra::{Host, Reference, core, portable::ByteStream};
use tembra_executor_wasm::{Contract, Engine, Install, Limits};

const SERVICE: &str = "frames::tembra-test:streams/frames@1.0.0";
/// The pipe's byte capacity (`worker::standard::PIPE_BYTES`).
const PIPE_BYTES: usize = 64 << 10;

/// Producer modes, as documented in `streams.wit`.
const CLEAN: u32 = 0;
const TRAP_HOLDING: u32 = 1;
const TRAP_AFTER_END: u32 = 2;
const WAIT_FOR_READER: u32 = 3;
const IDLE: u32 = 4;
const TRAP_BEFORE_HEAD: u32 = 5;

fn producer() -> Option<Vec<u8>> {
    let Some(path) =
        std::env::var_os("TEMBRA_WASM_STREAM_PRODUCER").filter(|path| !path.is_empty())
    else {
        eprintln!("skipped: TEMBRA_WASM_STREAM_PRODUCER names no compiled stream producer");
        return None;
    };
    Some(std::fs::read(path).unwrap())
}

struct Fixture {
    pool: LocalPool,
    host: Host,
    engine: Engine,
    reference: Rc<Reference>,
}

fn install(bytes: &[u8], interpreted: bool, timeout_ms: u64) -> Fixture {
    let limits = Limits {
        timeout_ms,
        ..Limits::default()
    };
    let engine = if interpreted {
        Engine::interpreted(limits)
    } else {
        Engine::new(limits)
    }
    .unwrap();
    let pool = LocalPool::new();
    let host = Host::new(pool.spawner());
    host.install(vec![orb(&engine, bytes)]).unwrap();
    let reference = Rc::new(host.get(SERVICE, vec!["main".into()], "host").unwrap());
    let mut fixture = Fixture {
        pool,
        host,
        engine,
        reference,
    };
    let reference = fixture.reference.clone();
    fixture.pool.run_until(reference.ready()).unwrap();
    fixture
}

fn orb(engine: &Engine, bytes: &[u8]) -> tembra::Orb {
    let contract = Contract::from_dir(
        concat!(env!("CARGO_MANIFEST_DIR"), "/tests/wit/streams"),
        Some("producer"),
    )
    .unwrap();
    engine
        .load(
            &contract
                .component_artifact(bytes, "frames", 1, &Default::default())
                .unwrap(),
            Install::default(),
        )
        .unwrap()
}

/// Run `body` once on Cranelift and once on Pulley.
fn engines(body: impl Fn(&[u8], bool)) {
    let Some(bytes) = producer() else { return };
    for interpreted in [false, true] {
        body(&bytes, interpreted);
    }
}

impl Fixture {
    /// Run a root as `alice`.
    fn root<T: 'static>(
        &mut self,
        operation: impl AsyncFnOnce(tembra::Context, Rc<Reference>, Host) -> tembra::Result<T> + 'static,
    ) -> tembra::Result<T> {
        let (reference, host) = (self.reference.clone(), self.host.clone());
        self.pool.run_until(self.host.root("alice", move |context| {
            operation(context, reference, host).boxed_local()
        }))
    }

    fn calls(&self) -> usize {
        self.host.kernel().diagnostics().calls
    }

    /// Let the Store's thread work until `done` holds, polling this host meanwhile.
    fn wait(&mut self, mut done: impl FnMut(&Self) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(20);
        while !done(self) {
            assert!(Instant::now() < deadline, "the condition never held");
            self.pool.run_until_stalled();
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    /// The revision serves an ordinary scalar call.
    fn ping(&mut self) -> tembra::Result<u32> {
        self.root(async move |context, reference, _| {
            reference
                .call_portable::<_, u32>(&context, "ping", (41_u32,))
                .await
        })
    }

    /// Close the host; every kernel record and Store must be gone.
    fn close(mut self) {
        self.reference.release().unwrap();
        self.pool.run_until(self.host.close()).unwrap();
        let diagnostics = self.host.kernel().diagnostics();
        assert_eq!(
            (diagnostics.calls, diagnostics.references, diagnostics.roots),
            (0, 0, 0)
        );
        assert_eq!(self.engine.instances(), 0);
    }
}

/// Byte `j` of chunk `i` is `(i + j) mod 256`.
fn expected(offset: usize, size: usize) -> u8 {
    ((offset / size + offset % size) % 256) as u8
}

async fn open(
    context: &tembra::Context,
    reference: &Reference,
    count: u32,
    size: u32,
    mode: u32,
) -> tembra::Result<ByteStream> {
    reference
        .call_portable::<_, ByteStream>(context, "frames", (count, size, mode))
        .await
}

#[test]
#[ignore = "requires TEMBRA_WASM_STREAM_PRODUCER"]
fn the_head_arrives_first_and_the_pipe_bounds_the_producer() {
    engines(|bytes, interpreted| {
        let mut fixture = install(bytes, interpreted, 30_000);
        let (calls_at_head, after_pause, received) = fixture
            .root(async move |context, reference, host| {
                let mut stream = open(&context, &reference, 64, 16 << 10, CLEAN).await?;
                // The head is here while the producer's call is still admitted.
                let calls_at_head = host.kernel().diagnostics().calls;
                let mut received = stream.next().await.unwrap()?;
                // Not reading: the producer can run ahead only by the pipe's capacity.
                std::thread::sleep(Duration::from_millis(200));
                let after_pause = stream.next().await.unwrap()?;
                received.extend_from_slice(&after_pause);
                while let Some(chunk) = stream.next().await {
                    received.extend(chunk?);
                }
                Ok((calls_at_head, after_pause.len(), received))
            })
            .unwrap();
        assert_eq!(calls_at_head, 1, "{interpreted}");
        assert_eq!(
            after_pause, PIPE_BYTES,
            "{interpreted}: exactly one full pipe"
        );
        assert_eq!(received.len(), 64 << 14);
        assert!(
            received
                .iter()
                .enumerate()
                .all(|(offset, byte)| *byte == expected(offset, 16 << 10))
        );
        fixture.wait(|fixture| fixture.calls() == 0);
        assert_eq!(fixture.ping().unwrap(), 42);
        assert!(fixture.host.errors().is_empty());
        fixture.close();
    });
}

#[test]
#[ignore = "requires TEMBRA_WASM_STREAM_PRODUCER"]
fn closing_or_dropping_the_end_lets_the_producer_finish_healthily() {
    engines(|bytes, interpreted| {
        let mut fixture = install(bytes, interpreted, 30_000);
        // An explicit close returns once the producer's call has physically ended.
        let closed = fixture
            .root(async move |context, reference, _| {
                let mut stream = open(&context, &reference, 1000, 4096, CLEAN).await?;
                stream.next().await.unwrap()?;
                Ok(stream.close().await)
            })
            .unwrap();
        assert!(closed.is_ok(), "{interpreted}: {closed:?}");
        fixture.wait(|fixture| fixture.calls() == 0);
        // A dropped end is the same request to stop.
        fixture
            .root(async move |context, reference, _| {
                let mut stream = open(&context, &reference, 1000, 4096, CLEAN).await?;
                stream.next().await.unwrap()?;
                Ok(())
            })
            .unwrap();
        fixture.wait(|fixture| fixture.calls() == 0);
        assert_eq!(fixture.ping().unwrap(), 42);
        assert!(
            fixture.host.errors().is_empty(),
            "a consumer close is healthy"
        );
        fixture.close();
    });
}

#[test]
#[ignore = "requires TEMBRA_WASM_STREAM_PRODUCER"]
fn a_refused_or_lost_head_still_drives_its_call_to_the_end() {
    engines(|bytes, interpreted| {
        let mut fixture = install(bytes, interpreted, 30_000);
        let refused = fixture
            .root(async move |context, reference, _| {
                Ok(reference
                    .call_portable::<_, serde_json::Value>(
                        &context,
                        "frames",
                        (1000_u32, 4096_u32, CLEAN),
                    )
                    .await)
            })
            .unwrap();
        assert!(
            refused.is_err(),
            "{interpreted}: data callers cannot take a stream"
        );
        fixture.wait(|fixture| fixture.calls() == 0);
        // A caller that goes away before or after the head.
        let (abort, registration) = futures::future::AbortHandle::new_pair();
        let caller = {
            let (host, reference) = (fixture.host.clone(), fixture.reference.clone());
            futures::future::Abortable::new(
                async move {
                    host.root("alice", move |context| {
                        async move { open(&context, &reference, 1000, 4096, CLEAN).await }
                            .boxed_local()
                    })
                    .await
                },
                registration,
            )
        };
        fixture
            .pool
            .spawner()
            .spawn_local(async move {
                let _ = caller.await;
            })
            .unwrap();
        fixture.pool.run_until_stalled();
        abort.abort();
        fixture.wait(|fixture| fixture.calls() == 0);
        assert_eq!(fixture.ping().unwrap(), 42);
        fixture.close();
    });
}

#[test]
#[ignore = "requires TEMBRA_WASM_STREAM_PRODUCER"]
fn the_receiving_root_ends_or_cancels_its_end() {
    engines(|bytes, interpreted| {
        let mut fixture = install(bytes, interpreted, 30_000);
        // Kept past its root, the end is closed and unusable.
        let mut kept = fixture
            .root(async move |context, reference, _| {
                open(&context, &reference, 1000, 4096, CLEAN).await
            })
            .unwrap();
        let read = fixture.pool.run_until(kept.next()).unwrap();
        assert_eq!(read.unwrap_err().code, core::Code::Expired, "{interpreted}");
        drop(kept);
        fixture.wait(|fixture| fixture.calls() == 0);
        // A read waiting for data wakes with the root's cancellation; the producer, parked in a
        // zero-length write, sees its reader gone and ends.
        let read = fixture
            .root(async move |context, reference, _| {
                let mut stream = open(&context, &reference, 0, 0, WAIT_FOR_READER).await?;
                let cancel = async {
                    futures::future::ready(()).await;
                    context.cancel()
                };
                let (read, cancelled) = futures::join!(stream.next(), cancel);
                cancelled?;
                Ok(read)
            })
            .unwrap();
        assert_eq!(
            read.unwrap().unwrap_err().code,
            core::Code::Cancelled,
            "{interpreted}"
        );
        fixture.wait(|fixture| fixture.calls() == 0);
        assert_eq!(fixture.ping().unwrap(), 42);
        fixture.close();
    });
}

#[test]
#[ignore = "requires TEMBRA_WASM_STREAM_PRODUCER"]
fn an_idle_producer_ends_only_at_the_terminal_timeout() {
    engines(|bytes, interpreted| {
        let mut fixture = install(bytes, interpreted, 1000);
        let started = Instant::now();
        let closed = fixture
            .root(async move |context, reference, _| {
                let stream = open(&context, &reference, 0, 0, IDLE).await?;
                // The producer never looks at its stream: the close is acknowledged only when the
                // call times out, and that is a failure.
                Ok(stream.close().await)
            })
            .unwrap();
        assert!(
            started.elapsed() >= Duration::from_millis(900),
            "{interpreted}"
        );
        assert!(
            closed.unwrap_err().message.contains("timed out"),
            "{interpreted}"
        );
        fixture.wait(|fixture| fixture.calls() == 0);
        // The Store is retired, never reused with its parked task.
        let ping = fixture.ping().unwrap_err();
        assert_eq!(ping.reason, "wasm_executor_quarantined", "{interpreted}");
        assert!(!fixture.host.errors().is_empty());
        fixture.close();
    });
}

#[test]
#[ignore = "requires TEMBRA_WASM_STREAM_PRODUCER"]
fn a_trap_is_never_a_clean_end() {
    engines(|bytes, interpreted| {
        for (mode, size) in [
            (TRAP_BEFORE_HEAD, 1024),
            (TRAP_HOLDING, 1024),
            (TRAP_AFTER_END, 1024),
            (TRAP_HOLDING, PIPE_BYTES as u32),
            (TRAP_AFTER_END, PIPE_BYTES as u32),
        ] {
            let mut fixture = install(bytes, interpreted, 30_000);
            let outcome = fixture.root(async move |context, reference, _| {
                let mut stream = open(&context, &reference, 4, size, mode).await?;
                let mut received = Vec::new();
                loop {
                    match stream.next().await {
                        Some(Ok(chunk)) => received.extend(chunk),
                        other => return Ok((received, other.map(|read| read.map(drop)))),
                    }
                }
            });
            let (failure, published) = match outcome {
                Err(error) => {
                    // A small body can finish and trap before the host observes its head.
                    // A body larger than the bounded pipe must instead publish a reader.
                    assert_eq!(
                        size, 1024,
                        "a backpressured producer failed before its head"
                    );
                    (error, false)
                }
                Ok((received, last)) => {
                    assert_ne!(mode, TRAP_BEFORE_HEAD, "a pre-head trap published a reader");
                    assert!(received.len() <= 4 * size as usize);
                    assert!(
                        received
                            .iter()
                            .enumerate()
                            .all(|(offset, byte)| *byte == expected(offset, size as usize)),
                        "{interpreted} {mode} {size}: corrupt prefix"
                    );
                    if size as usize == PIPE_BYTES {
                        assert!(!received.is_empty(), "the post-head case read no bytes");
                    }
                    // A trap may discard an unread tail; it must never expose a clean EOF,
                    // even when the writer closed before its task group trapped.
                    (
                        last.expect("a failure, not a clean end")
                            .expect_err("the producer trapped"),
                        true,
                    )
                }
            };
            assert!(
                failure.message.contains("trap"),
                "{interpreted} {mode}: {failure:?}"
            );
            // Join reporting as well as the invocation: this does not poll or drain errors.
            fixture.pool.run_until(fixture.host.settled());
            assert_eq!(fixture.calls(), 0);
            assert_eq!(fixture.engine.instances(), 0);
            let errors = fixture.host.errors();
            if published {
                // The caller already received the head; its physical failure is reported once.
                assert_eq!(errors, vec![failure], "{interpreted} {mode} {size}");
            } else {
                // The ordinary call result already delivered this failure: no duplicate report.
                assert!(errors.is_empty(), "{interpreted} {mode} {size}: {errors:?}");
            }
            assert_eq!(
                fixture.ping().unwrap_err().reason,
                "wasm_executor_quarantined"
            );
            fixture.close();
        }
    });
}

#[test]
#[ignore = "requires TEMBRA_WASM_STREAM_PRODUCER"]
fn host_close_and_reload_wait_for_the_producer() {
    engines(|bytes, interpreted| {
        // Close: the root still holds a half-read stream.
        let mut fixture = install(bytes, interpreted, 30_000);
        let read_once = Rc::new(Cell::new(false));
        {
            let (host, reference, read_once) = (
                fixture.host.clone(),
                fixture.reference.clone(),
                read_once.clone(),
            );
            fixture
                .pool
                .spawner()
                .spawn_local(async move {
                    let _ = host
                        .root("alice", move |context| {
                            async move {
                                let mut stream =
                                    open(&context, &reference, 1000, 4096, CLEAN).await?;
                                stream.next().await.unwrap()?;
                                read_once.set(true);
                                // Hold the end without reading until the host cancels the root.
                                context.signal().cancelled().await;
                                drop(stream);
                                Ok(())
                            }
                            .boxed_local()
                        })
                        .await;
                })
                .unwrap();
        }
        fixture.wait(|_| read_once.get());
        assert_eq!(fixture.calls(), 1);
        fixture.close();

        // Reload: a stream in progress keeps its old revision; new calls reach the new one.
        let mut fixture = install(bytes, interpreted, 30_000);
        let reloaded = Rc::new(RefCell::new(None));
        let received = fixture
            .root({
                let (engine, bytes, reloaded) =
                    (fixture.engine.clone(), bytes.to_vec(), reloaded.clone());
                async move |context, reference, host| {
                    let mut stream = open(&context, &reference, 64, 4096, CLEAN).await?;
                    let mut received = stream.next().await.unwrap()?.len();
                    let reload = host.reload(vec![orb(&engine, &bytes)], 30_000);
                    let read_rest = async {
                        while let Some(chunk) = stream.next().await {
                            received += chunk?.len();
                        }
                        Ok::<_, tembra::Error>(())
                    };
                    let (version, rest) = futures::join!(reload, read_rest);
                    *reloaded.borrow_mut() = Some(version.map(|_| ()));
                    rest.map(|()| received)
                }
            })
            .unwrap();
        assert_eq!(received, 64 * 4096, "{interpreted}");
        reloaded.borrow_mut().take().unwrap().unwrap();
        fixture.wait(|fixture| fixture.calls() == 0);
        assert_eq!(fixture.ping().unwrap(), 42);
        fixture.wait(|fixture| fixture.engine.instances() == 1);
        fixture.close();
    });
}
