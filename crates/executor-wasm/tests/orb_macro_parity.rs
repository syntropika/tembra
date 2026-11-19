//! One `#[tembra::orb]` module answers alike as a native Orb and as a component. The parity
//! fixture's Orb builds both ways: `parity::orb(..)` natively, and the component that
//! `just build-orb-parity` builds from the same crate, which `just test-orb-parity` and acceptance
//! pass in `TEMBRA_WASM_ORB_PARITY`. The test installs each build in a host of its own, beside the
//! same native provider of the dials the Orb imports, and compares the services each build
//! declares, by their identities. It runs every scenario of `scenarios()` on both, each in a root
//! of its own, and compares what each step answers: a call's value, or its error's code and reason,
//! the claims the kernel holds, a state's JSON, or what the provider's `hold` saw; and the address
//! of each object of the provider the step's calls reached, its service, key and principal. The
//! steps name a service by its interface: a new scenario is a module of the fixture, or a function
//! of one, and a row of the list.
use futures::{FutureExt, executor::LocalPool};
use serde_json::{Value, json};
use std::{
    cell::{Cell, RefCell},
    collections::BTreeMap,
    path::Path,
    rc::Rc,
    task::Poll,
    time::{Duration, Instant},
};
use tembra::{
    Context, Host, Orb, Provider, Reference, Service,
    core::Code,
    portable::{Delivered, Owned},
};
use tembra_contracts::ModuleContract;
use tembra_executor_wasm::{Contract, Engine, Install, Limits};
use tembra_orb_parity_fixture::parity;

/// The interface the Orb imports plainly and by key, the view the macro derives for the import by
/// key, and the native Orb serving both.
const DIAL: &str = "example:dials/dial@1.0.0";
const DIAL_VIEW: &str = "example:parity/dial-ref@1.0.0";
const DIALS: &str = "dials";

/// What a scenario covers.
#[derive(Clone, Copy, Debug)]
enum Feature {
    /// The plain import and the views of the import by key, whose claims the call that drops a
    /// view, or the `get` that fails, releases.
    Imports,
    /// A call of an import that the method stops awaiting, or that its caller cancels.
    Cancellation,
    /// The JSON the kernel keeps for a state.
    StateJson,
    /// The WIT resources the Orb issues to its callers.
    WitResources,
    /// The code and reason of a call that fails.
    ErrorCodes,
    /// The resources of the Orb's root, opened after the resources they depend on.
    RootResources,
    /// The resources of a service's objects, opened for each object after the root's resources and
    /// the object's earlier ones they depend on.
    ServiceResources,
}

