//! Explicit checkpoint/drain/restart is an application operation, not transparent rotation.
use futures::{FutureExt, channel::oneshot};
use serde_json::{Value, json};
use std::{cell::RefCell, collections::BTreeMap, rc::Rc};
use tembra_core::{contracts::*, *};
use tembra_executor_js::{BundledJsArtifact, Executor, ExecutorOptions};

fn method(args: Option<Value>, result: Option<Value>) -> Member {
    Member::Method {
        args: args.map(ValueSchema::Portable),
        result: result.map(ValueSchema::Portable),
    }
}

fn counter_contract() -> Rc<Contract> {
    Rc::new(
        Contract::local(
            "session.counter",
            1,
            json!({}),
            BTreeMap::from([
                (
                    "count".into(),
                    Member::State {
                        schema: json!({"type":"integer"}),
                    },
                ),
                ("read".into(), method(None, Some(json!({"type":"integer"})))),
                (
                    "add".into(),
                    method(
                        Some(json!({"type":"integer"})),
                        Some(json!({"type":"integer"})),
                    ),
                ),
                ("wait".into(), method(None, Some(json!({"type":"integer"})))),
                (
                    "origin".into(),
                    method(None, Some(json!({"type":"integer"}))),
                ),
                (
                    "evaluations".into(),
                    method(None, Some(json!({"type":"integer"}))),
                ),
            ]),
        )
        .unwrap(),
    )
}

fn gate_contract() -> Rc<Contract> {
    Rc::new(
        Contract::local(
            "session.gate",
            1,
            json!({}),
            BTreeMap::from([
                (
                    "record".into(),
                    method(Some(json!({"type":"string"})), None),
                ),
                ("wait".into(), method(None, Some(json!({"type":"integer"})))),
            ]),
        )
        .unwrap(),
    )
}

