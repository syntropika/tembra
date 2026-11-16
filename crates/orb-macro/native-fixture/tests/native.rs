//! `calculator::orb()` in a native host: the declaration the kernel sees, local calls with Rust
//! values, through the generated clients too, portable calls with the boundary's JSON through the
//! generated codecs, which write what the module's contract accepts, a cancelled context, and the
//! services of the sub-modules.
use futures::{FutureExt, executor::LocalPool, future::LocalBoxFuture};
use serde_json::{Value, json};
use std::rc::Rc;
use tembra::{
    Context, Host, Reference,
    contracts::Contract,
    core::{Code, Lifetime},
};
use tembra_orb_native_fixture::calculator::{self, Heading, Move, Note, Pen, Point, Shape, Stroke};

const MATH: &str = "calculator::example:calculator/math@1.0.0";
const PLOTTER: &str = "calculator::example:calculator/plotter@1.0.0";
const TALLY: &str = "calculator::example:calculator/tally@1.0.0";
const MEMO: &str = "calculator::example:calculator/memo@1.0.0";

/// The module's contract, read from its `WIT`, as a compiled or TypeScript caller validates what
/// it sends to the Orb and what it receives.
fn contract() -> Contract {
    Contract::from_source("calculator.wit", calculator::WIT, Some("calculator")).unwrap()
}

/// Whether the contract accepts `value` as the arguments, or as the result, of the method of an
/// interface.
fn accepted(
    contract: &Contract,
    interface: &str,
    method: &str,
    arguments: bool,
    value: &Value,
) -> bool {
    let slot = contract.bind(interface, method, true).unwrap();
    contract.validate_slot(slot, arguments, value).is_ok()
}

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
    assert_eq!(names, [MATH, PLOTTER, TALLY, MEMO]);
    assert!(
        expected
            .services
            .iter()
            .all(|service| service.signature.is_some())
    );
    assert_eq!(
        expected.services[0].methods,
        [
            "add", "both", "initial", "label", "largest", "negate", "new", "ping", "ratio",
            "scale", "swap", "total"
        ]
        .map(String::from)
        .into()
    );
    assert_eq!(
        expected.services[1].methods,
        ["area", "redrawn", "settled", "sorted", "step", "walk"]
            .map(String::from)
            .into()
    );
}

