#[path = "scope_retention/observers.rs"]
mod observers;

use futures::FutureExt;
use serde_json::json;
use std::{cell::Cell, collections::BTreeMap, rc::Rc, time::Duration};
use tembra_core::{
    contracts::{Contract, Member, ValueSchema},
    *,
};
use tembra_executor_js::{BundledJsArtifact, Executor, ExecutorOptions};
use tembra_native::TokioDriver;

fn run(body: impl FnOnce(Rc<TokioDriver>) -> futures::future::LocalBoxFuture<'static, ()>) {
    let driver = TokioDriver::new();
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(driver.run_until(async {
            tokio::time::timeout(Duration::from_secs(15), body(driver.clone()))
                .await
                .unwrap();
        }));
}

fn service() -> Rc<Contract> {
    Rc::new(
        Contract::local(
            "scope-retention",
            1,
            json!({}),
            BTreeMap::from([(
                "read".into(),
                Member::Method {
                    args: None,
                    result: Some(ValueSchema::Portable(json!({"type":"integer"}))),
                },
            )]),
        )
        .unwrap(),
    )
}

async fn prepare(host: &Host, executor: &Executor, source: String) -> ExecutableFacet {
    executor
        .prepare(
            host.id(),
            BundledJsArtifact::new("scope-retention", source, vec!["default".into()]).unwrap(),
            "default".into(),
        )
        .await
        .unwrap()
}

fn assert_closed(executor: &Executor) {
    let stats = executor.stats();
    assert_eq!(stats.callbacks, 0, "{stats:?}");
    assert_eq!(stats.capabilities, 0, "{stats:?}");
    assert_eq!(stats.contexts, 0, "{stats:?}");
    assert_eq!(stats.queued_jobs, 0, "{stats:?}");
    assert_eq!(stats.physical_jobs, 0, "{stats:?}");
}

async fn repeated_setup(driver: Rc<TokioDriver>, touch: &'static str) {
    let host = Host::new(
        HostOptions::new("scope-retention", json!(null)),
        driver.clone(),
    );
    let executor = Executor::new(
        driver,
        ExecutorOptions {
            max_evaluations: 1,
            max_capabilities: 32,
            ..Default::default()
        },
    )
    .unwrap();
    let contract = service();
    let source = format!(
        r#"
const token = {};
export default {{
    name: 'scope-retention', uses: [], setup(scope, declare) {{
        {touch}
        declare.serve(token, () => ({{methods: {{read: () => 1}}}}), {{lifetime: 'host'}});
    }}
}};
"#,
        serde_json::to_string(contract.descriptor()).unwrap()
    );
    let facet = prepare(&host, &executor, source).await;
    host.install_executable(facet.clone(), LoadOptions::default())
        .unwrap()
        .await
        .unwrap();
    let reference = host
        .get(contract, json!({}), GetOptions::default())
        .await
        .unwrap();
    let mut plateau = None;
    for step in 0..100 {
        host.reload(vec![facet.clone()], ReloadOptions::default())
            .await
            .unwrap();
        assert_eq!(
            reference.call_json("read", None, None).await.unwrap(),
            Some(json!(1))
        );
        tokio::task::yield_now().await;
        let stats = executor.stats();
        if step == 2 {
            plateau = Some(stats);
        } else if let Some(plateau) = plateau {
            assert_eq!(
                stats.capabilities, plateau.capabilities,
                "step {step}: {stats:?}"
            );
            assert_eq!(stats.callbacks, plateau.callbacks, "step {step}: {stats:?}");
        }
        assert_eq!(stats.evaluations, 1);
        assert_eq!(stats.contexts, 0);
        assert_eq!(stats.physical_jobs, 0);
        assert_eq!(stats.queued_jobs, 0);
        assert!(
            stats.capabilities <= 16,
            "spare capacity disappeared: {stats:?}"
        );
    }
    reference.release();
    host.close().await.unwrap();
    drop(facet);
    tokio::task::yield_now().await;
    assert_closed(&executor);
}

