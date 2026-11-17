//! One `#[tembra::orb]` module answers alike as a native Orb and as a component. The parity
//! fixture's Orb builds both ways: `parity::orb(..)` natively, and the component that
//! `just build-orb-parity` builds from the same crate, which `just test-orb-parity` and acceptance
//! pass in `TEMBRA_WASM_ORB_PARITY`. The test installs each build in a host of its own, beside the
//! same native provider of the dials the Orb imports, runs every scenario of `scenarios()` on both,
//! each in a root of its own, and compares what each step answers: a call's value, or its error's
//! code and reason, the claims the kernel holds, a state's JSON, or what the provider saw. Each
//! build declares its services, which the steps name by their interface: a new scenario is a
//! module of the fixture, or a function of one, and a row of the list.
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
    /// Call a function of the `main` object of a service, and cancel the call once the provider saw
    /// the call it made: the result, or the error.
    Cancel(&'static str, &'static str, Value),
    /// The claims the kernel holds beyond those it held when the scenario's root began.
    Claims,
    /// The JSON the kernel keeps for the state of the `main` object of a service.
    State(&'static str),
    /// What the provider saw since the step that last asked, or since the host started.
    Seen,
}

/// What a step answered: the boundary's JSON, or an error.
#[derive(Debug)]
enum Answer {
    Value(Value),
    Error(tembra::Error),
}

impl Answer {
    /// Whether two builds answered alike: the same value, or errors of one code and reason. The
    /// message of an error is the native codec's or the executor's own wording.
    fn agrees(&self, other: &Answer) -> bool {
        match (self, other) {
            (Answer::Value(one), Answer::Value(other)) => one == other,
            (Answer::Error(one), Answer::Error(other)) => {
                one.code == other.code && one.reason == other.reason
            }
            _ => false,
        }
    }
}

impl From<tembra::Result<Value>> for Answer {
    fn from(result: tembra::Result<Value>) -> Self {
        match result {
            Ok(value) => Answer::Value(value),
            Err(error) => Answer::Error(error),
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
    /// Whether the two answers of a step differ as marked.
    fn explains(&self, native: &Answer, component: &Answer) -> bool {
        failure(native) == Some(self.native) && failure(component) == Some(self.component)
    }
}

/// The code and reason of an answer that is an error.
fn failure(answer: &Answer) -> Option<(Code, &str)> {
    match answer {
        Answer::Error(error) => Some((error.code, error.reason.as_str())),
        Answer::Value(_) => None,
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
                Call("ledger", "deposit", json!(["rent", 9_007_199_254_740_993_u64])),
                Call("ledger", "deposit", json!(["food", 7])),
                Call("ledger", "transfer", json!(["bob", 5])),
                Call("ledger", "transfer", json!(["carol", 1_u64 << 60])),
                Call("ledger", "mark", json!([{ "audited": true }])),
                State("ledger"),
                Call("ledger", "kept", json!([])),
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
                Call("checks", "flipped", json!([{ "on": true }])),
                Call("checks", "or-zero", json!([null])),
                Call("checks", "or-zero", json!([18_446_744_073_709_551_615_u64])),
                Call("checks", "halve", json!([4])),
                Call("checks", "halve", json!([3])),
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
                Call("checks", "flipped", json!([{ "on": true, "dim": true }])),
                Call("checks", "or-zero", json!([18_446_744_073_709_551_616_f64])),
            ],
        )
        .differs(
            (Code::Invalid, "arguments"),
            (Code::Unavailable, "wasm_executor"),
            "fails on main: a component answers unavailable/wasm_executor where the native build answers invalid/arguments; fixed in #10",
        ),
        row(
            ErrorCodes,
            "a result the boundary cannot carry fails the call after its method ran",
            vec![
                Call("checks", "ratio", json!([1, 0])),
                Call("checks", "ratio", json!([0, 0])),
            ],
        )
        .differs(
            (Code::Invalid, "result"),
            (Code::Unavailable, "wasm_executor"),
            "fails on main: a component answers unavailable/wasm_executor where the native build answers invalid/result; fixed in #10",
        ),
    ]
}

/// What the provider saw, in order.
type Seen = Rc<RefCell<Vec<String>>>;

/// One dial of the provider: where it stands.
struct Dial {
    position: Cell<u64>,
}

/// The native provider of the dials, from the package the fixture consumes: each object is a dial
/// of its own, the key `refused` is refused, and the dial of the key `closed` fails to open. Its
/// `hold` answers once its call is cancelled, and tells `seen` when it starts and why it ended.
fn dials(seen: Seen) -> Orb {
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
        .method("turn", |dial, _, (by,): (u32,)| async move {
            dial.position.set(dial.position.get() + u64::from(by));
            Ok(dial.position.get())
        })
        .method("position", |dial, _, _: [Value; 0]| async move {
            Ok(dial.position.get())
        })
        .method("hold", move |dial, context: Context, _: [Value; 0]| {
            let seen = seen.clone();
            async move {
                seen.borrow_mut().push("hold started".to_owned());
                let signal = context.signal();
                signal.cancelled().await;
                let reason = signal.check().unwrap_err().reason;
                seen.borrow_mut().push(format!("hold cancelled: {reason}"));
                Ok(dial.position.get())
            }
        }),
    )
}