/// `orb_named` builds the Orb under the name and API version a host chooses, so one crate installs
/// twice: each Orb names its services after itself, and each answers. `orb()` is `orb_named` with
/// the manifest's name and version.
#[test]
fn one_crate_installs_twice_under_two_names() {
    assert_eq!(
        calculator::orb().unwrap().declaration(),
        calculator::orb_named("calculator", 2)
            .unwrap()
            .declaration()
    );
    let (left, right) = (
        calculator::orb_named("left", 1).unwrap(),
        calculator::orb_named("right", 3).unwrap(),
    );
    for (orb, name, version) in [(&left, "left", 1), (&right, "right", 3)] {
        let declaration = orb.declaration();
        assert_eq!(
            (declaration.name.as_str(), declaration.api_version),
            (name, version)
        );
        assert!(declaration.services.iter().all(|service| {
            service
                .name
                .starts_with(&format!("{name}::example:calculator/"))
        }));
    }
    let mut pool = LocalPool::new();
    let host = Host::new(pool.spawner());
    host.install(vec![left, right]).unwrap();
    let math = |orb: &str| {
        let service = format!("{orb}::example:calculator/math@1.0.0");
        Rc::new(host.get(&service, vec!["main".into()], "host").unwrap())
    };
    let (left, right) = (math("left"), math("right"));
    let called = (left.clone(), right.clone());
    pool.run_until(host.root("host", move |context| {
        async move {
            let (left, right) = called;
            assert_eq!(left.call::<_, u32>(&context, "add", (1u32, 2u32)).await?, 3);
            assert_eq!(
                right
                    .call_portable::<_, Value>(&context, "add", json!([3, 4]))
                    .await?,
                json!(7)
            );
            Ok(())
        }
        .boxed_local()
    }))
    .unwrap();
    left.release().unwrap();
    right.release().unwrap();
    pool.run_until(host.close()).unwrap();
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

/// Numbers cross as the contract has them: a float argument that is not finite, is `-0` or
/// narrows to neither in an `f32` fails the call as its arguments, and a float result that is not
/// finite or is `-0` fails it as its result, where serde would write `null` or `-0`. An `f32`
/// is written as the `f64` it widens to, as a component writes it. A signed integer is no `-0`
/// either. What the codec refuses as arguments, the contract refuses too.
#[test]
fn numbers_cross_as_the_contract_has_them() {
    with_main(MATH, |math, context| {
        async move {
            let contract = contract();
            let interface = "example:calculator/math@1.0.0";
            let call = |method: &'static str, arguments: Value| {
                let math = math.clone();
                let context = context.clone();
                async move {
                    math.call_portable::<_, Value>(&context, method, arguments)
                        .await
                }
            };
            for (method, arguments, result) in [
                ("ratio", json!([1.5, 0.5]), json!(3.0)),
                ("ratio", json!([1.0, 3.0]), json!(f64::from(1.0f32 / 3.0))),
                ("ratio", json!([1e-50, 1.0]), json!(0.0)),
                ("scale", json!([[1.5, -2.0], 2.0]), json!([3.0, -4.0])),
            ] {
                assert!(accepted(&contract, interface, method, true, &arguments));
                let answer = call(method, arguments).await?;
                assert_eq!(answer, result, "{method}");
                assert!(accepted(&contract, interface, method, false, &answer));
            }
            let negative_zero: Value = serde_json::from_str("[-0]").unwrap();
            for (method, arguments, message) in [
                (
                    "ratio",
                    json!([1e39, 1.0]),
                    "expected a finite f32 other than -0, got 1e+39",
                ),
                (
                    "ratio",
                    json!([-0.0, 1.0]),
                    "expected a finite f32 other than -0, got -0.0",
                ),
                (
                    "ratio",
                    json!([-1e-50, 1.0]),
                    "expected a finite f32 other than -0, got -1e-50",
                ),
                (
                    "scale",
                    json!([[-0.0], 1.0]),
                    "expected a finite f64 other than -0, got -0.0",
                ),
                (
                    "ratio",
                    json!(["1", 1.0]),
                    "invalid type: string \"1\", expected f32",
                ),
                ("negate", negative_zero, "expected an integer, got -0"),
            ] {
                assert!(!accepted(&contract, interface, method, true, &arguments));
                let error = call(method, arguments).await.unwrap_err();
                assert_eq!(error.reason, "arguments", "{method}: {error}");
                assert_eq!(error.message, message, "{method}");
            }
            for (method, arguments, message) in [
                ("ratio", json!([0.0, 0.0]), "cannot write NaN"),
                ("ratio", json!([1.0, 0.0]), "cannot write inf"),
                ("ratio", json!([-1e-30, 1e30]), "cannot write -0"),
                ("scale", json!([[1e308], 10.0]), "cannot write inf"),
                ("scale", json!([[-1.0], 0.0]), "cannot write -0"),
            ] {
                let error = call(method, arguments).await.unwrap_err();
                assert_eq!(error.reason, "result", "{method}: {error}");
                assert_eq!(
                    error.message,
                    format!("{message}: the boundary carries finite numbers other than -0"),
                    "{method}"
                );
            }
            Ok(())
        }
        .boxed_local()
    });
}

