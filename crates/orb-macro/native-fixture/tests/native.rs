//! `calculator::orb()` in a native host: the declaration the kernel sees, local calls with Rust
//! values, through the generated clients too, portable calls with the boundary's JSON through the
//! generated codecs, a cancelled context, and the services of the sub-modules.
use futures::{FutureExt, executor::LocalPool, future::LocalBoxFuture};
use serde_json::{Value, json};
use std::rc::Rc;
use tembra::{
    Context, Host, Reference,
    contracts::Contract,
    core::{Code, Lifetime},
};
use tembra_orb_native_fixture::calculator;

const MATH: &str = "calculator::example:calculator/math@1.0.0";
const TALLY: &str = "calculator::example:calculator/tally@1.0.0";
const MEMO: &str = "calculator::example:calculator/memo@1.0.0";

/// Install the Orb in a host and run `test` under a root of the principal `host`, with a
/// reference to the `main` object of `service`.
fn with_main(
    service: &str,
    test: impl FnOnce(Rc<Reference>, Context) -> LocalBoxFuture<'static, tembra::Result<()>>,
) {
    let mut pool = LocalPool::new();
    let host = Host::new(pool.spawner());
    host.install(vec![calculator::orb().unwrap()]).unwrap();
    let reference = Rc::new(host.get(service, vec!["main".into()], "host").unwrap());
    let called = reference.clone();
    pool.run_until(host.root("host", move |context| test(called, context)))
        .unwrap();
    reference.release().unwrap();
    pool.run_until(host.close()).unwrap();
}

/// `orb()` declares what the kernel computes for a component of the same contract, from the
/// module's `WIT`: the Orb named by the manifest, `calculator` at API version 2, not the crate
/// name at 1; each interface as a service, with its signature and its methods; and the ownership
/// policy of each `#[service]` attribute, which the WIT does not carry and a component's manifest
/// adds, so the expectation sets it the same way.
#[test]
fn the_declaration_is_the_plan_of_the_contract() {
    let orb = calculator::orb().unwrap();
    let mut expected = Contract::from_source("calculator.wit", calculator::WIT, Some("calculator"))
        .unwrap()
        .plan("calculator", 2, &Default::default())
        .unwrap()
        .orb;
    for service in &mut expected.services {
        match service.name.as_str() {
            TALLY => service.lifetime = Lifetime::Host,
            MEMO => service.linger_ms = 2500,
            _ => {}
        }
    }
    assert_eq!(orb.declaration(), &expected);
    let names: Vec<&str> = expected
        .services
        .iter()
        .map(|service| service.name.as_str())
        .collect();
    assert_eq!(names, [MATH, TALLY, MEMO]);
    assert!(
        expected
            .services
            .iter()
            .all(|service| service.signature.is_some())
    );
    assert_eq!(
        expected.services[0].methods,
        [
            "add", "both", "initial", "label", "largest", "negate", "new", "ping", "scale", "swap",
            "total"
        ]
        .map(String::from)
        .into()
    );
}

/// A local caller passes the arguments as a tuple in WIT order, `()` without arguments, and gets
/// the result as the Rust value; the codec never runs. The types are exact: other ones are refused
/// before the method runs.
#[test]
fn local_calls_move_rust_values() {
    with_main(MATH, |math, context| {
        async move {
            assert_eq!(math.call::<_, u32>(&context, "add", (1u32, 2u32)).await?, 3);
            assert_eq!(math.call::<_, i64>(&context, "negate", (5i64,)).await?, -5);
            assert_eq!(
                math.call::<_, Option<u64>>(&context, "largest", (vec![3u64, 9, 4],))
                    .await?,
                Some(9)
            );
            assert_eq!(
                math.call::<_, Vec<f64>>(&context, "scale", (vec![1.5, 2.0], 2.0))
                    .await?,
                [3.0, 4.0]
            );
            assert_eq!(
                math.call::<_, Option<char>>(&context, "initial", ("héllo".to_string(),))
                    .await?,
                Some('h')
            );
            assert_eq!(
                math.call::<_, (u32, String)>(&context, "swap", (("a".to_string(), 1u32),))
                    .await?,
                (1, "a".to_string())
            );
            assert!(
                !math
                    .call::<_, bool>(&context, "both", (true, false))
                    .await?
            );
            assert_eq!(
                math.call::<_, u64>(&context, "total", (vec![1u32, 2, 3],))
                    .await?,
                6
            );
            math.call::<_, ()>(&context, "ping", ()).await?;
            let error = math
                .call::<_, u32>(&context, "add", (1u64, 2u64))
                .await
                .unwrap_err();
            assert_eq!(error.reason, "rust_type", "{error}");
            Ok(())
        }
        .boxed_local()
    });
}

/// The typed client of a service calls each of its methods with the parameters the function takes,
/// after the call's context, and returns what it returns: the compiler checks both, and the call
/// takes the local entrypoint, with the arguments as a tuple. The function `new` is the client's
/// `new_`, and a parameter named `context` is the function's, beside the call's context. A service
/// of a sub-module has its client in its module.
#[test]
fn the_client_calls_each_method_with_its_types() {
    with_main(MATH, |math, context| {
        async move {
            let math = calculator::MathClient::new(math);
            assert_eq!(math.add(&context, 1, 2).await?, 3);
            assert_eq!(math.negate(&context, 5).await?, -5);
            assert_eq!(math.largest(&context, vec![3, 9, 4]).await?, Some(9));
            assert_eq!(math.scale(&context, vec![1.5, 2.0], 2.0).await?, [3.0, 4.0]);
            assert_eq!(math.initial(&context, "héllo".into()).await?, Some('h'));
            assert_eq!(math.swap(&context, ("a".into(), 1)).await?, (1, "a".into()));
            assert!(!math.both(&context, true, false).await?);
            assert_eq!(math.total(&context, vec![1, 2, 3]).await?, 6);
            math.ping(&context).await?;
            assert_eq!(math.new_(&context).await?, 0);
            assert_eq!(math.label(&context, 42, "total".into()).await?, "total: 42");
            Ok(())
        }
        .boxed_local()
    });
    // Recording nothing leaves the total, which another test of the process may be adding to.
    with_main(TALLY, |tally, context| {
        async move {
            let tally = calculator::tally::TallyClient::new(tally);
            let _total: u32 = tally.record(&context, 0).await?;
            Ok(())
        }
        .boxed_local()
    });
}

