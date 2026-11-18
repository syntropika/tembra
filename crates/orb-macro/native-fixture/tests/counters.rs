//! `counters::orb(..)` beside the same Orb written by hand against the runtime's API, as a native
//! host writes one without the macro: both declare the same Orb to the kernel, also once the host
//! appends a service of its own to each, and both answer the same local and portable calls with the
//! same results and the same effects, which their inputs report to the host.
use futures::{FutureExt, executor::LocalPool};
use serde_json::{Value, json};
use std::{cell::RefCell, fmt::Debug, rc::Rc};
use tembra::{Context, Host, Orb, Reference, Service, contracts::Contract, core::Address};
use tembra_orb_native_fixture::counters;

const COUNTER: &str = "calculator::example:counter/counter@1.0.0";
const BOARD: &str = "calculator::example:counter/board@1.0.0";
/// The service the host appends to the Orb, named as a service written by hand may be.
const AUDIT: &str = "calculator::audit";

/// What happened in the Orb, in order, as it reports it to the host.
type Events = Rc<RefCell<Vec<String>>>;

/// The Orb written by hand. It provides the contract `WIT`, written by hand too, whose plan gives
/// its services their names and signatures, as a native provider of a WIT contract reads them; its
/// create, each service's factory and each method are closures over the runtime's types; and a
/// method takes its arguments as a tuple, which serde reads from a portable caller's JSON.
mod by_hand {
    use super::{BOARD, COUNTER};
    use std::{
        cell::{Cell, RefCell},
        rc::Rc,
    };
    use tembra::{
        Code, Context, Error, Orb, Resource, Result, Service, State, contracts::Contract,
        core::Address,
    };

    /// The contract the Orb provides.
    pub const WIT: &str = "package example:counter@1.0.0;

interface counter {
  step: func() -> u32;
  step-by: func(steps: u32) -> u32;
  count: func() -> u32;
  display: func() -> string;
}

interface board {
  steps: func() -> u32;
  tape: func() -> list<string>;
}

world counters {
  export counter;
  export board;
}
";

    /// What the host lends the Orb.
    pub struct Inputs {
        /// What happened, in order, for the host to read back.
        pub events: Rc<RefCell<Vec<String>>>,
        /// The highest count of a counter, resolved from its address.
        pub limit: Box<dyn Fn(&Address) -> u32>,
    }

    /// What every counter shares, which the Orb's create makes: the steps they counted in all,
    /// and the tape each count they reach is written on, a resource of the Orb.
    struct Tally {
        steps: Cell<u32>,
        tape: Resource<RefCell<Vec<String>>>,
    }

    /// The environment of a counter object, which its factory builds.
    struct Counter {
        name: String,
        count: State<u32>,
        display: Resource<String>,
        tally: Rc<Tally>,
    }

    impl Counter {
        /// Count `steps` steps, none past the counter's limit, and write the count on the tape.
        fn step_by(&self, context: &Context, steps: u32) -> Result<u32> {
            let count = *self.count.update(context, |count| Ok(count + steps))?;
            self.tally.steps.set(self.tally.steps.get() + steps);
            self.tally
                .tape
                .value()?
                .borrow_mut()
                .push(format!("{} at {count}", self.name));
            Ok(count)
        }
    }

