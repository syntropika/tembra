//! `notebook::orb(..)` in a native host: the Orb takes the host's inputs, the create of `Notes`
//! builds each of its objects from them, `Board` admits only the address its guard names, and a
//! method's `tembra::Result` is the call's error channel.
use futures::{FutureExt, executor::LocalPool};
use serde_json::{Value, json};
use std::{cell::RefCell, rc::Rc};
use tembra::{Code, Host, contracts::Contract};
use tembra_orb_native_fixture::notebook;

const NOTES: &str = "calculator::example:notes/notes@1.0.0";
const BOARD: &str = "calculator::example:notes/board@1.0.0";
const SESSION: &str = "calculator::example:notes/session@1.0.0";

/// What happened in the Orb, in order, as its objects report it to the host.
type Events = Rc<RefCell<Vec<String>>>;

/// The inputs a host lends the Orb: the log of what happened, and a title per address.
fn inputs(events: &Events) -> notebook::Inputs {
    notebook::Inputs {
        events: events.clone(),
        title: Box::new(|address| format!("{} of {}", address.key.join("/"), address.principal)),
    }
}

/// The events that start with `kind`, sorted: objects activate in no fixed order.
fn reported(events: &Events, kind: &str) -> Vec<String> {
    let mut found: Vec<String> = events
        .borrow()
        .iter()
        .filter(|event| event.starts_with(kind))
        .cloned()
        .collect();
    found.sort();
    found
}

/// `orb(..)` takes the Orb's `#[inputs]`, which stay out of the WIT: the Orb declares the plan of
/// the module's contract, as `calculator::orb()` does, and builds no object before a host
/// acquires one.
#[test]
fn orb_takes_the_inputs() {
    let events = Events::default();
    let orb = notebook::orb(inputs(&events)).unwrap();
    let expected = Contract::from_source("notebook.wit", notebook::WIT, Some("notebook"))
        .unwrap()
        .plan("calculator", 2, &Default::default())
        .unwrap()
        .orb;
    assert_eq!(orb.declaration(), &expected);
    assert!(!notebook::WIT.contains("record"), "{}", notebook::WIT);
    assert!(events.borrow().is_empty(), "{:?}", events.borrow());
}

/// Each object, a key and a principal, is built once by the create, from the inputs and the
/// object's address: the same address again reaches the same object, while another key or
/// another principal is another object. A method borrows the object's environment: its title,
/// which the create resolved through the inputs, and its own notes. The create declares the
/// object's state, which the host reads, a resource, which a method reads once it is open and
/// which closes with the object, and a task, which runs once the object is active.
#[test]
fn each_object_is_built_once_by_its_create() {
    let events = Events::default();
    let mut pool = LocalPool::new();
    let host = Host::new(pool.spawner());
    host.install(vec![notebook::orb(inputs(&events)).unwrap()])
        .unwrap();
    let get =
        |key: &str, principal: &str| Rc::new(host.get(NOTES, vec![key.into()], principal).unwrap());
    let objects = [
        get("a", "alice"),
        get("a", "alice"),
        get("b", "alice"),
        get("a", "bob"),
    ];
    let called = objects.clone();
    let answers = pool
        .run_until(host.root("host", move |context| {
            async move {
                let mut answers = Vec::new();
                for object in &called {
                    let title = object.call::<_, String>(&context, "title", ()).await?;
                    let count = object
                        .call::<_, u32>(&context, "add", ("note".to_string(),))
                        .await?;
                    let journal = object
                        .call::<_, Option<String>>(&context, "journal", ())
                        .await?;
                    answers.push((title, count, journal));
                }
                Ok(answers)
            }
            .boxed_local()
        }))
        .unwrap();
    let answer = |title: &str, count: u32, object: &str| {
        (
            title.to_string(),
            count,
            Some(format!("journal of {object}")),
        )
    };
    assert_eq!(
        answers,
        [
            answer("a of alice", 1, "a/alice"),
            answer("a of alice", 2, "a/alice"),
            answer("b of alice", 1, "b/alice"),
            answer("a of bob", 1, "a/bob"),
        ]
    );
    assert_eq!(
        reported(&events, "create "),
        ["create a/alice", "create a/bob", "create b/alice"]
    );
    pool.run_until_stalled();
    for (object, label) in [(0, "a/alice"), (2, "b/alice"), (3, "a/bob")] {
        let state = objects[object].state::<String>().unwrap().value().unwrap();
        assert_eq!(*state, label);
    }
    assert_eq!(
        reported(&events, "open "),
        ["open a/alice", "open a/bob", "open b/alice"]
    );
    assert_eq!(
        reported(&events, "task "),
        ["task a/alice", "task a/bob", "task b/alice"]
    );
    assert!(reported(&events, "close ").is_empty());
    for object in &objects {
        object.release().unwrap();
    }
    pool.run_until(host.close()).unwrap();
    assert_eq!(
        reported(&events, "close "),
        [
            "close journal of a/alice",
            "close journal of a/bob",
            "close journal of b/alice"
        ]
    );
    assert_eq!(reported(&events, "create ").len(), 3);
}

