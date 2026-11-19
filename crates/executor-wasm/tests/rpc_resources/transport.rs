//! Bounded application-owned byte transport for the actual Rust-host SDK integration tests.
use super::*;
use std::sync::mpsc as output_queue;

/// Run the public RPC adapter while blocking standard I/O stays on separate test threads.
pub(super) fn run(
    pool: &mut LocalPool,
    host: Host,
    options: rpc::Options,
    stopped: oneshot::Receiver<()>,
    release_gates: impl FnOnce(),
) -> Result<()> {
    let (output, received) = output_queue::sync_channel::<Vec<u8>>(32);
    let writer = std::thread::spawn(move || -> io::Result<()> {
        let mut stdout = io::stdout().lock();
        for bytes in received {
            stdout.write_all(&bytes)?;
            stdout.write_all(b"\n")?;
            stdout.flush()?;
        }
        Ok(())
    });
    let connection = rpc::serve(host.clone(), options, move |bytes| {
        output
            .try_send(bytes)
            .map_err(|_| tembra::Error::invalid("stdio", "test output queue full or closed"))
    });
    let (mut send, mut receive) = mpsc::channel(8);
    let reader = std::thread::spawn(move || {
        use std::io::Read;
        let mut input = io::stdin().lock();
        loop {
            let mut frame = Vec::new();
            match (&mut input).take(65_538).read_until(b'\n', &mut frame) {
                Ok(0) => break,
                Ok(_) => {
                    if frame.last() == Some(&b'\n') {
                        frame.pop();
                    }
                    if block_on(send.send(frame)).is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    });
    let result = pool.run_until(async {
        let incoming = async {
            while let Some(frame) = receive.next().await {
                connection.receive(&frame)?;
            }
            Ok::<_, tembra::Error>(true)
        }
        .fuse();
        let stopped = stopped.fuse();
        futures::pin_mut!(incoming, stopped);
        futures::select! { result = incoming => result, _ = stopped => Ok(false) }
    });
    connection.close();
    release_gates();
    // Cancellation cannot free a physically running provider; its gate must settle too.
    pool.run_until(host.close())?;
    let diagnostics = host.kernel().diagnostics();
    eprintln!(
        "{}",
        json!({"references": diagnostics.references, "calls": diagnostics.calls, "roots": diagnostics.roots})
    );
    assert_eq!(diagnostics.references, 0);
    assert_eq!(diagnostics.calls, 0);
    drop(connection);
    if matches!(result, Ok(true)) {
        reader.join().expect("input reader");
    }
    writer
        .join()
        .expect("output writer")
        .map_err(|error| tembra::Error::invalid("stdio", error.to_string()))?;
    result.map(|_| ())
}