    /// The Orb, under the name and API version the manifest of the crate gives its Orbs, which
    /// `orb(..)` of a module of the crate reads.
    pub fn orb(inputs: Inputs) -> Result<Orb> {
        let plan = Contract::from_source("counters.wit", WIT, Some("counters"))
            .and_then(|contract| contract.plan("calculator", 2, &Default::default()))
            .map_err(|error| Error::invalid("contract", error.to_string()))?;
        let signature = |name: &str| {
            plan.orb
                .services
                .iter()
                .find(|service| service.name == name)
                .and_then(|service| service.signature.clone())
        };
        let inputs = Rc::new(inputs);
        let (root, counter, board) = (inputs.clone(), inputs.clone(), inputs);
        Ok(Orb::new("calculator", 2)
            .create(move |declaration| {
                root.events.borrow_mut().push("root".to_string());
                let (opened, closed) = (root.events.clone(), root.events.clone());
                let tape = declaration.resource(
                    "tape",
                    1,
                    move || {
                        opened.borrow_mut().push("open tape".to_string());
                        Ok(RefCell::default())
                    },
                    move |tape: Rc<RefCell<Vec<String>>>, _| {
                        Box::pin(async move {
                            let counts = tape.borrow().len();
                            closed
                                .borrow_mut()
                                .push(format!("close tape of {counts} counts"));
                            Ok(())
                        })
                    },
                )?;
                Ok(Tally {
                    steps: Cell::new(0),
                    tape,
                })
            })
            .service(
                Service::new(COUNTER, move |declaration| {
                    let address = declaration.address()?;
                    let name = format!("{}/{}", address.key.join("/"), address.principal);
                    counter.events.borrow_mut().push(format!("create {name}"));
                    let limit = (counter.limit)(&address);
                    let count = declaration.state(0, 1, move |count: &u32| {
                        if *count <= limit {
                            Ok(())
                        } else {
                            Err(Error::invalid(
                                "limit",
                                format!("{count} is past the limit of {limit}"),
                            ))
                        }
                    })?;
                    let (opened, closed) = (counter.events.clone(), counter.events.clone());
                    let label = name.clone();
                    let display = declaration.resource(
                        "display",
                        1,
                        move || {
                            opened.borrow_mut().push(format!("open display of {label}"));
                            Ok(format!("display of {label}"))
                        },
                        move |display, _| {
                            Box::pin(async move {
                                closed.borrow_mut().push(format!("close {display}"));
                                Ok(())
                            })
                        },
                    )?;
                    let (events, task) = (counter.events.clone(), name.clone());
                    declaration.task(move |scope| {
                        Box::pin(async move {
                            events.borrow_mut().push(format!("task of {task} starts"));
                            scope.signal().cancelled().await;
                            events.borrow_mut().push(format!("task of {task} stops"));
                            Ok(())
                        })
                    })?;
                    Ok(Counter {
                        name,
                        count,
                        display,
                        tally: declaration.orb::<Tally>()?,
                    })
                })
                .signature(signature(COUNTER))
                .method("step", |counter: Rc<Counter>, context, (): ()| async move {
                    counter.step_by(&context, 1)
                })
                .method(
                    "step-by",
                    |counter: Rc<Counter>, context, (steps,): (u32,)| async move {
                        counter.step_by(&context, steps)
                    },
                )
                .method("count", |counter: Rc<Counter>, _, (): ()| async move {
                    Ok(*counter.count.value()?)
                })
                .method("display", |counter: Rc<Counter>, _, (): ()| async move {
                    Ok(format!(
                        "{}: {}",
                        counter.display.value()?,
                        counter.count.value()?
                    ))
                })
                .local_method(
                    "apply",
                    |counter: Rc<Counter>,
                     context,
                     (operation,): (Rc<dyn Fn(u32) -> u32>,)| async move {
                        Ok(*counter
                            .count
                            .update(&context, |count| Ok(operation(*count)))?)
                    },
                ),
            )
            .service(
                Service::new(BOARD, move |declaration| {
                    let address = declaration.address()?;
                    if address.key != ["main"] {
                        return Err(Error::new(
                            Code::Forbidden,
                            "key",
                            "the objects of example:counter/board@1.0.0 use the key main",
                        ));
                    }
                    if address.principal != "host" {
                        return Err(Error::new(
                            Code::Forbidden,
                            "principal",
                            "the objects of example:counter/board@1.0.0 belong to the principal host",
                        ));
                    }
                    board.events.borrow_mut().push("create board".to_string());
                    declaration.orb::<Tally>()
                })
                .signature(signature(BOARD))
                .method("steps", |tally: Rc<Rc<Tally>>, _, (): ()| async move {
                    Ok(tally.steps.get())
                })
                .method("tape", |tally: Rc<Rc<Tally>>, _, (): ()| async move {
                    Ok(tally.tape.value()?.borrow().clone())
                }),
            ))
    }
}

/// The limit of a counter: 3 at the key `a`, 10 at any other.
fn limit(address: &Address) -> u32 {
    if address.key == ["a"] { 3 } else { 10 }
}

/// The Orb written by hand, which reports to `events`.
fn written_by_hand(events: &Events) -> Orb {
    by_hand::orb(by_hand::Inputs {
        events: events.clone(),
        limit: Box::new(limit),
    })
    .unwrap()
}

/// The Orb rewritten with the macro, which reports to `events`.
fn rewritten(events: &Events) -> Orb {
    counters::orb(counters::Inputs {
        events: events.clone(),
        limit: Box::new(limit),
    })
    .unwrap()
}

/// The service a host writes by hand and appends to the Orb, either one: its objects reach the
/// Orb's board through the reference their factory acquires.
fn audit() -> Service<Rc<Reference>> {
    Service::new(AUDIT, |declaration| {
        declaration.get(BOARD, vec!["main".into()])
    })
    .method("steps", |board, context, (): ()| async move {
        board.call::<_, u32>(&context, "steps", ()).await
    })
}