fn source(
    counter: &Contract,
    gate: &Contract,
    seed: &Value,
    session: u64,
    revision: u64,
) -> String {
    r#"
globalThis.sessionEvaluations=(globalThis.sessionEvaluations??0)+1;
const counter=$COUNTER,gate=$GATE;
class PrivateResource {
    #closed=false;
    constructor(origin){this.origin=origin;}
    close(){this.#closed=true;}
    isClosed(){return this.#closed;}
}
export default {name:'session.counter',uses:[gate],setup(scope,declare){
    declare.serve(counter,(_input,self)=>{
        const dependency=self.get(gate,{});
        const count=self.cell('count',{schema:{type:'integer'},initial:$SEED});
        const resource=self.resource('private',{
            version:1,deps:[dependency],
            async open(){
                await scope.root(ctx=>dependency.record('open:$SESSION:$REVISION',ctx));
                return new PrivateResource($SESSION);
            },
            async close(value){
                value.close();
                await scope.root(ctx=>dependency.record(
                    'close:$SESSION:$REVISION:origin'+value.origin,ctx));
            }
        });
        return {methods:{
            read:()=>count.value,
            add:(ctx,by)=>count.replace(ctx,count.value+by).value,
            origin:()=>resource.value.origin,
            evaluations:()=>globalThis.sessionEvaluations,
            async wait(ctx){
                const captured=resource.value;
                await dependency.wait(ctx);
                if(captured.isClosed())throw new Error('private resource closed before settlement');
                return count.value;
            }
        },state:{count}};
    },{lifetime:'host'});
}};
"#
    .replace(
        "$COUNTER",
        &serde_json::to_string(counter.descriptor()).unwrap(),
    )
    .replace("$GATE", &serde_json::to_string(gate.descriptor()).unwrap())
    .replace("$SEED", &seed.to_string())
    .replace("$SESSION", &session.to_string())
    .replace("$REVISION", &revision.to_string())
}

fn prepare(
    driver: &ManualDriver,
    host: &Host,
    executor: &Executor,
    source: String,
    name: &str,
) -> ExecutableFacet {
    driver
        .run_until(executor.prepare(
            host.id(),
            BundledJsArtifact::new(name, source, vec!["default".into()]).unwrap(),
            "default".into(),
        ))
        .unwrap()
}

fn install_gate(
    host: &Host,
    gate: Rc<Contract>,
    records: Rc<RefCell<Vec<String>>>,
    receiver: Option<oneshot::Receiver<()>>,
) {
    let receiver = Rc::new(RefCell::new(receiver));
    host.serve(
        gate,
        Rc::new(move |_, _, _| {
            let records = records.clone();
            let receiver = receiver.clone();
            Ok(Implementation::new()
                .json_method("record", move |value, _| {
                    records
                        .borrow_mut()
                        .push(value.unwrap().as_str().unwrap().to_owned());
                    async { Ok(None) }.boxed_local()
                })
                .json_method("wait", move |_, context| {
                    let receiver = receiver.borrow_mut().take().expect("one admitted wait");
                    async move {
                        // Deliberately ignore cancellation: physical completion needs this gate.
                        let _context = context;
                        receiver.await.unwrap();
                        Ok(Some(json!(0)))
                    }
                    .boxed_local()
                }))
        }),
        ServeOptions {
            lifetime: Lifetime::Host,
            ..Default::default()
        },
    )
    .unwrap();
}

fn assert_closed(executor: &Executor) {
    let stats = executor.stats();
    assert_eq!(stats.callbacks, 0);
    assert_eq!(stats.capabilities, 0);
    assert_eq!(stats.contexts, 0);
    assert_eq!(stats.queued_jobs, 0);
    assert_eq!(stats.physical_jobs, 0);
}

#[test]
fn explicit_checkpoint_drain_and_restart_reacquires_references_and_private_resources() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let _entered = runtime.enter();
    let driver = ManualDriver::new();
    let counter = counter_contract();
    let gate = gate_contract();
    let records = Rc::new(RefCell::new(Vec::new()));
    let checkpoint = {
        let host = Host::new(HostOptions::new("session", json!(null)), driver.clone());
        let (release, receiver) = oneshot::channel();
        install_gate(&host, gate.clone(), records.clone(), Some(receiver));
        let executor = Executor::new(
            driver.clone(),
            ExecutorOptions {
                max_evaluations: 2,
                ..Default::default()
            },
        )
        .unwrap();
        let first = prepare(
            &driver,
            &host,
            &executor,
            source(&counter, &gate, &json!(0), 1, 1),
            "session-one",
        );
        driver
            .run_until(
                host.install_executable(first.clone(), LoadOptions::default())
                    .unwrap(),
            )
            .unwrap();
        let reference = driver
            .run_until(host.get(counter.clone(), json!({}), GetOptions::default()))
            .unwrap();
        assert_eq!(
            driver
                .run_until(reference.call_json("add", Some(json!(5)), None))
                .unwrap(),
            Some(json!(5))
        );
        let second = prepare(
            &driver,
            &host,
            &executor,
            source(&counter, &gate, &json!(999), 1, 2),
            "session-two",
        );
        driver
            .run_until(host.reload(vec![second.clone()], ReloadOptions::default()))
            .unwrap();
        let definitions = vec![first, second];
        let readable = reference.readable("count").unwrap();
        // The embedding has stopped writers. The remaining wait below only reads this state;
        // applications with physical writers must drain them before taking a final checkpoint.
        let checkpoint = readable.snapshot().unwrap();
        assert_eq!(checkpoint.value(), &json!(5));
        assert_eq!(
            driver
                .run_until(reference.call_json("origin", None, None))
                .unwrap(),
            Some(json!(1))
        );
        let before_refusal = records.borrow().clone();
        let refused = driver
            .run_until(
                executor.prepare(
                    host.id(),
                    BundledJsArtifact::new(
                        "session-refused",
                        source(&counter, &gate, &json!(9999), 1, 3),
                        vec!["default".into()],
                    )
                    .unwrap(),
                    "default".into(),
                ),
            )
            .err()
            .expect("budget must refuse before evaluating the third module");
        assert_eq!(refused.code, ErrorCode::Unavailable);
        assert_eq!(refused.reason, "rotation_required");
        assert_eq!(executor.stats().evaluations, 2);
        assert_eq!(
            driver
                .run_until(reference.call_json("evaluations", None, None))
                .unwrap(),
            Some(json!(2))
        );
        assert_eq!(readable.snapshot().unwrap(), checkpoint);
        assert_eq!(*records.borrow(), before_refusal);

        let mut call = reference.call_json("wait", None, None).boxed_local();
        assert!(
            driver
                .run_until(async { call.as_mut().now_or_never() })
                .is_none()
        );
        driver.run_until_stalled();
        assert!(executor.stats().physical_jobs > 0);
        let mut close = host.close().boxed_local();
        assert!(
            driver
                .run_until(async { close.as_mut().now_or_never() })
                .is_none()
        );
        assert!(
            !records
                .borrow()
                .iter()
                .any(|value| value.starts_with("close:"))
        );
        assert!(executor.stats().physical_jobs > 0);
        release.send(()).unwrap();
        let (result, closed) = driver.run_until(futures::future::join(call, close));
        assert_eq!(result.unwrap(), Some(json!(5)));
        closed.unwrap();
        assert_eq!(reference.status(), RefStatus::Gone);
        assert_eq!(*records.borrow(), ["open:1:1", "close:1:2:origin1"]);
        let retained = executor.stats();
        // The two explicitly retained executable definitions still own their setup callbacks.
        assert_eq!(retained.callbacks, 2);
        assert_eq!(retained.capabilities, 0);
        assert_eq!(retained.contexts, 0);
        assert_eq!(retained.queued_jobs, 0);
        assert_eq!(retained.physical_jobs, 0);
        let portable = checkpoint.value().clone();
        drop(checkpoint);
        drop(readable);
        drop(reference);
        drop(executor);
        assert_eq!(
            Executor::new(driver.clone(), ExecutorOptions::default())
                .err()
                .expect("drained work does not release retained executable definitions")
                .reason,
            "engine_thread",
        );
        drop(definitions);
        drop(host);
        driver.run_until_stalled();
        portable
    };

    // Only portable application data crosses this explicit session boundary.
    let host = Host::new(HostOptions::new("session", json!(null)), driver.clone());
    install_gate(&host, gate.clone(), records.clone(), None);
    let executor = Executor::new(driver.clone(), ExecutorOptions::default()).unwrap();
    let replacement = prepare(
        &driver,
        &host,
        &executor,
        source(&counter, &gate, &checkpoint, 2, 1),
        "new-session",
    );
    driver
        .run_until(
            host.install_executable(replacement, LoadOptions::default())
                .unwrap(),
        )
        .unwrap();
    let reacquired = driver
        .run_until(host.get(counter, json!({}), GetOptions::default()))
        .unwrap();
    assert_eq!(
        driver
            .run_until(reacquired.call_json("read", None, None))
            .unwrap(),
        Some(json!(5))
    );
    assert_eq!(
        driver
            .run_until(reacquired.call_json("origin", None, None))
            .unwrap(),
        Some(json!(2))
    );
    assert_eq!(
        driver
            .run_until(reacquired.call_json("evaluations", None, None))
            .unwrap(),
        Some(json!(1))
    );
    assert_eq!(
        reacquired
            .readable("count")
            .unwrap()
            .snapshot()
            .unwrap()
            .value(),
        &checkpoint
    );
    driver.run_until(host.close()).unwrap();
    assert_eq!(
        *records.borrow(),
        [
            "open:1:1",
            "close:1:2:origin1",
            "open:2:1",
            "close:2:1:origin2"
        ]
    );
    assert_closed(&executor);
    drop(reacquired);
    drop(host);
    drop(executor);
    driver.run_until_stalled();
}
