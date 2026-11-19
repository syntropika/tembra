//! `notebook::orb(..)` in a native host: the Orb takes the host's inputs, the create of `Notes`
//! builds each of its objects from them, `Board` admits only the address its guard names, a
//! method's `tembra::Result` is the call's error channel, the `archive` feature adds what its
//! `#[cfg]`s gate, and the host extends the Orb `orb(..)` builds.
use futures::{FutureExt, executor::LocalPool};
use serde_json::{Value, json};
use std::{cell::RefCell, rc::Rc};
use tembra::{Code, Host, Orb, Service, contracts::Contract, core::Requirement};
use tembra_orb_native_fixture::notebook;

const NOTES: &str = "calculator::example:notes/notes@1.0.0";
const BOARD: &str = "calculator::example:notes/board@1.0.0";
const SESSION: &str = "calculator::example:notes/session@1.0.0";
const SHELF: &str = "calculator::example:notes/shelf@1.0.0";

/// What happened in the Orb, in order, as its objects report it to the host.
type Events = Rc<RefCell<Vec<String>>>;

/// The inputs a host lends the Orb: the log of what happened, a title per address and, with the
/// `archive` feature, an empty archive.
fn inputs(events: &Events) -> notebook::Inputs {
    notebook::Inputs {
        events: events.clone(),
        title: Box::new(|address| format!("{} of {}", address.key.join("/"), address.principal)),
        #[cfg(feature = "archive")]
        archive: Rc::default(),
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
/// acquires one. A `#[local]` method is the native service's alone, so the expectation adds it.
#[test]
fn orb_takes_the_inputs() {
    let events = Events::default();
    let orb = notebook::orb(inputs(&events)).unwrap();
    let mut expected = Contract::from_source("notebook.wit", notebook::WIT, Some("notebook"))
        .unwrap()
        .plan("calculator", 2, &Default::default())
        .unwrap()
        .orb;
    for service in &mut expected.services {
        if service.name == SESSION {
            service.methods.insert("apply".into());
        }
    }
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

/// A `#[local]` method takes what only exists natively, here a closure: a local caller passes it in
/// the tuple of arguments, or through the service's client, whose compiler checks its types, and a
/// portable caller is refused as `local_value`.
#[test]
fn a_local_method_takes_native_values_and_refuses_portable_callers() {
    assert!(!notebook::WIT.contains("apply"), "{}", notebook::WIT);
    let events = Events::default();
    let mut pool = LocalPool::new();
    let host = Host::new(pool.spawner());
    host.install(vec![notebook::orb(inputs(&events)).unwrap()])
        .unwrap();
    let session = Rc::new(host.get(SESSION, vec!["main".into()], "host").unwrap());
    let called = session.clone();
    pool.run_until(host.root("host", move |context| {
        async move {
            let double: Rc<dyn Fn(u32) -> u32> = Rc::new(|value| value * 2);
            assert_eq!(
                called
                    .call::<_, u32>(&context, "apply", (21u32, double))
                    .await?,
                42
            );
            let client = notebook::session::SessionClient::new(called.clone());
            assert_eq!(
                client
                    .apply(&context, 21, Rc::new(|value| value + 1))
                    .await?,
                22
            );
            let error = called
                .call_portable::<_, Value>(&context, "apply", json!([21]))
                .await
                .unwrap_err();
            assert_eq!(error.reason, "local_value", "{error}");
            Ok(())
        }
        .boxed_local()
    }))
    .unwrap();
    session.release().unwrap();
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

/// The `archive` feature adds an input, the Orb's root, a method and a sub-module with a service,
/// each under its `#[cfg]`, and the crate builds with and without it: the macro expands the module
/// once per configuration, so the WIT, the signatures and the declaration have the method and the
/// service exactly when the feature is on, as the contract of the WIT the build carries plans them.
/// Then `orb(..)` takes the archive, the root shares it with the objects, the method moves an
/// object's notes there and the service counts them; without the feature, the host knows neither.
/// The expectation adds the `#[local]` method, as `orb_takes_the_inputs` does.
#[test]
fn the_archive_feature_adds_an_input_a_method_and_a_service() {
    let archive = cfg!(feature = "archive");
    let events = Events::default();
    let orb = notebook::orb(inputs(&events)).unwrap();
    let mut expected = Contract::from_source("notebook.wit", notebook::WIT, Some("notebook"))
        .unwrap()
        .plan("calculator", 2, &Default::default())
        .unwrap()
        .orb;
    for service in &mut expected.services {
        if service.name == SESSION {
            service.methods.insert("apply".into());
        }
    }
    assert_eq!(orb.declaration(), &expected);
    for text in ["archive: func() -> u32;", "interface shelf {"] {
        assert_eq!(
            notebook::WIT.contains(text),
            archive,
            "{text}\n{}",
            notebook::WIT
        );
    }
    let services = &orb.declaration().services;
    let names: Vec<&str> = services
        .iter()
        .map(|service| service.name.as_str())
        .collect();
    let mut expected = vec![NOTES, BOARD, SESSION];
    if archive {
        expected.push(SHELF);
    }
    assert_eq!(names, expected);
    assert_eq!(services[0].methods.contains("archive"), archive);
    let mut pool = LocalPool::new();
    let host = Host::new(pool.spawner());
    host.install(vec![orb]).unwrap();
    let notes = Rc::new(host.get(NOTES, vec!["a".into()], "alice").unwrap());
    let shelf = host.get(SHELF, vec!["main".into()], "host");
    if let Err(error) = &shelf {
        assert!(!archive, "{error}");
        assert_eq!(error.reason, "unknown_service", "{error}");
    }
    let shelf = shelf.ok().map(Rc::new);
    let (called, counted) = (notes.clone(), shelf.clone());
    let (moved, count) = pool
        .run_until(host.root("host", move |context| {
            async move {
                for text in ["first", "second"] {
                    called
                        .call::<_, u32>(&context, "add", (text.to_string(),))
                        .await?;
                }
                let moved = called.call::<_, u32>(&context, "archive", ()).await;
                let count = match counted {
                    Some(shelf) => Some(shelf.call::<_, u32>(&context, "count", ()).await?),
                    None => None,
                };
                Ok((moved, count))
            }
            .boxed_local()
        }))
        .unwrap();
    if archive {
        assert_eq!(moved.unwrap(), 2);
        assert_eq!(count, Some(2));
    } else {
        let error = moved.unwrap_err();
        assert_eq!(error.reason, "method", "{error}");
        assert_eq!(count, None);
    }
    notes.release().unwrap();
    if let Some(shelf) = shelf {
        shelf.release().unwrap();
    }
    pool.run_until(host.close()).unwrap();
}

/// `orb(..)` returns the runtime's `Orb`, an ordinary builder: a host appends a service written by
/// hand and a requirement on another Orb, which that service calls, and installs the result beside
/// that Orb. The declaration is the macro's with both appended; the host refuses the Orb alone,
/// whose requirement nothing provides; and once both are installed, the appended service reaches
/// the other Orb through the requirement while the macro's services answer as before.
#[test]
fn orb_stays_an_ordinary_builder() {
    const CLOCK: &str = "clock::time";
    const STAMP: &str = "calculator::stamp";
    let events = Events::default();
    let requirement = Requirement {
        signatures: Default::default(),
        hook_signatures: Default::default(),
        orb: "clock".into(),
        api_version: 1,
        methods: [(CLOCK.into(), ["now".into()].into())].into(),
        hooks: Default::default(),
    };
    let extended = || {
        notebook::orb(inputs(&events))
            .unwrap()
            .service(
                Service::new(STAMP, |declaration| {
                    declaration.get(CLOCK, vec!["main".into()])
                })
                .method(
                    "stamp",
                    |clock, context, (text,): (String,)| async move {
                        let now = clock.call::<_, u32>(&context, "now", ()).await?;
                        Ok(format!("{text} at {now}"))
                    },
                ),
            )
            .requires(requirement.clone())
    };
    let clock = Orb::new("clock", 1).service(
        Service::new(CLOCK, |_| Ok(())).method("now", |_, _, (): ()| async move { Ok(7u32) }),
    );
    let mut expected = notebook::orb(inputs(&events))
        .unwrap()
        .declaration()
        .clone();
    expected
        .services
        .push(tembra::core::Service::new(STAMP, ["stamp"]));
    expected.requires.push(requirement.clone());
    assert_eq!(extended().declaration(), &expected);
    let mut pool = LocalPool::new();
    let host = Host::new(pool.spawner());
    let error = host.install(vec![extended()]).unwrap_err();
    assert_eq!(
        (error.code, error.reason.as_str(), error.message.as_str()),
        (Code::Unavailable, "dependency", "clock"),
        "{error}"
    );
    host.install(vec![extended(), clock]).unwrap();
    let stamp = Rc::new(host.get(STAMP, vec!["main".into()], "host").unwrap());
    let notes = Rc::new(host.get(NOTES, vec!["a".into()], "alice").unwrap());
    let (stamped, added) = (stamp.clone(), notes.clone());
    let answers = pool
        .run_until(host.root("host", move |context| {
            async move {
                let stamped = stamped
                    .call::<_, String>(&context, "stamp", ("noted".to_string(),))
                    .await?;
                let count = added
                    .call::<_, u32>(&context, "add", ("note".to_string(),))
                    .await?;
                Ok((stamped, count))
            }
            .boxed_local()
        }))
        .unwrap();
    assert_eq!(answers, ("noted at 7".to_string(), 1));
    stamp.release().unwrap();
    notes.release().unwrap();
    pool.run_until(host.close()).unwrap();
}

/// A newtype whose `TryFrom` refuses some values of its field is native only, as the notebook is: a
/// local caller passes the author's struct, which only its `TryFrom` builds, and a portable caller
/// the field, which the contract accepts, read back through the same `TryFrom`. A value it refuses
/// fails that call alone, as its arguments, with its message, before the method runs, and the next
/// call is served.
#[test]
fn a_value_a_newtype_refuses_fails_only_its_call() {
    assert!(
        notebook::WIT.contains("tagged: func(tag: string) -> string;"),
        "{}",
        notebook::WIT
    );
    let contract = Contract::from_source("notebook.wit", notebook::WIT, Some("notebook")).unwrap();
    let slot = contract
        .bind("example:notes/session@1.0.0", "tagged", true)
        .unwrap();
    let events = Events::default();
    let mut pool = LocalPool::new();
    let host = Host::new(pool.spawner());
    host.install(vec![notebook::orb(inputs(&events)).unwrap()])
        .unwrap();
    let session = Rc::new(host.get(SESSION, vec!["main".into()], "host").unwrap());
    let called = session.clone();
    pool.run_until(host.root("host", move |context| {
        async move {
            let urgent = notebook::Tag::try_from("urgent".to_string()).unwrap();
            assert_eq!(
                called
                    .call::<_, String>(&context, "tagged", (urgent,))
                    .await?,
                "#urgent"
            );
            for (tag, message) in [("", "`` is no tag"), ("two words", "`two words` is no tag")] {
                let arguments = json!([tag]);
                assert!(contract.validate_slot(slot, true, &arguments).is_ok());
                let error = called
                    .call_portable::<_, Value>(&context, "tagged", arguments)
                    .await
                    .unwrap_err();
                assert_eq!(
                    (error.reason.as_str(), error.message.as_str()),
                    ("arguments", message)
                );
                assert_eq!(
                    called
                        .call_portable::<_, Value>(&context, "tagged", json!(["later"]))
                        .await?,
                    json!("#later")
                );
            }
            Ok(())
        }
        .boxed_local()
    }))
    .unwrap();
    session.release().unwrap();
    pool.run_until(host.close()).unwrap();
}