/// What a local call of `method` with the Rust value `arguments` answered: its value or its error,
/// after `label`.
async fn local<A: 'static, R: Debug + 'static>(
    label: &str,
    reference: &Reference,
    context: &Context,
    method: &str,
    arguments: A,
) -> String {
    let answer = match reference.call::<A, R>(context, method, arguments).await {
        Ok(value) => format!("{value:?}"),
        Err(error) => format!("error {error}"),
    };
    format!("{label} {method}: {answer}")
}

/// What a portable call of `method` with the JSON `arguments` answered: its JSON or its error,
/// after `label`.
async fn portable(
    label: &str,
    reference: &Reference,
    context: &Context,
    method: &str,
    arguments: Value,
) -> String {
    let sent = arguments.to_string();
    let answer = match reference
        .call_portable::<_, Value>(context, method, arguments)
        .await
    {
        Ok(value) => value.to_string(),
        Err(error) => format!("error {error}"),
    };
    format!("{label} {method} {sent}: {answer}")
}

/// Install `orb`, with the service the host appends, and make the same calls on it: acquisitions of
/// the board that its guard refuses, then, under a root of `host`, local calls with Rust values and
/// portable calls with the boundary's JSON to three counters, the board and the appended service,
/// and the host's read of a counter's state. What each answered, in order, and what the Orb
/// reported to the host, in order, by the time the host closed. A portable caller of the Orb
/// written by hand sends no arguments as `null`, which serde reads as `()`, and the codec reads
/// too.
fn session(orb: fn(&Events) -> Orb) -> (Vec<String>, Vec<String>) {
    let events = Events::default();
    let mut pool = LocalPool::new();
    let host = Host::new(pool.spawner());
    host.install(vec![orb(&events).service(audit())]).unwrap();
    let mut answers: Vec<String> = [("other", "host"), ("main", "alice")]
        .into_iter()
        .map(|(key, principal)| {
            let answer = match host.get(BOARD, vec![key.into()], principal) {
                Ok(_) => "admitted".to_string(),
                Err(error) => format!("error {error}"),
            };
            format!("board {key}/{principal}: {answer}")
        })
        .collect();
    let get = |service: &str, key: &str, principal: &str| {
        Rc::new(host.get(service, vec![key.into()], principal).unwrap())
    };
    let references = [
        get(COUNTER, "a", "alice"),
        get(COUNTER, "b", "alice"),
        get(COUNTER, "a", "bob"),
        get(BOARD, "main", "host"),
        get(AUDIT, "main", "host"),
    ];
    let [a, b, bob, board, audit] = references.clone();
    let called = pool
        .run_until(host.root("host", move |context| {
            async move {
                let context = &context;
                let increment: Rc<dyn Fn(u32) -> u32> = Rc::new(|count| count + 1);
                let double: Rc<dyn Fn(u32) -> u32> = Rc::new(|count| count * 2);
                Ok(vec![
                    local::<_, u32>("a/alice", &a, context, "step", ()).await,
                    local::<_, u32>("a/alice", &a, context, "step-by", (1u32,)).await,
                    local::<_, u32>("a/alice", &a, context, "step-by", (5u32,)).await,
                    local::<_, u32>("a/alice", &a, context, "apply", (increment,)).await,
                    local::<_, u32>("a/alice", &a, context, "apply", (double,)).await,
                    local::<_, u32>("a/alice", &a, context, "count", ()).await,
                    local::<_, String>("a/alice", &a, context, "display", ()).await,
                    local::<_, u32>("b/alice", &b, context, "step-by", (4u32,)).await,
                    local::<_, u32>("a/bob", &bob, context, "step", ()).await,
                    local::<_, u32>("board", &board, context, "steps", ()).await,
                    local::<_, Vec<String>>("board", &board, context, "tape", ()).await,
                    local::<_, u32>("audit", &audit, context, "steps", ()).await,
                    portable("a/alice", &a, context, "step", Value::Null).await,
                    portable("b/alice", &b, context, "step-by", json!([3])).await,
                    portable("b/alice", &b, context, "count", Value::Null).await,
                    portable("b/alice", &b, context, "display", Value::Null).await,
                    portable("a/alice", &a, context, "apply", json!([2])).await,
                    portable("board", &board, context, "steps", Value::Null).await,
                    portable("board", &board, context, "tape", Value::Null).await,
                    portable("audit", &audit, context, "steps", Value::Null).await,
                ])
            }
            .boxed_local()
        }))
        .unwrap();
    answers.extend(called);
    let state = references[0]
        .state::<u32>()
        .and_then(|state| state.value())
        .unwrap();
    answers.push(format!("a/alice state: {state}"));
    for reference in &references {
        reference.release().unwrap();
    }
    pool.run_until(host.close()).unwrap();
    assert!(host.errors().is_empty(), "{:?}", host.errors());
    (answers, events.take())
}