/// One step of a scenario. A step names a service by its interface, `dialing` for
/// `example:parity/dialing@1.0.0`.
#[derive(Clone)]
enum Step {
    /// Call a function of the `main` object of a service with the boundary's JSON arguments: it
    /// answers the result, whose resources are numbered in it, or the error. The scenario keeps the
    /// handles of the resources a result holds, in order, after those it kept before.
    Call(&'static str, &'static str, Value),
    /// Call a function through the handle the scenario kept at a position, as `Call` does.
    Method(usize, &'static str, Value),
    /// Drop the handle the scenario kept at a position, which drops the resource: it answers `null`.
    Drop(usize),
    /// Call a function of the `main` object of a service, and cancel the call once the provider's
    /// `hold` started: the result, or the error.
    Cancel(&'static str, &'static str, Value),
    /// The claims the kernel holds beyond those it held when the scenario's root began.
    Claims,
    /// The JSON the kernel keeps for the state of the `main` object of a service.
    State(&'static str),
    /// What the provider's `hold` saw since the step that last asked, or since the host started.
    Seen,
}

/// What a step answered, and the address of each object of the provider its calls reached, in
/// order, with the provider's function each call ran.
#[derive(Debug)]
struct Answer {
    outcome: Outcome,
    reached: Vec<String>,
}

/// What a step's call or reading gave.
#[derive(Debug)]
enum Outcome {
    /// The boundary's JSON.
    Value(Value),
    /// An error.
    Error(tembra::Error),
    /// What the step names and the build lacks: a handle it did not deliver, or a service it does
    /// not declare. Only the report reads it, through `Debug`.
    Missing(#[allow(dead_code)] String),
}

impl Answer {
    /// Whether two builds answered alike: the same objects reached, and the same value or errors of
    /// one code and reason. The message of an error is the native codec's or the executor's own
    /// wording. A build that lacks what the step names never agrees.
    fn agrees(&self, other: &Answer) -> bool {
        self.reached == other.reached
            && match (&self.outcome, &other.outcome) {
                (Outcome::Value(one), Outcome::Value(other)) => one == other,
                (Outcome::Error(one), Outcome::Error(other)) => {
                    one.code == other.code && one.reason == other.reason
                }
                _ => false,
            }
    }
}

impl From<tembra::Result<Value>> for Outcome {
    fn from(result: tembra::Result<Value>) -> Self {
        match result {
            Ok(value) => Outcome::Value(value),
            Err(error) => Outcome::Error(error),
        }
    }
}

/// A scenario: what it checks, the feature it covers, its steps, which run in order in one root of
/// `alice` on each host, and the difference the builds have in it today, if any.
struct Scenario {
    feature: Feature,
    name: &'static str,
    steps: Vec<Step>,
    differs: Option<Difference>,
}

impl Scenario {
    /// Mark a difference the builds have today: where a step differs, the native build fails with
    /// the code and reason `native` and the component with `component`, for the reason `why`.
    /// No scenario differs today; a difference found later marks its row with it.
    #[allow(dead_code)]
    fn differs(
        self,
        native: (Code, &'static str),
        component: (Code, &'static str),
        why: &'static str,
    ) -> Self {
        Self {
            differs: Some(Difference {
                native,
                component,
                why,
            }),
            ..self
        }
    }
}

/// A difference the builds have today, which a fix removes with its mark: the code and reason of
/// each build's error, and why they differ.
#[derive(Clone, Copy)]
struct Difference {
    native: (Code, &'static str),
    component: (Code, &'static str),
    why: &'static str,
}

impl Difference {
    /// Whether the two answers of a step differ as marked: they reached the same objects, and
    /// failed with the codes and reasons of the mark.
    fn explains(&self, native: &Answer, component: &Answer) -> bool {
        native.reached == component.reached
            && failure(native) == Some(self.native)
            && failure(component) == Some(self.component)
    }
}

/// The code and reason of an answer that is an error.
fn failure(answer: &Answer) -> Option<(Code, &str)> {
    match &answer.outcome {
        Outcome::Error(error) => Some((error.code, error.reason.as_str())),
        Outcome::Value(_) | Outcome::Missing(_) => None,
    }
}

/// Every scenario, by feature. A new one is a module of the fixture, or a function of one, and a
/// row here.
fn scenarios() -> Vec<Scenario> {
    use Feature::*;
    use Step::*;
    let row = |feature, name, steps| Scenario {
        feature,
        name,
        steps,
        differs: None,
    };
    vec![
        row(
            Imports,
            "the plain import reaches the provider's main dial, which keeps its position from call to call",
            vec![
                Call("dialing", "turn-main", json!([[2, 3]])),
                Call("dialing", "turn-main", json!([[4]])),
                Claims,
            ],
        ),
        row(
            Imports,
            "two views of one key reach one dial, another key's dial turns on its own, and the call releases the claims of the views it drops",
            vec![Call("dialing", "turn-both", json!(["a", "b", 2])), Claims],
        ),
        row(
            Imports,
            "a view dropped in a call leaves no claim, so the next get of its key in the same root finds a fresh dial",
            vec![
                Call("dialing", "turn-at", json!(["a", 3])),
                Claims,
                Call("dialing", "turn-at", json!(["a", 4])),
                Claims,
            ],
        ),
        row(
            Imports,
            "a get of a key the provider refuses answers why, and leaves no claim",
            vec![Call("dialing", "turn-at", json!(["refused", 1])), Claims],
        ),
        row(
            Imports,
            "a get of a dial that fails to open answers why, and leaves no claim",
            vec![Call("dialing", "turn-at", json!(["closed", 1])), Claims],
        ),
        row(
            Cancellation,
            "a call of an import the method stops awaiting is cancelled for that call, and the method answers once it has ended",
            vec![Call("glancing", "glance", json!([])), Seen, Claims],
        ),
        row(
            Cancellation,
            "a call its caller cancels cancels the call of the import it awaits",
            vec![Cancel("glancing", "wait", json!([])), Seen, Claims],
        ),
        row(
            StateJson,
            "a state starts from its default and keeps the JSON serde writes for each value",
            vec![
                State("ledger"),
                Call("ledger", "open", json!([])),
                State("ledger"),
                Call(
                    "ledger",
                    "deposit",
                    json!(["rent", 9_007_199_254_740_993_u64]),
                ),
                Call("ledger", "deposit", json!(["food", 7])),
                Call("ledger", "transfer", json!(["bob", 5])),
                Call("ledger", "transfer", json!(["carol", 1_u64 << 60])),
                Call(
                    "ledger",
                    "mark",
                    json!([{ "audited": true, "disputed": false }]),
                ),
                State("ledger"),
                Call("ledger", "kept", json!([])),
            ],
        ),
        row(
            StateJson,
            "a state holding an enum serde tags inside its own object, beside the author's derives, reads back the JSON serde writes for it",
            vec![
                State("sketching"),
                Call("sketching", "circle", json!([3])),
                State("sketching"),
                Call("sketching", "square", json!([4])),
                State("sketching"),
            ],
        ),
        row(
            WitResources,
            "a constructor issues a tally its caller owns, whose methods read it through the handle, and dropping the handle drops it",
            vec![
                Call("tallies", "dropped", json!([])),
                Call("tallies", "[constructor]tally", json!([5])),
                Method(0, "[method]tally.add", json!([3])),
                Method(0, "[method]tally.total", json!([])),
                Drop(0),
                Call("tallies", "dropped", json!([])),
            ],
        ),
        row(
            WitResources,
            "a service in a private module issues tokens of a module below it, whose constructor counts in the state of another module below it",
            vec![
                State("vault"),
                Call("vault", "[constructor]token", json!([3])),
                Method(0, "[method]token.number", json!([])),
                Call("vault", "open", json!([4])),
                Method(1, "[method]token.number", json!([])),
                Call("vault", "issued", json!([])),
                State("vault"),
                Drop(0),
                Drop(1),
            ],
        ),
        row(
            WitResources,
            "a function returns tallies nested in a list, an option and a tuple, each handle naming its own tally",
            vec![
                Call("tallies", "dropped", json!([])),
                Call("tallies", "several", json!([[1, 2]])),
                Method(1, "[method]tally.add", json!([10])),
                Method(0, "[method]tally.total", json!([])),
                Method(2, "[method]tally.total", json!([])),
                Drop(0),
                Drop(1),
                Drop(2),
                Call("tallies", "dropped", json!([])),
                Call("tallies", "several", json!([[]])),
            ],
        ),
        row(
            WitResources,
            "a static function and a result issue tallies too, and a refused result issues none",
            vec![
                Call("tallies", "dropped", json!([])),
                Call("tallies", "[static]tally.summed", json!([[4, 5]])),
                Method(0, "[method]tally.total", json!([])),
                Call("tallies", "even", json!([3])),
                Call("tallies", "even", json!([4])),
                Method(1, "[method]tally.add", json!([1])),
                Drop(1),
                Drop(0),
                Call("tallies", "dropped", json!([])),
            ],
        ),
        row(
            WitResources,
            "a method of a tally needs the tally's handle, and a function of the service takes none",
            vec![
                Call("tallies", "[method]tally.add", json!([1])),
                Call("tallies", "start", json!([2])),
                Method(0, "start", json!([1])),
                Method(0, "[method]tally.total", json!([])),
                Drop(0),
            ],
        ),
        row(
            ErrorCodes,
            "each parameter reads a value of its kind, and a function's own error is the value of its result",
            vec![
                Call("checks", "add", json!([200, 100])),
                Call("checks", "ratio", json!([1, 4])),
                Call("checks", "code", json!(["é"])),
                Call("checks", "shift", json!([{ "x": 1, "y": -2 }, 3])),
                Call("checks", "named", json!(["green"])),
                Call("checks", "area", json!([{ "tag": "square", "val": 3 }])),
                Call("checks", "area", json!([{ "tag": "dot" }])),
                Call("checks", "flipped", json!([{ "on": true, "loud": false }])),
                Call("checks", "or-zero", json!([null])),
                Call("checks", "or-zero", json!([18_446_744_073_709_551_615_u64])),
                Call("checks", "halve", json!([4])),
                Call("checks", "halve", json!([3])),
            ],
        ),
        row(
            ErrorCodes,
            "a type named through a renaming `use` or an alias reads and answers as the type itself",
            vec![
                Call("names", "mirrored", json!([{ "x": 1, "y": -2 }])),
                Call("names", "red", json!(["red"])),
                Call("names", "red", json!(["green"])),
                Call("names", "red", json!(["purple"])),
            ],
        ),
        row(
            RootResources,
            "a resource opens after the resource it depends on, whose value its opening reads",
            vec![
                Call("stock", "add", json!([2])),
                Call("stock", "add", json!([4])),
            ],
        ),
        row(
            ServiceResources,
            "an object's resources open after the root's and the object's earlier ones, whose values their openings read, and keep their values from call to call",
            vec![
                Call("ship", "berth", json!([])),
                Call("ship", "tie", json!([3])),
                Call("ship", "tie", json!([2])),
            ],
        ),
        row(
            ErrorCodes,
            "a call of a function the interface does not have fails",
            vec![Call("checks", "subtract", json!([2, 1]))],
        ),
        row(
            ErrorCodes,
            "arguments that do not read as the parameters fail the call before its method runs",
            vec![
                Call("checks", "add", json!(["one", 2])),
                Call("checks", "add", json!([256, 1])),
                Call("checks", "add", json!([-1, 1])),
                Call("checks", "add", json!([1.5, 1])),
                Call("checks", "add", json!([1])),
                Call("checks", "add", json!([1, 2, 3])),
                Call("checks", "add", json!(null)),
                Call("checks", "add", json!({ "a": 1, "b": 2 })),
                Call("checks", "ratio", json!([1e39, 1])),
                Call("checks", "code", json!(["ab"])),
                Call("checks", "code", json!([""])),
                Call("checks", "shift", json!([{ "x": 1 }, 3])),
                Call("checks", "shift", json!([{ "x": 1, "y": 2, "z": 3 }, 3])),
                Call("checks", "named", json!(["purple"])),
                Call("checks", "area", json!([{ "tag": "circle", "val": 1 }])),
                Call("checks", "area", json!([{ "tag": "square" }])),
                Call(
                    "checks",
                    "flipped",
                    json!([{ "on": true, "loud": false, "dim": true }]),
                ),
                Call("checks", "or-zero", json!([18_446_744_073_709_551_616_f64])),
            ],
        ),
        row(
            ErrorCodes,
            "arguments of a resource's method that do not read as its parameters fail the call before the method runs",
            vec![
                Call("tallies", "[constructor]tally", json!([5])),
                Method(0, "[method]tally.add", json!(["one"])),
                Method(0, "[method]tally.add", json!([-1])),
                Method(0, "[method]tally.add", json!([])),
                Method(0, "[method]tally.add", json!([1, 2])),
                Method(0, "[method]tally.add", json!(null)),
                Method(0, "[method]tally.total", json!([1])),
                Method(0, "[method]tally.total", json!([])),
                Drop(0),
            ],
        ),
        row(
            ErrorCodes,
            "a result the boundary cannot carry fails the call after its method ran",
            vec![
                Call("checks", "ratio", json!([1, 0])),
                Call("checks", "ratio", json!([0, 0])),
            ],
        ),
    ]
}

/// What the provider saw, in order, on the host's thread: the object each call reached, which every
/// step reports, and when each call of `hold` started and why it ended, which `Step::Seen` reads.
#[derive(Clone, Default)]
struct Watch {
    reached: Rc<RefCell<Vec<String>>>,
    held: Rc<RefCell<Vec<String>>>,
}

impl Watch {
    /// Note that a call of `function` reached the object running it, by the object's address.
    fn reach(&self, function: &str, context: &Context) -> tembra::Result<()> {
        let address = context.address()?;
        self.reached.borrow_mut().push(format!(
            "{function} on {} [{}] of {}",
            address.service,
            address.key.join("/"),
            address.principal
        ));
        Ok(())
    }
}

/// One dial of the provider: where it stands.
struct Dial {
    position: Cell<u64>,
}

/// The native provider of the dials, from the package the fixture consumes: each object is a dial
/// of its own, the key `refused` is refused, and the dial of the key `closed` fails to open. Each
/// call tells `watch` the object it reached, and `hold`, which answers once its call is cancelled,
/// tells it too when it starts and why it ended.
fn dials(watch: Watch) -> Orb {
    let wit = format!(
        "{}\nworld provider {{ export dial; }}\n",
        include_str!("../../orb-macro/parity-fixture/consumed/dials.wit")
    );
    let service = Contract::from_source("dials.wit", &wit, Some("provider"))
        .unwrap()
        .plan(DIALS, 1, &BTreeMap::new())
        .unwrap()
        .orb
        .services
        .remove(0);
    let (turned, read) = (watch.clone(), watch.clone());
    Orb::new(DIALS, 1).service(
        Service::new(service.name, |declaration| {
            let key = declaration.address()?.key;
            if key == ["refused"] {
                return Err(tembra::Error::new(Code::Forbidden, "key", "unapproved key"));
            }
            if key == ["closed"] {
                declaration.resource(
                    "dial",
                    1,
                    || Err::<(), _>(tembra::Error::invalid("key", "no dial is open there")),
                    |_, _| async { Ok(()) }.boxed_local(),
                )?;
            }
            Ok(Dial {
                position: Cell::new(0),
            })
        })
        .signature(service.signature)
        .method("turn", move |dial, context: Context, (by,): (u32,)| {
            let reached = turned.reach("turn", &context);
            async move {
                reached?;
                dial.position.set(dial.position.get() + u64::from(by));
                Ok(dial.position.get())
            }
        })
        .method("position", move |dial, context: Context, _: [Value; 0]| {
            let reached = read.reach("position", &context);
            async move {
                reached?;
                Ok(dial.position.get())
            }
        })
        .method("hold", move |dial, context: Context, _: [Value; 0]| {
            let reached = watch.reach("hold", &context);
            let held = watch.held.clone();
            async move {
                reached?;
                held.borrow_mut().push("hold started".to_owned());
                let signal = context.signal();
                signal.cancelled().await;
                let reason = signal.check().unwrap_err().reason;
                held.borrow_mut().push(format!("hold cancelled: {reason}"));
                Ok(dial.position.get())
            }
        }),
    )
}

/// The name of an interface, by its identity: `dialing` for `example:parity/dialing@1.0.0`.
fn interface(identity: &str) -> &str {
    let (_, interface) = identity
        .rsplit_once('/')
        .expect("a service is named after its interface");
    interface
        .split_once('@')
        .map_or(interface, |(name, _)| name)
}

/// The outcome of a call, keeping the handles of the resources its result holds, in order, after
/// those kept before.
fn kept(result: tembra::Result<Delivered>, handles: &mut Vec<Option<Owned>>) -> Outcome {
    result
        .map(|delivered| {
            handles.extend(delivered.owned.into_iter().map(Some));
            delivered.value
        })
        .into()
}

/// Wait until `ready` holds, polling again at once: a component's call runs on the thread of its
/// Store, which wakes the host's pool only once it reaches the host.
async fn until(ready: impl Fn() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(20);
    std::future::poll_fn(|cx| {
        if ready() {
            return Poll::Ready(());
        }
        assert!(Instant::now() < deadline, "the provider never saw the call");
        cx.waker().wake_by_ref();
        Poll::Pending
    })
    .await
}

/// A host running one build of the Orb beside the provider, with a reference to the `main` object
/// of each service the build declares, of `alice`, by the identity of the service's interface,
/// `example:parity/dialing@1.0.0`, and what the provider saw.
struct Build {
    name: &'static str,
    pool: LocalPool,
    host: Host,
    watch: Watch,
    services: BTreeMap<String, Rc<Reference>>,
}

impl Build {
    /// Install `orb` beside the provider in a host of its own.
    fn new(name: &'static str, orb: Orb) -> Self {
        let mut pool = LocalPool::new();
        let host = Host::new(pool.spawner());
        let watch = Watch::default();
        let declared: Vec<String> = orb
            .declaration()
            .services
            .iter()
            .map(|service| service.name.clone())
            .collect();
        host.install(vec![dials(watch.clone()), orb]).unwrap();
        let mut services = BTreeMap::new();
        for service in declared {
            let reference = Rc::new(host.get(&service, vec!["main".into()], "alice").unwrap());
            pool.run_until(reference.ready()).unwrap();
            // The name is `<orb>::<identity>`, and the test gives both builds one Orb name.
            let identity = service
                .split_once("::")
                .map_or(service.as_str(), |(_, identity)| identity);
            services.insert(identity.to_owned(), reference);
        }
        Self {
            name,
            pool,
            host,
            watch,
            services,
        }
    }

    /// Run the steps in order in one root of `alice`, and return what each answered. A step names
    /// what the build may lack, a handle or a service: it then answers what is missing, and the
    /// scenario goes on.
    fn run(&mut self, steps: Vec<Step>) -> Vec<Answer> {
        let kernel = self.host.kernel();
        let services = self.services.clone();
        let watch = self.watch.clone();
        self.pool
            .run_until(self.host.root("alice", move |context| {
                async move {
                    let service = |wanted: &str| {
                        services
                            .iter()
                            .find(|(identity, _)| interface(identity) == wanted)
                            .map(|(_, reference)| Rc::clone(reference))
                            .ok_or_else(|| {
                                Outcome::Missing(format!("no service of the interface `{wanted}`"))
                            })
                    };
                    let no_handle = |handle| Outcome::Missing(format!("no handle at {handle}"));
                    let claims = kernel.diagnostics().references;
                    let mut handles: Vec<Option<Owned>> = Vec::new();
                    let mut answers = Vec::new();
                    for step in steps {
                        let outcome = match step {
                            Step::Call(wanted, function, arguments) => match service(wanted) {
                                Ok(reference) => {
                                    let result = reference
                                        .call_portable::<_, Delivered>(
                                            &context, function, arguments,
                                        )
                                        .await;
                                    kept(result, &mut handles)
                                }
                                Err(missing) => missing,
                            },
                            Step::Method(handle, function, arguments) => {
                                let result = match handles.get(handle).and_then(Option::as_ref) {
                                    Some(owned) => Some(
                                        owned
                                            .call_portable::<_, Delivered>(
                                                &context, function, arguments,
                                            )
                                            .await,
                                    ),
                                    None => None,
                                };
                                match result {
                                    Some(result) => kept(result, &mut handles),
                                    None => no_handle(handle),
                                }
                            }
                            Step::Drop(handle) => {
                                match handles.get_mut(handle).and_then(Option::take) {
                                    Some(owned) => {
                                        drop(owned);
                                        Outcome::Value(Value::Null)
                                    }
                                    None => no_handle(handle),
                                }
                            }
                            Step::Cancel(wanted, function, arguments) => match service(wanted) {
                                Ok(reference) => {
                                    let saw = watch.held.borrow().len();
                                    let result = match reference
                                        .issue_portable::<_, Delivered>(
                                            &context, function, arguments,
                                        )
                                        .await
                                    {
                                        Ok((issued, result)) => {
                                            until(|| watch.held.borrow().len() > saw).await;
                                            issued.cancel();
                                            result.await
                                        }
                                        Err(error) => Err(error),
                                    };
                                    kept(result, &mut handles)
                                }
                                Err(missing) => missing,
                            },
                            Step::Claims => {
                                let held = kernel.diagnostics().references as i64 - claims as i64;
                                Outcome::Value(json!(held))
                            }
                            Step::State(wanted) => match service(wanted) {
                                Ok(reference) => reference
                                    .state::<Value>()
                                    .and_then(|state| state.value())
                                    .map(|value| (*value).clone())
                                    .into(),
                                Err(missing) => missing,
                            },
                            Step::Seen => Outcome::Value(json!(watch.held.take())),
                        };
                        answers.push(Answer {
                            outcome,
                            reached: watch.reached.take(),
                        });
                    }
                    Ok(answers)
                }
                .boxed_local()
            }))
            .unwrap()
    }

    /// Release the references and close the host, which must report no error.
    fn close(mut self) {
        for reference in self.services.values() {
            reference.release().unwrap();
        }
        self.pool.run_until(self.host.close()).unwrap();
        assert!(
            self.host.errors().is_empty(),
            "{}: {:?}",
            self.name,
            self.host.errors()
        );
    }
}

/// The component of the fixture, packaged as `cargo tembra build` packages it: the contract is the
/// WIT the macro embedded, with the consumed package nested whole, and the artifact keeps the
/// roles the component carries, the view of the import by key among them.
fn component() -> Vec<u8> {
    let path = std::env::var_os("TEMBRA_WASM_ORB_PARITY")
        .expect("TEMBRA_WASM_ORB_PARITY names the component of the parity fixture");
    let bytes = std::fs::read(path).expect("reading TEMBRA_WASM_ORB_PARITY");
    let consumed =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../orb-macro/parity-fixture/consumed");
    let module = ModuleContract::from_component(&bytes, Some(&consumed))
        .unwrap()
        .expect("the component carries the WIT the macro derived");
    let contract = module.contract();
    let roles = contract.embedded_roles(&bytes).unwrap();
    contract
        .component_artifact(&bytes, "parity", 1, &roles)
        .unwrap()
}

/// The native build and the component of the parity fixture declare the services of the same
/// identities and answer every scenario alike: each step reaches the same objects of the provider,
/// by service, key and principal, and answers the same value, or an error of the same code and
/// reason. The native side of the macro keeps the guarantees a component gives for imports and
/// views, cancellation, states, WIT resources and failed calls. In a scenario marked with a
/// difference the builds have today, a step may differ only as marked, and at least one must: the
/// fix that makes them agree removes the mark. Every difference is reported, a handle or a service
/// one build lacks included, before the test fails.
#[test]
#[ignore = "requires TEMBRA_WASM_ORB_PARITY"]
fn the_native_build_and_the_component_answer_alike() {
    let artifact = component();
    let engine = Engine::new(Limits::default()).unwrap();
    let providers = parity::Providers {
        dial: Provider::new(DIALS, 1),
        dial_ref: Provider::new(DIALS, 1),
    };
    let mut native = Build::new("native", parity::orb(providers).unwrap());
    let install = Install::default()
        .requires(DIAL, DIALS, 1)
        .requires(DIAL_VIEW, DIALS, 1);
    let mut compiled = Build::new("component", engine.load(&artifact, install).unwrap());
    let mut failures = Vec::new();
    let declared = |build: &Build| build.services.keys().cloned().collect::<Vec<_>>();
    if declared(&native) != declared(&compiled) {
        failures.push(format!(
            "the builds declare different services\n  native {:?}\n  component {:?}",
            declared(&native),
            declared(&compiled)
        ));
    }
    for scenario in scenarios() {
        let natively = native.run(scenario.steps.clone());
        let as_component = compiled.run(scenario.steps.clone());
        let mut marked = 0;
        let mut differences = Vec::new();
        for (step, (native, component)) in natively.iter().zip(&as_component).enumerate() {
            if native.agrees(component) {
                continue;
            }
            if let Some(difference) = scenario.differs
                && difference.explains(native, component)
            {
                marked += 1;
                continue;
            }
            differences.push(format!(
                "  step {step}: native {native:?}\n          component {component:?}"
            ));
        }
        let title = format!("{:?}: {}", scenario.feature, scenario.name);
        if !differences.is_empty() {
            failures.push(format!("{title}\n{}", differences.join("\n")));
        }
        match scenario.differs {
            Some(difference) if marked == 0 => failures.push(format!(
                "{title}\n  no step differs as marked now, so drop its mark: {}",
                difference.why
            )),
            Some(difference) => eprintln!("{title}\n  differs as marked: {}", difference.why),
            None => {}
        }
    }
    native.close();
    compiled.close();
    assert_eq!(engine.instances(), 0);
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

/// A state holding an enum `serde` tags inside its own object, `#[serde(tag = "kind")]`, with the
/// author's derives beside the attribute, keeps the JSON `serde` writes for it and reads it back:
/// natively here, and as a component in the parity scenario, which answers alike.
#[test]
fn a_state_with_an_internally_tagged_enum_round_trips() {
    let providers = parity::Providers {
        dial: Provider::new(DIALS, 1),
        dial_ref: Provider::new(DIALS, 1),
    };
    let mut native = Build::new("native", parity::orb(providers).unwrap());
    let answers: Vec<Value> = native
        .run(vec![
            Step::State("sketching"),
            Step::Call("sketching", "circle", json!([3])),
            Step::State("sketching"),
            Step::Call("sketching", "square", json!([4])),
            Step::State("sketching"),
        ])
        .into_iter()
        .map(|answer| match answer.outcome {
            Outcome::Value(value) => value,
            outcome => panic!("{outcome:?}"),
        })
        .collect();
    assert_eq!(
        answers,
        [
            json!({ "shape": { "kind": "Dot" }, "strokes": 0 }),
            json!(3),
            json!({ "shape": { "kind": "Circle", "radius": 3 }, "strokes": 1 }),
            json!(4),
            json!({ "shape": { "kind": "Square", "side": 4 }, "strokes": 2 }),
        ]
    );
    native.close();
}