/// The interface a service implements, by its name: `dialing` for the service
/// `parity::example:parity/dialing@1.0.0`.
fn interface(service: &str) -> &str {
    let (_, interface) = service
        .rsplit_once('/')
        .expect("a service is named after its interface");
    interface
        .split_once('@')
        .map_or(interface, |(name, _)| name)
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
/// of each service the build declares, of `alice`, by the service's interface, and what the
/// provider saw.
struct Build {
    name: &'static str,
    pool: LocalPool,
    host: Host,
    seen: Seen,
    services: BTreeMap<String, Rc<Reference>>,
}

impl Build {
    /// Install `orb` beside the provider in a host of its own.
    fn new(name: &'static str, orb: Orb) -> Self {
        let mut pool = LocalPool::new();
        let host = Host::new(pool.spawner());
        let seen = Seen::default();
        let declared: Vec<String> = orb
            .declaration()
            .services
            .iter()
            .map(|service| service.name.clone())
            .collect();
        host.install(vec![dials(seen.clone()), orb]).unwrap();
        let mut services = BTreeMap::new();
        for service in declared {
            let reference = Rc::new(host.get(&service, vec!["main".into()], "alice").unwrap());
            pool.run_until(reference.ready()).unwrap();
            services.insert(interface(&service).to_owned(), reference);
        }
        Self {
            name,
            pool,
            host,
            seen,
            services,
        }
    }

    /// Run the steps in order in one root of `alice`, and return what each answered.
    fn run(&mut self, steps: Vec<Step>) -> Vec<Answer> {
        let kernel = self.host.kernel();
        let services = self.services.clone();
        let seen = self.seen.clone();
        self.pool
            .run_until(self.host.root("alice", move |context| {
                async move {
                    let service = |interface: &str| {
                        services.get(interface).unwrap_or_else(|| {
                            panic!("the Orb declares no service of the interface `{interface}`")
                        })
                    };
                    let claims = kernel.diagnostics().references;
                    let mut handles: Vec<Option<Owned>> = Vec::new();
                    let mut answers = Vec::new();
                    for step in steps {
                        let delivered = match step {
                            Step::Call(interface, function, arguments) => {
                                service(interface)
                                    .call_portable::<_, Delivered>(&context, function, arguments)
                                    .await
                            }
                            Step::Method(handle, function, arguments) => {
                                handles[handle]
                                    .as_ref()
                                    .expect("the scenario holds the handle")
                                    .call_portable::<_, Delivered>(&context, function, arguments)
                                    .await
                            }
                            Step::Cancel(interface, function, arguments) => {
                                let saw = seen.borrow().len();
                                match service(interface)
                                    .issue_portable::<_, Delivered>(&context, function, arguments)
                                    .await
                                {
                                    Ok((issued, result)) => {
                                        until(|| seen.borrow().len() > saw).await;
                                        issued.cancel();
                                        result.await
                                    }
                                    Err(error) => Err(error),
                                }
                            }
                            Step::Drop(handle) => {
                                handles[handle]
                                    .take()
                                    .expect("the scenario holds the handle");
                                answers.push(Answer::Value(Value::Null));
                                continue;
                            }
                            Step::Claims => {
                                let held = kernel.diagnostics().references as i64 - claims as i64;
                                answers.push(Answer::Value(json!(held)));
                                continue;
                            }
                            Step::State(interface) => {
                                let state = service(interface)
                                    .state::<Value>()
                                    .and_then(|state| state.value())
                                    .map(|value| (*value).clone());
                                answers.push(state.into());
                                continue;
                            }
                            Step::Seen => {
                                answers.push(Answer::Value(json!(seen.take())));
                                continue;
                            }
                        };
                        let value = delivered.map(|delivered| {
                            handles.extend(delivered.owned.into_iter().map(Some));
                            delivered.value
                        });
                        answers.push(value.into());
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

/// The native build and the component of the parity fixture declare the same services and answer
/// every scenario alike, each step with the same value, or an error of the same code and reason:
/// the native side of the macro keeps the guarantees a component gives for imports and views,
/// cancellation, states, WIT resources and failed calls. In a scenario marked with a difference
/// the builds have today, a step may differ only as marked, and at least one must: the fix that
/// makes them agree removes the mark.
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
    assert_eq!(
        native.services.keys().collect::<Vec<_>>(),
        compiled.services.keys().collect::<Vec<_>>(),
        "the two builds declare different services"
    );
    let mut failures = Vec::new();
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
