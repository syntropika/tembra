//! A real TypeScript build, including WIT types and the SDK chunk helper, consumed by Rust.
use futures::{FutureExt, StreamExt, executor::LocalPool};
use serde::Deserialize;
use std::{path::PathBuf, rc::Rc};
use tembra::{Host, portable::ByteStream};
use tembra_executor_wasm::{Engine, Install, Limits};

#[test]
#[ignore = "requires TEMBRA_WASM_ENGINE and TEMBRA_WASM_FIXTURES"]
fn an_authored_typescript_stream_reaches_rust_without_truncation() {
    let interpreter = std::fs::read(
        std::env::var_os("TEMBRA_WASM_ENGINE").expect("TEMBRA_WASM_ENGINE is required"),
    )
    .unwrap();
    let fixtures = PathBuf::from(
        std::env::var_os("TEMBRA_WASM_FIXTURES").expect("TEMBRA_WASM_FIXTURES is required"),
    );
    let artifact = std::fs::read(fixtures.join("stream-source.wasm")).unwrap();
    for interpreted in [false, true] {
        let engine = if interpreted {
            Engine::interpreted(Limits::default())
        } else {
            Engine::new(Limits::default())
        }
        .unwrap()
        .interpreter(&interpreter)
        .unwrap();
        let mut pool = LocalPool::new();
        let host = Host::new(pool.spawner());
        host.install(vec![engine.load(&artifact, Install::default()).unwrap()])
            .unwrap();
        let reference = Rc::new(
            host.get(
                "typed-stream::tembra-test:typed-stream/frames",
                vec!["main".into()],
                "alice",
            )
            .unwrap(),
        );
        pool.run_until(reference.ready()).unwrap();
        let called = reference.clone();
        let count = pool
            .run_until(host.root("alice", move |context| {
                async move {
                    let mut stream: ByteStream = called
                        .call_portable(&context, "frames", (64_u32, 8192_u32))
                        .await?;
                    let mut offset = 0;
                    while let Some(chunk) = stream.next().await {
                        for byte in chunk? {
                            let expected = (((offset / 8192) * 17 + 23) & 255) as u8;
                            assert_eq!(
                                byte, expected,
                                "byte at {offset}, interpreted={interpreted}"
                            );
                            offset += 1;
                        }
                    }
                    stream.close().await?;
                    Ok(offset)
                }
                .boxed_local()
            }))
            .unwrap();
        assert_eq!(count, 512 << 10);
        reference.release().unwrap();
        pool.run_until(host.close()).unwrap();
        let diagnostics = host.kernel().diagnostics();
        assert_eq!(diagnostics.calls, 0);
        assert_eq!(diagnostics.roots, 0);
        assert_eq!(diagnostics.references, 0);
        assert_eq!(engine.instances(), 0);
        assert_eq!(engine.resources(), 0);
        assert!(host.errors().is_empty());
    }
}

#[derive(Debug, Deserialize, PartialEq)]
struct Summary {
    bytes: u64,
    checksum: u64,
}

#[test]
#[ignore = "requires TEMBRA_WASM_ENGINE and TEMBRA_WASM_FIXTURES"]
fn authored_typescript_consumes_and_forwards_another_orbs_stream() {
    let interpreter = std::fs::read(
        std::env::var_os("TEMBRA_WASM_ENGINE").expect("TEMBRA_WASM_ENGINE is required"),
    )
    .unwrap();
    let fixtures = PathBuf::from(
        std::env::var_os("TEMBRA_WASM_FIXTURES").expect("TEMBRA_WASM_FIXTURES is required"),
    );
    let producer = std::fs::read(fixtures.join("stream-source.wasm")).unwrap();
    let consumer = std::fs::read(fixtures.join("stream-source-consumer.wasm")).unwrap();
    for interpreted in [false, true] {
        // The checksum deliberately runs a JavaScript calculation over every received byte.
        let limits = Limits {
            fuel: 1_000_000_000,
            timeout_ms: 30_000,
            ..Limits::default()
        };
        let engine = if interpreted {
            Engine::interpreted(limits)
        } else {
            Engine::new(limits)
        }
        .unwrap()
        .interpreter(&interpreter)
        .unwrap();
        let mut pool = LocalPool::new();
        let host = Host::new(pool.spawner());
        host.install(vec![
            engine.load(&producer, Install::default()).unwrap(),
            engine
                .load(
                    &consumer,
                    Install::default().requires(
                        "tembra-test:typed-stream/frames",
                        "typed-stream",
                        1,
                    ),
                )
                .unwrap(),
        ])
        .unwrap();
        let reference = Rc::new(
            host.get(
                "typed-stream-consumer::tembra-test:typed-stream/relay",
                vec!["main".into()],
                "alice",
            )
            .unwrap(),
        );
        pool.run_until(reference.ready()).unwrap();
        for (method, count, size) in [("summarize", 32_u32, 4096_u32), ("bytewise", 4, 31)] {
            let called = reference.clone();
            let summary: Summary = pool
                .run_until(host.root("alice", move |context| {
                    async move { called.call_portable(&context, method, (count, size)).await }
                        .boxed_local()
                }))
                .unwrap();
            assert_eq!(
                summary,
                Summary {
                    bytes: u64::from(count) * u64::from(size),
                    checksum: (0..count)
                        .map(|index| u64::from((index * 17 + 23) & 255) * u64::from(size))
                        .sum(),
                },
                "{method}, interpreted={interpreted}"
            );
        }
        let called = reference.clone();
        let count = pool
            .run_until(host.root("alice", move |context| {
                async move {
                    let mut stream: ByteStream = called
                        .call_portable(&context, "forward", (64_u32, 8192_u32))
                        .await?;
                    let mut offset = 0;
                    while let Some(chunk) = stream.next().await {
                        for byte in chunk? {
                            assert_eq!(byte, (((offset / 8192) * 17 + 23) & 255) as u8);
                            offset += 1;
                        }
                    }
                    stream.close().await?;
                    Ok(offset)
                }
                .boxed_local()
            }))
            .unwrap();
        assert_eq!(count, 512 << 10);
        reference.release().unwrap();
        pool.run_until(host.close()).unwrap();
        let diagnostics = host.kernel().diagnostics();
        assert_eq!(diagnostics.calls, 0);
        assert_eq!(diagnostics.roots, 0);
        assert_eq!(diagnostics.references, 0);
        assert_eq!(engine.instances(), 0);
        assert_eq!(engine.resources(), 0);
        assert!(host.errors().is_empty());
    }
}
