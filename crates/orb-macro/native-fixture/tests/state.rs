//! The walker of `calculator::orb()` in a native host: the kernel keeps the `#[state]` of each
//! walker object, which its methods read and update through the kernel, natively as in a component.
//! The host reads the state as the JSON serde writes, which is what a component keeps for the same
//! value; the executor's `orb_macro_runtime` test compares the two.
use futures::{FutureExt, executor::LocalPool};
use serde_json::{Value, json};
use std::rc::Rc;
use tembra::{Code, Host, Reference};
use tembra_orb_native_fixture::{
    calculator::{self, Heading, Move, Point, Position},
    set_grid,
};

const WALKER: &str = "calculator::example:calculator/walker@1.0.0";

/// A host with the Orb installed, and a reference to the walker of `key`, ready, under the
/// principal `host`.
fn walker(pool: &mut LocalPool, host: &Host, key: &str) -> Rc<Reference> {
    let walker = Rc::new(host.get(WALKER, vec![key.into()], "host").unwrap());
    pool.run_until(walker.ready()).unwrap();
    walker
}

/// Call a method of the walker portably, with the boundary's JSON, under a root of `host`.
fn portable(
    pool: &mut LocalPool,
    host: &Host,
    walker: &Rc<Reference>,
    method: &'static str,
    arguments: Value,
) -> Value {
    let called = walker.clone();
    pool.run_until(host.root("host", move |context| {
        async move {
            called
                .call_portable::<_, Value>(&context, method, arguments)
                .await
        }
        .boxed_local()
    }))
    .unwrap_or_else(|error| panic!("{method}: {error}"))
}

/// Make the moves of `path` with a local call, under a root of `host`.
fn walk(
    pool: &mut LocalPool,
    host: &Host,
    walker: &Rc<Reference>,
    method: &'static str,
    path: Vec<Move>,
) -> Result<Position, String> {
    let called = walker.clone();
    pool.run_until(host.root("host", move |context| {
        async move {
            called
                .call::<_, Result<Position, String>>(&context, method, (path,))
                .await
        }
        .boxed_local()
    }))
    .unwrap_or_else(|error| panic!("{method}: {error}"))
}

/// The kernel keeps one state per walker object, from the state's `Default`: a method updates it,
/// locally or portably, and reads it, and the host reads it too, as the JSON serde writes, with the
/// Rust names of its fields and of its enum's cases, where the boundary writes the WIT's. Another
/// object of the service has its own state.
#[test]
fn a_method_reads_and_updates_the_state_of_its_object() {
    let mut pool = LocalPool::new();
    let host = Host::new(pool.spawner());
    host.install(vec![calculator::orb().unwrap()]).unwrap();
    let main = walker(&mut pool, &host, "main");
    let other = walker(&mut pool, &host, "other");
    let state = main.state::<Value>().unwrap();
    let start = json!({ "at": { "x": 0, "y": 0 }, "facing": "North", "moves": 0 });
    assert_eq!(*state.value().unwrap(), start);
    let path = vec![
        Move::Step(2),
        Move::Turn(Heading::West),
        Move::Step(1),
        Move::Turn(Heading::East),
    ];
    let ended = Position {
        at: Point { x: -1, y: 2 },
        facing: Heading::East,
        moves: 4,
    };
    assert_eq!(walk(&mut pool, &host, &main, "walk", path), Ok(ended));
    assert_eq!(
        *state.value().unwrap(),
        json!({ "at": { "x": -1, "y": 2 }, "facing": "East", "moves": 4 })
    );
    assert_eq!(
        portable(&mut pool, &host, &main, "here", json!([])),
        json!({ "at": { "x": -1, "y": 2 }, "facing": "east", "moves": 4 })
    );
    assert_eq!(
        portable(
            &mut pool,
            &host,
            &main,
            "walk",
            json!([[{ "tag": "turn", "val": "south" }, { "tag": "step", "val": 3 }]])
        ),
        json!({ "tag": "ok", "val": { "at": { "x": -1, "y": -1 }, "facing": "south", "moves": 6 } })
    );
    assert_eq!(
        *state.value().unwrap(),
        json!({ "at": { "x": -1, "y": -1 }, "facing": "South", "moves": 6 })
    );
    assert_eq!(*other.state::<Value>().unwrap().value().unwrap(), start);
    assert_eq!(
        portable(&mut pool, &host, &other, "here", json!([])),
        json!({ "at": { "x": 0, "y": 0 }, "facing": "north", "moves": 0 })
    );
    main.release().unwrap();
    other.release().unwrap();
    pool.run_until(host.close()).unwrap();
    assert!(host.errors().is_empty(), "{:?}", host.errors());
}