/// `orb(..)` declares the Orb written by hand: the plan of the contract that Orb provides, from
/// WIT written by hand, which the macro derives from the module instead, with the same service
/// names, signatures and methods, the local method among them. Once the host appends its own
/// service to each, both declare it after the Orb's. Neither runs the root or a create before a
/// host installs it.
#[test]
fn the_rewritten_orb_declares_the_orb_written_by_hand() {
    let events = Events::default();
    let (by_hand, rewritten) = (written_by_hand(&events), rewritten(&events));
    let mut planned = Contract::from_source("counters.wit", by_hand::WIT, Some("counters"))
        .unwrap()
        .plan("calculator", 2, &Default::default())
        .unwrap()
        .orb;
    for service in &mut planned.services {
        if service.name == COUNTER {
            service.methods.insert("apply".into());
        }
    }
    assert_eq!(by_hand.declaration(), &planned);
    assert_eq!(rewritten.declaration(), by_hand.declaration());
    let (by_hand, rewritten) = (by_hand.service(audit()), rewritten.service(audit()));
    assert_eq!(rewritten.declaration(), by_hand.declaration());
    let names: Vec<&str> = rewritten
        .declaration()
        .services
        .iter()
        .map(|service| service.name.as_str())
        .collect();
    assert_eq!(names, [COUNTER, BOARD, AUDIT]);
    assert!(events.borrow().is_empty(), "{:?}", events.borrow());
}

/// The same calls return the same results from the Orb written by hand and from the rewritten
/// one, with the same effects in the same order: the root runs when the host installs the Orb; the
/// board's guard refuses another key and another principal before its create runs; each create
/// runs as the host acquires its object, and once the host's executor runs, the tape opens, then
/// each display, and each task starts. A count past a counter's limit, which the state's rule
/// refuses, fails that call alone; the local method takes a closure, which a portable caller
/// cannot send; the board reads what the counters share; and the appended service reaches the
/// board. As the host closes, each task stops, then each display and the tape close.
#[test]
fn the_same_calls_return_the_same_results() {
    let by_hand = session(written_by_hand);
    assert_eq!(session(rewritten), by_hand);
    let (answers, events) = by_hand;
    assert_eq!(
        answers,
        [
            "board other/host: error forbidden/key: the objects of example:counter/board@1.0.0 use the key main",
            "board main/alice: error forbidden/principal: the objects of example:counter/board@1.0.0 belong to the principal host",
            "a/alice step: 1",
            "a/alice step-by: 2",
            "a/alice step-by: error invalid/limit: 7 is past the limit of 3",
            "a/alice apply: 3",
            "a/alice apply: error invalid/limit: 6 is past the limit of 3",
            "a/alice count: 3",
            "a/alice display: \"display of a/alice: 3\"",
            "b/alice step-by: 4",
            "a/bob step: 1",
            "board steps: 7",
            "board tape: [\"a/alice at 1\", \"a/alice at 2\", \"b/alice at 4\", \"a/bob at 1\"]",
            "audit steps: 7",
            "a/alice step null: error invalid/limit: 4 is past the limit of 3",
            "b/alice step-by [3]: 7",
            "b/alice count null: 7",
            "b/alice display null: \"display of b/alice: 7\"",
            "a/alice apply [2]: error invalid/local_value: apply is a host-local method without a portable entrypoint",
            "board steps null: 10",
            "board tape null: [\"a/alice at 1\",\"a/alice at 2\",\"b/alice at 4\",\"a/bob at 1\",\"b/alice at 7\"]",
            "audit steps null: 10",
            "a/alice state: 3",
        ]
    );
    assert_eq!(
        events,
        [
            "root",
            "create a/alice",
            "create b/alice",
            "create a/bob",
            "create board",
            "open tape",
            "open display of a/alice",
            "open display of b/alice",
            "open display of a/bob",
            "task of a/alice starts",
            "task of b/alice starts",
            "task of a/bob starts",
            "task of a/alice stops",
            "task of b/alice stops",
            "task of a/bob stops",
            "close display of a/alice",
            "close display of b/alice",
            "close display of a/bob",
            "close tape of 5 counts",
        ]
    );
}