/// A local caller passes and gets the module's own types: the record and the enum the bindings
/// generate, the author's enum with a case with named fields and the record holding it, which the
/// macro keeps, and the author's flags. A `Result` is a value, and so is an empty side of one.
#[test]
fn local_calls_move_the_module_types() {
    with_main(PLOTTER, |plotter, context| {
        async move {
            let from = Point {
                x: 1,
                y: i64::MAX - 1,
            };
            assert_eq!(
                plotter
                    .call::<_, Point>(&context, "step", (from, Some(Heading::West), 3u64))
                    .await?,
                Point {
                    x: -2,
                    y: i64::MAX - 1
                }
            );
            assert_eq!(
                plotter
                    .call::<_, Result<u64, String>>(
                        &context,
                        "area",
                        (Shape::Rect {
                            width: 2,
                            height: 3,
                        },)
                    )
                    .await?,
                Ok(6)
            );
            assert_eq!(
                plotter
                    .call::<_, Result<u64, String>>(&context, "area", (Shape::Dot,))
                    .await?,
                Err("a dot has no area".to_string())
            );
            assert_eq!(
                plotter
                    .call::<_, u64>(&context, "settled", (Err::<u64, _>(String::new()), 7u64))
                    .await?,
                7
            );
            let stroke = Stroke {
                shape: Shape::Square(2),
                pen: Pen {
                    filled: true,
                    dashed_line: false,
                },
            };
            assert_eq!(
                plotter
                    .call::<_, Stroke>(&context, "redrawn", (stroke, 3u32))
                    .await?,
                Stroke {
                    shape: Shape::Square(6),
                    pen: Pen {
                        filled: false,
                        dashed_line: true,
                    },
                }
            );
            let points = vec![Point { x: 0, y: 2 }, Point { x: 1, y: 1 }];
            assert_eq!(
                plotter
                    .call::<_, (Vec<Option<Heading>>, Result<(), u32>)>(
                        &context,
                        "sorted",
                        (points,)
                    )
                    .await?,
                (vec![Some(Heading::North), None], Err(1))
            );
            let path = vec![Move::Step(2), Move::Turn(Heading::East), Move::Step(3)];
            assert_eq!(
                plotter
                    .call::<_, Point>(&context, "walk", (Point { x: 0, y: 0 }, path))
                    .await?,
                Point { x: 3, y: 2 }
            );
            // The typed client takes and returns the same types.
            let client = calculator::PlotterClient::new(plotter);
            assert_eq!(
                client
                    .area(
                        &context,
                        Shape::Rect {
                            width: 2,
                            height: 2,
                        }
                    )
                    .await?,
                Ok(4)
            );
            let stroke = Stroke {
                shape: Shape::Dot,
                pen: Pen {
                    filled: false,
                    dashed_line: false,
                },
            };
            assert_eq!(
                client.redrawn(&context, stroke, 2).await?.pen,
                Pen {
                    filled: true,
                    dashed_line: true,
                }
            );
            assert_eq!(
                client
                    .walk(&context, Point { x: 1, y: 1 }, vec![Move::Stay])
                    .await?,
                Point { x: 1, y: 1 }
            );
            Ok(())
        }
        .boxed_local()
    });
}