/// An update the state's `#[validate]` rule refuses is reported as in a component: `update`
/// returns the refusal, which the method answers as the `err` of its WIT result, to a local and a
/// portable caller, and the kernel keeps the previous value, which the host still reads.
#[test]
fn a_refused_update_leaves_the_state_as_it_was() {
    let mut pool = LocalPool::new();
    let host = Host::new(pool.spawner());
    host.install(vec![calculator::orb().unwrap()]).unwrap();
    let main = walker(&mut pool, &host, "main");
    let state = main.state::<Value>().unwrap();
    assert_eq!(
        walk(&mut pool, &host, &main, "walk", vec![Move::Step(1)]),
        Ok(Position {
            at: Point { x: 0, y: 1 },
            facing: Heading::North,
            moves: 1,
        })
    );
    let kept = json!({ "at": { "x": 0, "y": 1 }, "facing": "North", "moves": 1 });
    assert_eq!(*state.value().unwrap(), kept);
    assert_eq!(
        walk(
            &mut pool,
            &host,
            &main,
            "walk",
            vec![Move::Turn(Heading::East), Move::Step(1001)]
        ),
        Err("(1001, 1) is off the grid".to_string())
    );
    assert_eq!(
        portable(
            &mut pool,
            &host,
            &main,
            "walk",
            json!([[{ "tag": "step", "val": 1000 }]])
        ),
        json!({ "tag": "err", "val": "(0, 1001) is off the grid" })
    );
    assert_eq!(*state.value().unwrap(), kept);
    assert_eq!(
        portable(&mut pool, &host, &main, "here", json!([])),
        json!({ "at": { "x": 0, "y": 1 }, "facing": "north", "moves": 1 })
    );
    main.release().unwrap();
    pool.run_until(host.close()).unwrap();
    assert!(host.errors().is_empty(), "{:?}", host.errors());
}

/// The kernel refuses the value of a call cancelled before it stores it, and keeps the previous
/// one: `update` answers with the value kept, as a component's does, so `stumble`, which cancels
/// its call before it walks, returns where the walker stood and leaves it there.
#[test]
fn a_cancelled_call_keeps_the_state() {
    let mut pool = LocalPool::new();
    let host = Host::new(pool.spawner());
    host.install(vec![calculator::orb().unwrap()]).unwrap();
    let main = walker(&mut pool, &host, "main");
    let state = main.state::<Value>().unwrap();
    let stood = Position {
        at: Point { x: 0, y: 2 },
        facing: Heading::North,
        moves: 1,
    };
    assert_eq!(
        walk(&mut pool, &host, &main, "walk", vec![Move::Step(2)]),
        Ok(stood)
    );
    assert_eq!(
        walk(&mut pool, &host, &main, "stumble", vec![Move::Step(5)]),
        Ok(stood)
    );
    assert_eq!(
        *state.value().unwrap(),
        json!({ "at": { "x": 0, "y": 2 }, "facing": "North", "moves": 1 })
    );
    main.release().unwrap();
    pool.run_until(host.close()).unwrap();
    assert!(host.errors().is_empty(), "{:?}", host.errors());
}

/// The state belongs to the kernel, not to the Orb's code: a reload builds each object again for
/// the new code, which reads the value it inherits as its state's type and runs the rule on it
/// before it adopts it, and the walker stands where it stood.
#[test]
fn a_reload_keeps_the_state() {
    let mut pool = LocalPool::new();
    let host = Host::new(pool.spawner());
    host.install(vec![calculator::orb().unwrap()]).unwrap();
    let main = walker(&mut pool, &host, "main");
    let path = vec![Move::Turn(Heading::West), Move::Step(7)];
    let stood = Position {
        at: Point { x: -7, y: 0 },
        facing: Heading::West,
        moves: 2,
    };
    assert_eq!(walk(&mut pool, &host, &main, "walk", path), Ok(stood));
    pool.run_until(host.reload(vec![calculator::orb().unwrap()], 5_000))
        .unwrap();
    pool.run_until(host.settled());
    assert_eq!(
        portable(&mut pool, &host, &main, "here", json!([])),
        json!({ "at": { "x": -7, "y": 0 }, "facing": "west", "moves": 2 })
    );
    assert_eq!(
        *main.state::<Value>().unwrap().value().unwrap(),
        json!({ "at": { "x": -7, "y": 0 }, "facing": "West", "moves": 2 })
    );
    main.release().unwrap();
    pool.run_until(host.close()).unwrap();
    assert!(host.errors().is_empty(), "{:?}", host.errors());
}

/// A reload runs the rule of the state on the value the new code inherits, as a component's does:
/// once the grid narrows, a walker that stands off it is a value the new code's rule refuses, so
/// the reload fails as `invalid/state`, naming the refusal, and the running code keeps serving the
/// walker where it stands.
#[test]
fn a_reload_refuses_a_state_the_rule_of_the_new_code_refuses() {
    set_grid(1000);
    let mut pool = LocalPool::new();
    let host = Host::new(pool.spawner());
    host.install(vec![calculator::orb().unwrap()]).unwrap();
    let main = walker(&mut pool, &host, "main");
    let stood = Position {
        at: Point { x: 0, y: 5 },
        facing: Heading::North,
        moves: 1,
    };
    assert_eq!(
        walk(&mut pool, &host, &main, "walk", vec![Move::Step(5)]),
        Ok(stood)
    );
    set_grid(3);
    let error = pool
        .run_until(host.reload(vec![calculator::orb().unwrap()], 5_000))
        .unwrap_err();
    set_grid(1000);
    assert_eq!(
        (error.code, error.reason.as_str(), error.message.as_str()),
        (
            Code::Invalid,
            "state",
            "the state was refused: (0, 5) is off the grid"
        ),
        "{error}"
    );
    pool.run_until(host.settled());
    assert_eq!(
        portable(&mut pool, &host, &main, "here", json!([])),
        json!({ "at": { "x": 0, "y": 5 }, "facing": "north", "moves": 1 })
    );
    assert_eq!(
        *main.state::<Value>().unwrap().value().unwrap(),
        json!({ "at": { "x": 0, "y": 5 }, "facing": "North", "moves": 1 })
    );
    main.release().unwrap();
    pool.run_until(host.close()).unwrap();
}