/// `Board`'s guard, `key = main` and `principal = "host"`: acquiring another key or another
/// principal is forbidden, with the reason a hand-written factory gives, and its create never runs
/// for them; the address the guard names is built and answers.
#[test]
fn the_guard_admits_only_its_address() {
    let events = Events::default();
    let mut pool = LocalPool::new();
    let host = Host::new(pool.spawner());
    host.install(vec![notebook::orb(inputs(&events)).unwrap()])
        .unwrap();
    for (key, principal, reason, message) in [
        (
            "other",
            "host",
            "key",
            "the objects of example:notes/board@1.0.0 use the key main",
        ),
        (
            "main",
            "alice",
            "principal",
            "the objects of example:notes/board@1.0.0 belong to the principal host",
        ),
    ] {
        let error = host
            .get(BOARD, vec![key.into()], principal)
            .err()
            .unwrap_or_else(|| panic!("{key}/{principal} was admitted"));
        assert_eq!(error.code, Code::Forbidden, "{error}");
        assert_eq!(error.reason, reason, "{error}");
        assert_eq!(error.message, message);
    }
    assert!(events.borrow().is_empty(), "{:?}", events.borrow());
    let board = Rc::new(host.get(BOARD, vec!["main".into()], "host").unwrap());
    let pinned = board.clone();
    pool.run_until(host.root("host", move |context| {
        async move {
            pinned
                .call::<_, ()>(&context, "pin", ("hello".to_string(),))
                .await
        }
        .boxed_local()
    }))
    .unwrap();
    assert_eq!(*events.borrow(), ["board main/host", "pin hello"]);
    board.release().unwrap();
    pool.run_until(host.close()).unwrap();
}

/// A method returning `tembra::Result<T>` answers `T`, which is all its WIT result carries, or
/// fails the call with its error, the same for a local and a portable caller, through `?` on the
/// object's environment and `.await` too, and without a value for `tembra::Result<()>`. Any other
/// `Result` is a value: its failure comes back as a WIT `result`'s `err`, and the call succeeds.
#[test]
fn a_tembra_result_is_the_error_channel_any_other_result_a_value() {
    for function in [
        "note: func(position: u32) -> string;",
        "position: func(text: string) -> result<u32, string>;",
        "heading: async func() -> string;",
        "holds: func(count: u32);",
    ] {
        assert!(notebook::WIT.contains(function), "{function}");
    }
    let events = Events::default();
    let mut pool = LocalPool::new();
    let host = Host::new(pool.spawner());
    host.install(vec![notebook::orb(inputs(&events)).unwrap()])
        .unwrap();
    let notes = Rc::new(host.get(NOTES, vec!["a".into()], "alice").unwrap());
    let called = notes.clone();
    pool.run_until(host.root("host", move |context| {
        async move {
            let failed = |error: tembra::Error, reason: &str, message: &str| {
                assert_eq!(error.code, Code::Invalid, "{error}");
                assert_eq!(
                    (error.reason.as_str(), error.message.as_str()),
                    (reason, message)
                );
            };
            called
                .call::<_, u32>(&context, "add", ("first".to_string(),))
                .await?;
            assert_eq!(
                called.call::<_, String>(&context, "note", (0u32,)).await?,
                "first"
            );
            let error = called
                .call::<_, String>(&context, "note", (1u32,))
                .await
                .unwrap_err();
            failed(error, "no-note", "there is no note 1");
            assert_eq!(
                called
                    .call::<_, Result<u32, String>>(&context, "position", ("other".to_string(),))
                    .await?,
                Err("no note other".to_string())
            );
            assert_eq!(
                called.call::<_, String>(&context, "heading", ()).await?,
                "a of alice: journal of a/alice"
            );
            called.call::<_, ()>(&context, "holds", (1u32,)).await?;
            let error = called
                .call::<_, ()>(&context, "holds", (2u32,))
                .await
                .unwrap_err();
            failed(error, "too-few", "1 notes, not 2");

            let call = |method: &'static str, arguments: Value| {
                let called = called.clone();
                let context = context.clone();
                async move {
                    called
                        .call_portable::<_, Value>(&context, method, arguments)
                        .await
                }
            };
            assert_eq!(call("note", json!([0])).await?, json!("first"));
            failed(
                call("note", json!([1])).await.unwrap_err(),
                "no-note",
                "there is no note 1",
            );
            assert_eq!(
                call("position", json!(["first"])).await?,
                json!({ "tag": "ok", "val": 0 })
            );
            assert_eq!(
                call("position", json!(["other"])).await?,
                json!({ "tag": "err", "val": "no note other" })
            );
            assert_eq!(
                call("heading", json!([])).await?,
                json!("a of alice: journal of a/alice")
            );
            assert_eq!(call("holds", json!([1])).await?, Value::Null);
            failed(
                call("holds", json!([2])).await.unwrap_err(),
                "too-few",
                "1 notes, not 2",
            );
            Ok(())
        }
        .boxed_local()
    }))
    .unwrap();
    notes.release().unwrap();
    pool.run_until(host.close()).unwrap();
}

/// A method that takes `&Context` reads the call's context, which the WIT leaves out: a local
/// caller passes `()` and a portable caller `[]`, and both get the principal of their root.
#[test]
fn a_method_reads_the_callers_principal_from_its_context() {
    assert!(
        notebook::WIT.contains("caller: func() -> string;"),
        "{}",
        notebook::WIT
    );
    let events = Events::default();
    let mut pool = LocalPool::new();
    let host = Host::new(pool.spawner());
    host.install(vec![notebook::orb(inputs(&events)).unwrap()])
        .unwrap();
    let session = Rc::new(host.get(SESSION, vec!["main".into()], "alice").unwrap());
    let called = session.clone();
    pool.run_until(host.root("alice", move |context| {
        async move {
            assert_eq!(
                called.call::<_, String>(&context, "caller", ()).await?,
                "alice"
            );
            assert_eq!(
                called
                    .call_portable::<_, Value>(&context, "caller", json!([]))
                    .await?,
                json!("alice")
            );
            Ok(())
        }
        .boxed_local()
    }))
    .unwrap();
    session.release().unwrap();
    pool.run_until(host.close()).unwrap();
}