/// A portable caller sends and receives the boundary's JSON, which the module's contract accepts
/// both ways: a record as an object of lowerCamelCase fields, 64-bit integers as exact numbers, an
/// enum as the WIT name of its case, a variant and a `Result` as `{tag, val}`, without `val` for
/// a case or a side that carries nothing, the record of a case with named fields as an object, an
/// option as `null` or the value, and a flags as an object of the flags set, which reads a flag
/// left out, or `false`, as unset.
#[test]
fn portable_calls_write_what_the_contract_accepts() {
    with_main(PLOTTER, |plotter, context| {
        async move {
            let contract = contract();
            let interface = "example:calculator/plotter@1.0.0";
            let call = |method: &'static str, arguments: Value| {
                let plotter = plotter.clone();
                let context = context.clone();
                let contract = &contract;
                async move {
                    assert!(
                        accepted(contract, interface, method, true, &arguments),
                        "{method}: {arguments}"
                    );
                    let answer = plotter
                        .call_portable::<_, Value>(&context, method, arguments)
                        .await?;
                    assert!(
                        accepted(contract, interface, method, false, &answer),
                        "{method}: {answer}"
                    );
                    tembra::Result::Ok(answer)
                }
            };
            let far = 9_007_199_254_740_993_i64;
            assert_eq!(
                call("step", json!([{ "x": 1, "y": far }, "north", 2])).await?,
                json!({ "x": 1, "y": far + 2 })
            );
            assert_eq!(
                call("step", json!([{ "x": -5, "y": 0 }, null, u64::MAX])).await?,
                json!({ "x": -5, "y": 0 })
            );
            assert_eq!(
                call(
                    "area",
                    json!([{ "tag": "rect", "val": { "width": 2, "height": 3 } }])
                )
                .await?,
                json!({ "tag": "ok", "val": 6 })
            );
            assert_eq!(
                call("area", json!([{ "tag": "square", "val": 4 }])).await?,
                json!({ "tag": "ok", "val": 16 })
            );
            assert_eq!(
                call("area", json!([{ "tag": "dot" }])).await?,
                json!({ "tag": "err", "val": "a dot has no area" })
            );
            assert_eq!(
                call("settled", json!([{ "tag": "ok", "val": 6 }, 1])).await?,
                json!(6)
            );
            assert_eq!(
                call("settled", json!([{ "tag": "err", "val": "none" }, 1])).await?,
                json!(1)
            );
            assert_eq!(
                call(
                    "redrawn",
                    json!([{
                        "shape": { "tag": "rect", "val": { "width": 1, "height": 2 } },
                        "pen": { "filled": true }
                    }, 3])
                )
                .await?,
                json!({
                    "shape": { "tag": "rect", "val": { "width": 3, "height": 6 } },
                    "pen": { "dashedLine": true }
                })
            );
            assert_eq!(
                call(
                    "redrawn",
                    json!([{
                        "shape": { "tag": "dot" },
                        "pen": { "filled": false, "dashedLine": true }
                    }, 2])
                )
                .await?,
                json!({ "shape": { "tag": "dot" }, "pen": { "filled": true } })
            );
            assert_eq!(
                call(
                    "sorted",
                    json!([[{ "x": 0, "y": 2 }, { "x": -1, "y": 0 }, { "x": 1, "y": 1 }]])
                )
                .await?,
                json!([["north", "west", null], { "tag": "err", "val": 1 }])
            );
            assert_eq!(
                call("sorted", json!([[]])).await?,
                json!([[], { "tag": "ok" }])
            );
            assert_eq!(
                call(
                    "walk",
                    json!([{ "x": 0, "y": 0 }, [
                        { "tag": "step", "val": 2 },
                        { "tag": "turn", "val": "east" },
                        { "tag": "step", "val": 3 },
                        { "tag": "stay" }
                    ]])
                )
                .await?,
                json!({ "x": 3, "y": 2 })
            );
            Ok(())
        }
        .boxed_local()
    });
}

