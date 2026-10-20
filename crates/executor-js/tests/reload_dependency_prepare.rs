//! New foreign dependencies on unchanged providers remain usable during actual preparation.
use futures::FutureExt;
use serde_json::json;
use std::{cell::RefCell, collections::BTreeMap, rc::Rc, time::Duration};
use tembra_core::{
    contracts::{Contract, Member, ValueSchema},
    *,
};
use tembra_executor_js::*;

#[derive(Clone, Copy, Debug)]
enum DependencyUse {
    Resource,
    Derived,
    Initializer,
}

fn contract(name: &str, state: bool) -> Rc<Contract> {
    let mut members = BTreeMap::from([(
        "read".into(),
        Member::Method {
            args: None,
            result: Some(ValueSchema::Portable(json!({"type":"integer"}))),
        },
    )]);
    if state {
        members.insert(
            "count".into(),
            Member::State {
                schema: json!({"type":"integer"}),
            },
        );
    }
    Rc::new(Contract::local(name, 1, json!({}), members).unwrap())
}

fn source(consumer: &Contract, provider: &Contract, usage: Option<DependencyUse>) -> String {
    let (declaration, read) = match usage {
        None => ("".to_owned(), "1"),
        Some(DependencyUse::Resource) => (
            "const q=self.get(provider,{});\n\
             const value=self.resource('from-q',{deps:[q],\n\
                 open:()=>scope.root(ctx=>q.read(ctx)),close(){}});"
                .into(),
            "value.value+1",
        ),
        Some(DependencyUse::Derived) => (
            "const q=self.get(provider,{});\n\
             const value=self.derive([q.count],{type:'integer'},([s])=>s.value+1);"
                .into(),
            "value.value",
        ),
        Some(DependencyUse::Initializer) => (
            "const q=self.get(provider,{});\n\
             const value=self.cell('from-q',{schema:{type:'integer'},\n\
                 initialize:async ctx=>1+await q.read(ctx)});"
                .into(),
            "value.value",
        ),
    };
    format!(
        r#"
const consumer={consumer},provider={provider};
export default {{name:'consumer',uses:[provider],setup(scope,declare){{
    declare.serve(consumer,(input,self)=>{{
        {declaration}
        return {{methods:{{read:()=>{read}}},state:{{}}}};
    }},{{lifetime:'host'}});
}}}};
"#,
        consumer = serde_json::to_string(consumer.descriptor()).unwrap(),
        provider = serde_json::to_string(provider.descriptor()).unwrap()
    )
}

fn prepare(
    driver: &ManualDriver,
    host: &Host,
    executor: &Executor,
    source: String,
) -> ExecutableFacet {
    driver
        .run_until(executor.prepare(
            host.id(),
            BundledJsArtifact::new("consumer", source, vec!["default".into()]).unwrap(),
            "default".into(),
        ))
        .unwrap()
}

#[test]
fn unchanged_provider_is_ready_for_a_new_javascript_resource_dependency_during_reload() {
    run(DependencyUse::Resource);
}

#[test]
fn unchanged_provider_hydrates_a_new_javascript_derived_import_during_reload() {
    run(DependencyUse::Derived);
}

#[test]
fn unchanged_provider_accepts_a_new_javascript_initializer_call_during_reload() {
    run(DependencyUse::Initializer);
}

fn run(usage: DependencyUse) {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let _entered = runtime.enter();
    let driver = ManualDriver::new();
    let mut options = HostOptions::new("candidate-dependencies", json!(null));
    options.ready_timeout = Duration::from_millis(50);
    let host = Host::new(options, driver.clone());
    let executor = Executor::new(driver.clone(), ExecutorOptions::default()).unwrap();
    let provider = contract("unchanged.provider", true);
    let consumer = contract("changing.consumer", false);
    let calls = Rc::new(RefCell::new(Vec::new()));
    let entered = calls.clone();
    host.serve(
        provider.clone(),
        Rc::new(move |_, _, declaration| {
            let count = declaration.state(
                "count",
                StateDeclaration::new(json!({"type":"integer"}), json!(10)),
            )?;
            let entered = entered.clone();
            Ok(Implementation::new()
                .state("count", count.readable())
                .json_method("read", move |_, context| {
                    entered.borrow_mut().push(context.code_version());
                    async { Ok(Some(json!(10))) }.boxed_local()
                }))
        }),
        ServeOptions {
            lifetime: Lifetime::Host,
            ..Default::default()
        },
    )
    .unwrap();
    let q = driver
        .run_until(host.get(provider.clone(), json!({}), GetOptions::default()))
        .unwrap();
    let original = q.readable("count").unwrap().snapshot().unwrap();
    let first = prepare(
        &driver,
        &host,
        &executor,
        source(&consumer, &provider, None),
    );
    driver
        .run_until(
            host.install_executable(first, LoadOptions::default())
                .unwrap(),
        )
        .unwrap();
    let reference = driver
        .run_until(host.get(consumer.clone(), json!({}), GetOptions::default()))
        .unwrap();
    assert_eq!(
        driver
            .run_until(reference.call_json("read", None, None))
            .unwrap(),
        Some(json!(1))
    );
    let second = prepare(
        &driver,
        &host,
        &executor,
        source(&consumer, &provider, Some(usage)),
    );
    let result = driver.run_until(host.reload(vec![second], ReloadOptions::default()));
    if result.is_ok() {
        assert_eq!(
            driver.now(),
            Duration::ZERO,
            "preparation must not wait for its deadline"
        );
        assert_eq!(
            driver
                .run_until(reference.call_json("read", None, None))
                .unwrap(),
            Some(json!(11))
        );
        let current = q.readable("count").unwrap().snapshot().unwrap();
        assert_eq!(current.value(), original.value());
        assert_eq!(current.revision(), original.revision());
        let expected: &[u64] = if matches!(usage, DependencyUse::Derived) {
            &[]
        } else {
            &[1]
        };
        assert_eq!(calls.borrow().as_slice(), expected);
    }
    reference.release();
    q.release();
    driver.run_until(host.close()).unwrap();
    let stats = executor.stats();
    assert_eq!(
        (
            stats.callbacks,
            stats.capabilities,
            stats.contexts,
            stats.queued_jobs,
            stats.physical_jobs
        ),
        (0, 0, 0, 0, 0)
    );
    result.unwrap_or_else(|error| panic!("{usage:?} preparation failed: {error:?}"));
}
