//! Declared breaking semantics of prepared source Orbs: failures without a WIT error lane trap
//! and quarantine the revision; WIT `result` errors and refused arguments stay recoverable.
#[path = "support/source.rs"]
mod source;
use futures::{FutureExt, StreamExt};
use serde_json::{Value, json};
use source::{Installed, engines, fixture};
use std::rc::Rc;
use tembra::Reference;
use tembra::portable::ByteStream;
use tembra_executor_wasm::Install;

const FRAMES: &str = "failing::tembra-test:failing/frames@1.0.0";

/// A refused argument before the host call clears the selection: the next dependency call of the
/// same callback succeeds.
#[test]
#[ignore = "requires TEMBRA_WASM_FIXTURES"]
fn a_refused_dependency_argument_leaves_the_next_call_usable() {
    for engine in engines() {
        let mut installed = Installed::new(
            &engine,
            vec![
                (fixture("selection-provider"), Install::default()),
                (
                    fixture("selection"),
                    Install::default().requires("tembra-test:selection/math@1.0.0", "doubler", 1),
                ),
            ],
        );
        let probe = installed.reference("selection::tembra-test:selection/probe@1.0.0", "alice");
        assert_eq!(
            installed
                .call(&probe, "alice", "recover", json!([]))
                .unwrap(),
            json!("RangeError 42")
        );
        probe.release().unwrap();
        assert!(installed.host.errors().is_empty());
        installed.close(&engine);
    }
}

/// Read a whole stream result; the outcome is the bytes or the read failure.
fn read(
    installed: &mut Installed,
    frames: &Rc<Reference>,
    mode: &str,
) -> (Vec<u8>, Result<(), String>) {
    let mode = mode.to_owned();
    let called = frames.clone();
    let result = installed
        .pool
        .run_until(installed.host.root("alice", move |context| {
            async move {
                let mut stream: ByteStream = called
                    .call_portable(&context, "frames", (4_u32, mode))
                    .await?;
                let mut bytes = Vec::new();
                while let Some(chunk) = stream.next().await {
                    match chunk {
                        Ok(chunk) => bytes.extend_from_slice(&chunk),
                        Err(error) => return Ok((bytes, Err(error.message.clone()))),
                    }
                }
                Ok((bytes, Ok(())))
            }
            .boxed_local()
        }));
    result.unwrap_or_else(|error| (Vec::new(), Err(error.message)))
}

/// A producer that completes ends cleanly with exact bytes; one that throws or rejects after its
/// head never ends cleanly and quarantines its revision.
#[test]
#[ignore = "requires TEMBRA_WASM_FIXTURES"]
fn stream_producer_failures_are_never_a_clean_end() {
    for interpreted in [false, true] {
        for mode in ["none", "throw", "reject"] {
            let engine = source::engine(interpreted);
            let mut installed =
                Installed::new(&engine, vec![(fixture("failing"), Install::default())]);
            let frames = installed.reference(FRAMES, "alice");
            let (bytes, end) = read(&mut installed, &frames, mode);
            let after = installed.call(&frames, "alice", "refuse", json!([]));
            if mode == "none" {
                assert_eq!(end, Ok(()));
                assert_eq!(bytes.len(), 4 * 4096);
                assert!(
                    bytes
                        .chunks(4096)
                        .enumerate()
                        .all(|(index, chunk)| chunk.iter().all(|byte| usize::from(*byte) == index))
                );
                assert!(after.is_ok(), "{after:?}");
            } else {
                assert!(end.is_err(), "{mode}: a failed producer ended cleanly");
                assert!(bytes.len() <= 4 * 4096, "{mode}");
                assert!(
                    after.is_err(),
                    "{mode}: the revision kept serving after a producer trap"
                );
            }
            let _ = frames.release();
            if mode == "none" {
                installed.close(&engine);
            } else {
                installed.retire(&engine);
            }
        }
    }
}

/// A WIT `result` error is a value and keeps the revision; an uncaught exception from a function
/// without a result type traps and quarantines it.
#[test]
#[ignore = "requires TEMBRA_WASM_FIXTURES"]
fn result_errors_recover_and_uncaught_exceptions_quarantine() {
    for engine in engines() {
        let mut installed = Installed::new(&engine, vec![(fixture("failing"), Install::default())]);
        let frames = installed.reference(FRAMES, "alice");
        let refused: Value = installed
            .call(&frames, "alice", "refuse", json!([]))
            .unwrap();
        assert_eq!(refused, json!({"tag": "err", "val": "business refusal"}));
        let again: Value = installed
            .call(&frames, "alice", "refuse", json!([]))
            .unwrap();
        assert_eq!(again, refused);
        let exploded = installed.call(&frames, "alice", "explode", json!([]));
        assert!(exploded.is_err(), "{exploded:?}");
        let after = installed.call(&frames, "alice", "refuse", json!([]));
        assert!(
            after.is_err(),
            "the revision kept serving after an uncaught exception"
        );
        let _ = frames.release();
        installed.retire(&engine);
    }
}