/// What the boundary's JSON does not spell fails the call as its arguments, before the method
/// runs, and the contract refuses it too: a record with a field missing or one more, an enum case
/// or a variant tag the WIT does not name, serde's spellings of them, a payload where the case
/// carries none or none where it carries one, and a flag the WIT does not name or that is no
/// boolean.
#[test]
fn what_the_contract_refuses_fails_as_arguments() {
    with_main(PLOTTER, |plotter, context| {
        async move {
            let contract = contract();
            let interface = "example:calculator/plotter@1.0.0";
            let origin = json!({ "x": 0, "y": 0 });
            for (method, arguments, message) in [
                (
                    "step",
                    json!([{ "x": 1 }, null, 0]),
                    "missing the field `y`",
                ),
                (
                    "step",
                    json!([{ "x": 1, "y": 2, "z": 3 }, null, 0]),
                    "unexpected field `z`",
                ),
                (
                    "step",
                    json!([[1, 2], null, 0]),
                    "expected a record as an object",
                ),
                ("step", json!([origin, "up", 0]), "unknown case `up`"),
                ("step", json!([origin, "North", 0]), "unknown case `North`"),
                (
                    "area",
                    json!([{ "tag": "circle", "val": 1 }]),
                    "unknown case `circle`",
                ),
                (
                    "area",
                    json!([{ "tag": "square" }]),
                    "the case `square` carries a value, `val`",
                ),
                (
                    "area",
                    json!([{ "tag": "dot", "val": null }]),
                    "the case `dot` carries no value",
                ),
                (
                    "area",
                    json!([{ "tag": "dot", "size": 1 }]),
                    "unexpected field `size`",
                ),
                (
                    "area",
                    json!([{ "Square": 2 }]),
                    "unexpected field `Square`",
                ),
                (
                    "area",
                    json!([{ "tag": "rect", "val": { "width": 1 } }]),
                    "missing the field `height`",
                ),
                ("settled", json!([{ "Ok": 6 }, 1]), "unexpected field `Ok`"),
                (
                    "settled",
                    json!([{ "tag": "ok" }, 1]),
                    "the case `ok` carries a value, `val`",
                ),
                (
                    "redrawn",
                    json!([{ "shape": { "tag": "dot" }, "pen": { "bold": true } }, 1]),
                    "unknown flag `bold`",
                ),
                (
                    "redrawn",
                    json!([{ "shape": { "tag": "dot" }, "pen": { "dashed_line": true } }, 1]),
                    "unknown flag `dashed_line`",
                ),
                (
                    "redrawn",
                    json!([{ "shape": { "tag": "dot" }, "pen": { "filled": 1 } }, 1]),
                    "expected the flag `filled` as a boolean",
                ),
            ] {
                assert!(
                    !accepted(&contract, interface, method, true, &arguments),
                    "{method}: {arguments}"
                );
                let error = plotter
                    .call_portable::<_, Value>(&context, method, arguments)
                    .await
                    .unwrap_err();
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

/// Free JSON, `serde_json::Value`, is a WIT `string` that holds its JSON text: a local caller
/// passes and gets the value, a portable one its text, alone, in a list and in a record of the
/// module, which the macro keeps the author's. The contract accepts what crosses, and a text that
/// holds no JSON, or JSON that is no text, fails the call as its arguments.
#[test]
fn free_json_crosses_as_its_text() {
    for function in [
        "annotated: func(document: string, note: string) -> string;",
        "notes: func(documents: list<string>) -> list<note>;",
    ] {
        assert!(calculator::WIT.contains(function), "{function}");
    }
    with_main(MEMO, |memo, context| {
        async move {
            assert_eq!(
                memo.call::<_, Value>(
                    &context,
                    "annotated",
                    (json!({ "count": 2 }), "seen".to_string())
                )
                .await?,
                json!({ "count": 2, "note": "seen" })
            );
            assert_eq!(
                memo.call::<_, Vec<Note>>(&context, "notes", (vec![json!([1]), Value::Null],))
                    .await?,
                [
                    Note {
                        title: "0".to_string(),
                        body: json!([1]),
                    },
                    Note {
                        title: "1".to_string(),
                        body: Value::Null,
                    },
                ]
            );
            let contract = contract();
            let interface = "example:calculator/memo@1.0.0";
            for (method, arguments, result) in [
                (
                    "annotated",
                    json!([r#"{"tags":["a"],"count":2}"#, "seen"]),
                    json!(r#"{"count":2,"note":"seen","tags":["a"]}"#),
                ),
                ("annotated", json!(["[1,2]", "seen"]), json!("[1,2]")),
                (
                    "notes",
                    json!([[r#"{"k":true}"#, "null"]]),
                    json!([
                        { "title": "0", "body": r#"{"k":true}"# },
                        { "title": "1", "body": "null" },
                    ]),
                ),
            ] {
                assert!(accepted(&contract, interface, method, true, &arguments));
                let answer = memo
                    .call_portable::<_, Value>(&context, method, arguments)
                    .await?;
                assert_eq!(answer, result, "{method}");
                assert!(accepted(&contract, interface, method, false, &answer));
            }
            for (arguments, message) in [
                (
                    json!(["{", "seen"]),
                    "expected JSON text: EOF while parsing an object at line 1 column 1",
                ),
                (
                    json!([{ "count": 2 }, "seen"]),
                    "expected JSON text as a string",
                ),
            ] {
                let error = memo
                    .call_portable::<_, Value>(&context, "annotated", arguments)
                    .await
                    .unwrap_err();
                assert_eq!(error.reason, "arguments", "{error}");
                assert_eq!(error.message, message);
            }
            Ok(())
        }
        .boxed_local()
    });
}