#[test]
fn reused_setup_without_scope_views_is_a_stable_control() {
    run(|driver| repeated_setup(driver, "").boxed_local());
}

#[test]
fn reused_setup_signal_releases_retired_scope_slots() {
    run(|driver| repeated_setup(driver, "void scope.signal;").boxed_local());
}

#[test]
fn reused_setup_registry_releases_retired_scope_slots() {
    run(|driver| {
        repeated_setup(
            driver,
            "scope.registry({name:'stable-view',conflict:'error'});",
        )
        .boxed_local()
    });
}

#[test]
fn retained_old_scope_supports_cleanup_and_registry_subscription_reentry() {
    run(|driver| cleanup_scope(driver).boxed_local());
}

async fn cleanup_scope(driver: Rc<TokioDriver>) {
    let host = Host::new(
        HostOptions::new("cleanup-scope", json!(null)),
        driver.clone(),
    );
    let executor = Executor::new(driver, ExecutorOptions::default()).unwrap();
    let service = service();
    let monitor = Rc::new(
        Contract::local(
            "scope-monitor",
            1,
            json!({}),
            BTreeMap::from([(
                "mark".into(),
                Member::Method {
                    args: None,
                    result: None,
                },
            )]),
        )
        .unwrap(),
    );
    let marks = Rc::new(Cell::new(0));
    let recorded = marks.clone();
    host.serve(
        monitor.clone(),
        Rc::new(move |_, _, _| {
            let recorded = recorded.clone();
            Ok(Implementation::new().json_method("mark", move |_, _| {
                recorded.set(recorded.get() + 1);
                async { Ok(None) }.boxed_local()
            }))
        }),
        ServeOptions::default(),
    )
    .unwrap();
    let source = format!(
        r#"
const service = {service}, monitor = {monitor};
const registry = {{name: 'cleanup-view', conflict: 'error'}};
export default {{
    name: 'scope-retention', uses: [monitor], setup(scope, declare) {{
        const signal = scope.signal, view = scope.registry(registry);
        declare.serve(service, (input, self) => {{
            const log = self.get(monitor, {{}});
            self.resource('retained', {{
                deps: [log], open() {{
                    let observed = 0;
                    const stop = view.subscribe(table => {{
                        if (scope.signal !== signal || scope.registry(registry) !== view)
                            throw Error('scope views changed during reentry');
                        observed++;
                    }});
                    stop();
                    if (observed !== 1) throw Error('initial registry observation missing');
                    return {{signal, finish: () => scope.root(ctx => log.mark(ctx))}};
                }},
                async close(value) {{
                    if (!signal.aborted || !value.signal.aborted)
                        throw Error('retired run signal remained active');
                    if (view.table().entries.length !== 0)
                        throw Error('retained registry view changed');
                    await value.finish();
                    await scope.root(ctx => log.mark(ctx));
                }}
            }});
            return {{methods: {{read: () => 1}}}};
        }}, {{lifetime: 'host'}});
    }}
}};
"#,
        service = serde_json::to_string(service.descriptor()).unwrap(),
        monitor = serde_json::to_string(monitor.descriptor()).unwrap()
    );
    let facet = prepare(&host, &executor, source).await;
    host.install_executable(facet.clone(), LoadOptions::default())
        .unwrap()
        .await
        .unwrap();
    let reference = host
        .get(service, json!({}), GetOptions::default())
        .await
        .unwrap();
    host.reload(vec![facet.clone()], ReloadOptions::default())
        .await
        .unwrap();
    reference.release();
    host.close().await.unwrap();
    assert!(host.errors().is_empty(), "{:?}", host.errors());
    assert_eq!(
        marks.get(),
        2,
        "old and newly adopted cleanup scopes must both work"
    );
    drop(facet);
    tokio::task::yield_now().await;
    assert_closed(&executor);
}
