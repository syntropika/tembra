//! A method with a codec: local callers move Rust values and never reach the codec, portable
//! callers' JSON goes through it, and its refusals fail the call as the arguments' or the
//! result's.
use futures::{FutureExt, executor::LocalPool};
use serde_json::{Value, json};
use std::{cell::Cell, rc::Rc};
use tembra::{Codec, Host, Orb, Service};

/// The arguments and the result of `add`, with no serde implementation.
struct Terms(u32, u32);
struct Sum(u32);

thread_local! {
    /// How many times this thread ran the codec.
    static CODED: Cell<u32> = const { Cell::new(0) };
}

/// Reads the arguments as `[left, right]` and writes the sum as `{"sum": n}`; it has no encoding
/// for a sum above 100.
const ADD: Codec<Terms, Sum> = Codec {
    decode: |value| {
        CODED.set(CODED.get() + 1);
        let (left, right) =
            serde_json::from_value(value).map_err(|_| "expected [left, right]".to_string())?;
        Ok(Terms(left, right))
    },
    encode: |Sum(sum)| {
        CODED.set(CODED.get() + 1);
        if sum > 100 {
            return Err(format!("{sum} is above 100"));
        }
        Ok(json!({ "sum": sum }))
    },
};

#[test]
fn local_calls_skip_the_codec_and_portable_calls_go_through_it() {
    let mut pool = LocalPool::new();
    let host = Host::new(pool.spawner());
    let service = Service::new("calculator", |_| Ok(())).method_with(
        "add",
        |_, _, Terms(left, right)| async move { Ok(Sum(left + right)) },
        ADD,
    );
    host.install(vec![Orb::new("calculator", 1).service(service)])
        .unwrap();
    let reference = Rc::new(host.get("calculator", vec!["main".into()], "host").unwrap());
    let called = reference.clone();
    pool.run_until(host.root("host", move |context| {
        async move {
            // The typed local call moves Rust values; the codec never runs.
            let Sum(sum) = called.call::<_, Sum>(&context, "add", Terms(1, 2)).await?;
            assert_eq!(sum, 3);
            assert_eq!(CODED.get(), 0);
            // A portable call's arguments and result go through the codec.
            let sum = called
                .call_portable::<_, Value>(&context, "add", json!([1, 2]))
                .await?;
            assert_eq!(sum, json!({ "sum": 3 }));
            assert_eq!(CODED.get(), 2);
            // Arguments the codec refuses, and a result it cannot write.
            let error = called
                .call_portable::<_, Value>(&context, "add", json!({ "left": 1 }))
                .await
                .unwrap_err();
            assert_eq!(error.reason, "arguments", "{error}");
            assert_eq!(error.message, "expected [left, right]");
            let error = called
                .call_portable::<_, Value>(&context, "add", json!([60, 70]))
                .await
                .unwrap_err();
            assert_eq!(error.reason, "result", "{error}");
            assert_eq!(error.message, "130 is above 100");
            Ok(())
        }
        .boxed_local()
    }))
    .unwrap();
    reference.release().unwrap();
    pool.run_until(host.close()).unwrap();
}
