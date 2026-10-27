//! Expose native Rust methods over application-owned, newline-framed JSON bytes.

use futures::{
    FutureExt, SinkExt, StreamExt,
    channel::{mpsc, oneshot},
    executor::{LocalPool, block_on},
};
use serde::Serialize;
use serde_json::Value;
use std::{
    cell::Cell,
    io::{self, BufRead, Read, Write},
    rc::Rc,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
        mpsc as output_queue,
    },
};
use tembra::{Error, Host, Orb, Result, Service, rpc};

const COUNTER: &str = "example.counter";
const PRINCIPAL: &str = "trusted-client";
const MAX_FRAME: usize = 65_536;
const QUEUE_CAPACITY: usize = 8;
const MAX_OUTPUT_BYTES: usize = 4 * MAX_FRAME;
const METHODS: [&str; 7] = [
    "set",
    "read",
    "echo",
    "principal",
    "wait",
    "cancelled",
    "oversize",
];

#[derive(Clone, Serialize)]
struct Snapshot {
    value: i64,
    started: bool,
    #[serde(skip_serializing_if = "String::is_empty")]
    blob: String,
}

/// Declare the methods and state served by this example's connection.
fn counter() -> Orb {
    let cancellations = Rc::new(Cell::new(0_u32));
    let waiting = cancellations.clone();
    let service = Service::new(COUNTER, |declaration| {
        declaration.state(
            Snapshot {
                value: 0,
                started: false,
                blob: String::new(),
            },
            1,
            |_| Ok(()),
        )
    })
    .method("set", |state, context, (value,): (i64,)| async move {
        Ok(state
            .update(&context, |previous| {
                Ok(Snapshot {
                    value,
                    ..previous.clone()
                })
            })?
            .value)
    })
    .method(
        "read",
        |state, _, _: ()| async move { Ok(state.value()?.value) },
    )
    .method("echo", |_, _, (value,): (Value,)| async move { Ok(value) })
    .method("principal", |_, context, _: ()| async move {
        context.principal()
    })
    .method("wait", move |state, context, _: ()| {
        let cancellations = waiting.clone();
        async move {
            let signal = context.signal();
            state.update(&context, |previous| {
                Ok(Snapshot {
                    started: true,
                    ..previous.clone()
                })
            })?;
            signal.cancelled().await;
            cancellations.set(cancellations.get() + 1);
            // Cancellation does not permit another state write with the expired context.
            signal.check()?;
            Ok(())
        }
    })
    .method("cancelled", move |_, _, _: ()| {
        let cancellations = cancellations.clone();
        async move { Ok(cancellations.get()) }
    })
    .method("oversize", |state, context, _: ()| async move {
        state.update(&context, |previous| {
            Ok(Snapshot {
                blob: "x".repeat(MAX_FRAME),
                ..previous.clone()
            })
        })?;
        Ok(())
    })
    .method("hidden", |_, _, _: ()| async move { Ok("private") });
    #[cfg(feature = "views")]
    let service = service.access(|principal| {
        if principal == PRINCIPAL {
            METHODS.into_iter().map(str::to_owned).collect()
        } else {
            Default::default()
        }
    });
    Orb::new("rpc-example", 1).service(service)
}

/// Report transport failures through the same typed error boundary as native RPC.
fn io_error(error: io::Error) -> Error {
    Error::invalid("stdio", error.to_string())
}

/// Refuse output instead of blocking the host when the transport cannot keep up.
fn output_full() -> Error {
    Error::new(
        tembra::Code::Exhausted,
        "output_queue",
        "stdio output budget is full",
    )
}