/// A portable caller's arguments are a JSON array in WIT order, and the result comes back as the
/// boundary's JSON: 64-bit integers as exact numbers, a `char` as a string of one character, an
/// option as `null` or the value, a tuple as an array, and `null` for no result. A method without
/// parameters takes `[]` and `null`. What the codec refuses fails the call as its arguments.
#[test]
fn portable_calls_go_through_the_codec() {
    with_main(MATH, |math, context| {
        async move {
            let call = |method: &'static str, arguments: Value| {
                let math = math.clone();
                let context = context.clone();
                async move {
                    math.call_portable::<_, Value>(&context, method, arguments)
                        .await
                }
            };
            assert_eq!(call("add", json!([1, 2])).await?, json!(3));
            assert_eq!(call("negate", json!([-5])).await?, json!(5));
            assert_eq!(
                call("negate", json!([9_007_199_254_740_993_i64])).await?,
                json!(-9_007_199_254_740_993_i64)
            );
            assert_eq!(call("largest", json!([[3, 9, 4]])).await?, json!(9));
            assert_eq!(call("largest", json!([[]])).await?, Value::Null);
            assert_eq!(
                call("scale", json!([[1.5, 2.0], 2.0])).await?,
                json!([3.0, 4.0])
            );
            assert_eq!(call("initial", json!(["héllo"])).await?, json!("h"));
            assert_eq!(call("initial", json!([""])).await?, Value::Null);
            assert_eq!(call("swap", json!([["a", 1]])).await?, json!([1, "a"]));
            assert_eq!(call("both", json!([true, false])).await?, json!(false));
            assert_eq!(call("total", json!([[1, 2, 3]])).await?, json!(6));
            assert_eq!(call("ping", json!([])).await?, Value::Null);
            assert_eq!(call("ping", Value::Null).await?, Value::Null);
            for (method, arguments, message) in [
                ("add", json!({ "a": 1 }), "expected the arguments as a list"),
                ("add", json!([1]), "expected 2 arguments, got 1"),
                (
                    "add",
                    json!(["x", 2]),
                    "invalid type: string \"x\", expected u32",
                ),
                ("add", json!([1, -2]), "invalid number"),
                ("ping", json!([1]), "expected no arguments, got 1"),
                (
                    "negate",
                    json!(["5"]),
                    "invalid type: string \"5\", expected i64",
                ),
                ("largest", json!([7]), "expected a list"),
                (
                    "initial",
                    json!([["h"]]),
                    "invalid type: sequence, expected a string",
                ),
                (
                    "swap",
                    json!([["a"]]),
                    "expected a tuple of 2 elements, got 1",
                ),
                ("swap", json!(["a"]), "expected a tuple as a list"),
                (
                    "swap",
                    json!([[1, "a"]]),
                    "invalid type: number, expected a string",
                ),
            ] {
                let error = call(method, arguments).await.unwrap_err();
                assert_eq!(error.reason, "arguments", "{method}: {error}");
                assert_eq!(error.message, message, "{method}");
            }
            Ok(())
        }
        .boxed_local()
    });
}

/// A service of a sub-module is registered from the Orb module, with the lifetime of its
/// attribute: the `main` object of the host-lifetime `Tally` keeps its total across roots. A
/// method checks its context before it runs the author's function: under a cancelled context the
/// call fails and the total is left alone.
#[test]
fn a_sub_module_service_keeps_its_total_and_checks_the_context() {
    with_main(TALLY, |tally, context| {
        async move {
            assert_eq!(tally.call::<_, u32>(&context, "record", (5u32,)).await?, 5);
            assert_eq!(tally.call::<_, u32>(&context, "record", (2u32,)).await?, 7);
            context.cancel()?;
            let error = tally
                .call::<_, u32>(&context, "record", (10u32,))
                .await
                .unwrap_err();
            assert_eq!(error.code, Code::Cancelled, "{error}");
            Ok(())
        }
        .boxed_local()
    });
    with_main(TALLY, |tally, context| {
        async move {
            assert_eq!(tally.call::<_, u32>(&context, "record", (0u32,)).await?, 7);
            Ok(())
        }
        .boxed_local()
    });
}

/// A service of a private sub-module is registered too, since the registration sits in its module;
/// the lingering `Memo` answers like any other.
#[test]
fn a_private_sub_module_service_is_registered() {
    with_main(MEMO, |memo, context| {
        async move {
            assert_eq!(
                memo.call::<_, String>(&context, "echo", ("hi".to_string(),))
                    .await?,
                "hi"
            );
            assert_eq!(
                memo.call_portable::<_, Value>(&context, "echo", json!(["there"]))
                    .await?,
                json!("there")
            );
            Ok(())
        }
        .boxed_local()
    });
}