/// Feed complete application frames into RPC while the local executor runs native calls.
fn main() -> Result<()> {
    let mut executor = LocalPool::new();
    let host = Host::new(executor.spawner());
    host.install(vec![counter()])?;
    let mut options = rpc::Options::new(PRINCIPAL.to_owned(), |service, key| {
        service == COUNTER && key.is_none_or(|key| key.first().is_some_and(|part| part != "denied"))
    });
    options.max_frame = MAX_FRAME;
    let (output, output_frames) = output_queue::sync_channel::<Vec<u8>>(QUEUE_CAPACITY);
    let retained = Arc::new(AtomicUsize::new(0));
    let writing = retained.clone();
    let (failure, failed) = oneshot::channel();
    let writer = std::thread::spawn(move || {
        let mut stdout = io::stdout().lock();
        while let Ok(frame) = output_frames.recv() {
            let result = stdout
                .write_all(&frame)
                .and_then(|()| stdout.write_all(b"\n"))
                .and_then(|()| stdout.flush());
            writing.fetch_sub(frame.len() + 1, Ordering::AcqRel);
            if let Err(error) = result {
                let error = io_error(error);
                let _ = failure.send(error.clone());
                return Err(error);
            }
        }
        Ok(())
    });
    let connection = rpc::serve(host.clone(), options, move |frame| {
        let bytes = frame.len() + 1;
        retained
            .try_update(Ordering::AcqRel, Ordering::Acquire, |current| {
                current
                    .checked_add(bytes)
                    .filter(|next| *next <= MAX_OUTPUT_BYTES)
            })
            .map_err(|_| output_full())?;
        match output.try_send(frame) {
            Ok(()) => Ok(()),
            Err(error) => {
                retained.fetch_sub(bytes, Ordering::AcqRel);
                match error {
                    output_queue::TrySendError::Full(_) => Err(output_full()),
                    output_queue::TrySendError::Disconnected(_) => {
                        Err(Error::invalid("stdio", "output writer stopped"))
                    }
                }
            }
        }
    });
    let (mut send, mut receive) = mpsc::channel(QUEUE_CAPACITY);
    let reader = std::thread::spawn(move || {
        let mut input = io::stdin().lock();
        loop {
            let mut frame = Vec::new();
            // Limit allocation before a peer can complete an oversized frame.
            let read = (&mut input)
                .take((MAX_FRAME + 2) as u64)
                .read_until(b'\n', &mut frame);
            match read {
                Ok(0) => break,
                Ok(_) => {
                    if frame.last() == Some(&b'\n') {
                        frame.pop();
                    }
                    if frame.len() > MAX_FRAME {
                        let error = Error::new(
                            tembra::Code::Exhausted,
                            "frame_size",
                            "stdio frame exceeds the connection limit",
                        );
                        let _ = block_on(send.send(Err(error)));
                        break;
                    }
                    // Backpressure blocks this reader instead of retaining unlimited frames.
                    if block_on(send.send(Ok(frame))).is_err() {
                        break;
                    }
                }
                Err(error) => {
                    let _ = block_on(send.send(Err(io_error(error))));
                    break;
                }
            }
        }
    });
    let input = executor.run_until(async {
        let incoming = async {
            while let Some(frame) = receive.next().await {
                match frame {
                    Ok(frame) => connection.receive(&frame)?,
                    Err(error) => {
                        connection.close_with(error.clone());
                        return Err(error);
                    }
                }
            }
            Ok::<_, Error>(())
        }
        .fuse();
        let closed = connection.closed().fuse();
        let failed = failed.fuse();
        futures::pin_mut!(incoming, closed, failed);
        futures::select! {
            result = incoming => result.map(|()| true),
            () = closed => Ok(false),
            failure = failed => {
                let error = failure.unwrap_or_else(|_| Error::invalid("stdio", "output writer stopped"));
                connection.close_with(error.clone());
                Err(error)
            },
        }
    });
    connection.close();
    executor.run_until(connection.closed());
    let closed = executor.run_until(host.close());
    if matches!(input, Ok(true)) {
        reader.join().expect("the input reader must finish on EOF");
    }
    executor.run_until_stalled();
    let diagnostics = host.kernel().diagnostics();
    assert_eq!(diagnostics.references, 0);
    assert_eq!(diagnostics.calls, 0);
    assert_eq!(diagnostics.roots, 0);
    let cause = connection.close_cause();
    // End the output queue only after host work has settled and released its connection handles.
    drop(connection);
    if let Some(cause) = cause {
        return Err(cause);
    }
    closed?;
    if input? {
        // Clean input EOF drains accepted output; terminal failures never wait on a blocked writer.
        writer
            .join()
            .map_err(|_| Error::invalid("stdio", "output writer panicked"))??;
    }
    Ok(())
}
