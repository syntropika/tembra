//! Execute bundled TypeScript through the actual Tembra kernel and a native HTTP capability.
mod adapter;
mod engine;
use crate::adapter::Runtime;
use anyhow::{Result, ensure};
use engine::{CAPABILITIES, EngineHost, Progress, Report, Request, Response};
use futures::{FutureExt, channel::oneshot, executor::LocalPool};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    cell::{Cell, RefCell},
    fs,
    rc::Rc,
    thread,
    time::Duration,
};
use tembra::{Code, Context, Error, Hook, Host, Orb, Service};

#[derive(Clone, Serialize, Deserialize)]
pub struct Job {
    request: Request,
    report: Option<Report>,
}
const ANALYZER: &str = "example.analyzer";
fn job(path: &str) -> Job {
    Job {
        request: Request {
            path: path.into(),
            threshold: 22.0,
        },
        report: None,
    }
}
fn require_owner(context: &Context) -> tembra::Result<()> {
    context.check()?;
    if context.principal()? != "alice" {
        return Err(Error::new(
            Code::Forbidden,
            "example.owner",
            "principal cannot use this capability",
        ));
    }
    Ok(())
}
async fn fetch(base: String, path: String) -> tembra::Result<Response> {
    let (send, receive) = oneshot::channel();
    let worker = thread::spawn(move || -> Result<()> {
        let response = (|| -> Result<Response> {
            let agent: ureq::Agent = ureq::Agent::config_builder()
                .timeout_global(Some(Duration::from_secs(5)))
                .http_status_as_error(false)
                .build()
                .into();
            let mut response = agent.get(format!("{base}{path}")).call()?;
            let status = response.status().as_u16();
            let body = response
                .body_mut()
                .with_config()
                .limit(64 * 1024)
                .read_to_string()?;
            Ok(Response { status, body })
        })()
        .map_err(|error| error.to_string());
        let _ = send.send(response);
        Ok(())
    });
    let received = receive.await;
    worker
        .join()
        .map_err(|_| Error::invalid("example.http", "HTTP worker panicked"))?
        .map_err(|error| Error::invalid("example.http", error.to_string()))?;
    received
        .map_err(|_| Error::invalid("example.http", "HTTP worker stopped"))?
        .map_err(|message| Error::new(Code::Unavailable, "example.http", message))
}
fn assert_report(job: &Job, calibration: f64) -> Result<Value> {
    let report = job
        .report
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("missing report"))?;
    ensure!(report.station == "greenhouse" && report.count == 4);
    ensure!(report.mean == 21.0 + calibration && report.median == 20.5 + calibration);
    ensure!(report.minimum == 18.0 + calibration && report.maximum == 25.0 + calibration);
    ensure!(report.calibration == calibration && report.alert == (calibration > 0.0));
    Ok(serde_json::to_value(report)?)
}
fn main() -> Result<()> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    ensure!(
        args.len() == 5,
        "usage: typescript-http-orb <engine> <script-a> <script-b> <api-base> <report>"
    );
    let mut pool = LocalPool::new();
    let host = Host::new(pool.spawner());
    let progress = Rc::new(RefCell::new(Vec::<Value>::new()));
    let requests = Rc::new(RefCell::new(Vec::<Value>::new()));
    let blocked = Rc::new(Cell::new(0));
    let events = progress.clone();
    let calls = requests.clone();
    let denials = blocked.clone();
    let base = args[3].clone();
    ensure!(
        base.starts_with("http://127.0.0.1:"),
        "this reproducible fixture expects a loopback API"
    );
    let hook = Hook::<Job, Job>::waterfall_until("analyze", |value| value.report.is_some());
    let declared = hook.clone();
    host.install(vec![
        Orb::new(ANALYZER, 1).hook(hook).service(Service::new(ANALYZER, move |d| d.hook(&declared)).method("run", |hook, context, input: Job| async move {
            let path = input.request.path.clone();
            let threshold = input.request.threshold;
            let output = hook.run(&context, input).await?;
            if output.request.path != path || output.request.threshold != threshold { return Err(Error::invalid("example.input", "confirmed request changed")); }
            Ok(output)
        })),
        Orb::new(CAPABILITIES, 1).service(Service::new(CAPABILITIES, move |_| Ok((base.clone(), events.clone(), calls.clone(), denials.clone())))
            .method("get", |state, context, path: String| async move {
                require_owner(&context)?;
                if !["/readings", "/invalid", "/broken-json", "/http-error"].contains(&path.as_str()) {
                    state.3.set(state.3.get() + 1);
                    return Err(Error::new(Code::Forbidden, "example.path", "API path is not allowed"));
                }
                let invocation = context.invocation()?;
                state.2.borrow_mut().push(json!({"path":path,"principal":context.principal()?,"root":context.root,"version":invocation.code_version}));
                fetch(state.0.clone(), path).await
            })
            .method("publish", |state, context, event: Progress| async move {
                require_owner(&context)?;
                let invocation = context.invocation()?;
                state.1.borrow_mut().push(json!({"stage":event.stage,"message":event.message,"principal":context.principal()?,"root":context.root,"version":invocation.code_version}));
                Ok(())
            })),
    ])?;
    let runtime = Rc::new(Runtime::<Job, Job>::attach(
        host.clone(),
        ANALYZER,
        "analyze",
    )?);
    let reference = Rc::new(host.get(ANALYZER, vec!["main".into()], "alice")?);
    pool.run_until(reference.ready())?;
    let engine = EngineHost::new(&fs::read(&args[0])?)?;
    let (a, inspect_a) = pool.run_until(engine.prepare(
        &pool.spawner(),
        "analysis-a",
        &fs::read_to_string(&args[1])?,
    ))?;
    let (b, inspect_b) = pool.run_until(engine.prepare(
        &pool.spawner(),
        "analysis-b",
        &fs::read_to_string(&args[2])?,
    ))?;
    ensure!(
        requests.borrow().is_empty() && progress.borrow().is_empty(),
        "preparation invoked native capabilities"
    );
    runtime.install("example.readings-orb", a)?;
    let invoke = |pool: &mut LocalPool, path: &str| {
        let reference = reference.clone();
        let input = job(path);
        pool.run_until(host.root("alice", move |context| {
            async move { reference.call::<_, Job>(&context, "run", input).await }.boxed_local()
        }))
    };
    let first = invoke(&mut pool, "/readings")?;
    let first_report = assert_report(&first, 0.0)?;
    ensure!(
        progress
            .borrow()
            .iter()
            .map(|x| x["stage"].as_str().unwrap_or(""))
            .collect::<Vec<_>>()
            == ["fetching", "calculating", "done"]
    );
    let mut checks = vec![
        "typescript_npm_http_calculation_and_progress",
        "no_native_effect_during_source_preparation",
    ];
    let mut errors = Vec::new();
    for path in ["/invalid", "/broken-json", "/http-error", "/blocked"] {
        let count = requests.borrow().len();
        let error = match invoke(&mut pool, path) {
            Err(error) => error,
            Ok(_) => anyhow::bail!("invalid response was accepted"),
        };
        ensure!(
            error.reason == "example.analysis",
            "unexpected failure: {error}"
        );
        if path == "/http-error" {
            ensure!(error.message.contains("HTTP 503"));
        }
        if path == "/blocked" {
            ensure!(requests.borrow().len() == count && blocked.get() == 1);
        }
        errors.push(json!({"path":path,"message":error.message}));
        assert_report(&invoke(&mut pool, "/readings")?, 0.0)?;
        ensure!(inspect_a.registry.is_empty() && pool.run_until(inspect_a.resources_empty()));
    }
    checks.extend([
        "invalid_data_recovery",
        "invalid_json_recovery",
        "http_error_recovery",
        "forbidden_path_before_network_and_recovery",
    ]);
    let active = runtime.clone();
    let retained = reference.clone();
    let retained_reports = pool.run_until(host.root("alice", move |context| {
        async move {
            let before: Job = retained.call(&context, "run", job("/readings")).await?;
            active.install("example.readings-orb", b)?;
            let after: Job = retained.call(&context, "run", job("/readings")).await?;
            Ok((before, after))
        }
        .boxed_local()
    }))?;
    assert_report(&retained_reports.0, 0.0)?;
    assert_report(&retained_reports.1, 0.0)?;
    let second_report = assert_report(&invoke(&mut pool, "/readings")?, 1.5)?;
    checks.push("script_edit_changes_new_roots_and_preserves_existing_root");
    ensure!(inspect_a.registry.is_empty() && inspect_b.registry.is_empty());
    ensure!(
        pool.run_until(inspect_a.resources_empty()) && pool.run_until(inspect_b.resources_empty())
    );
    drop(reference);
    pool.run_until(runtime.close())?;
    pool.run_until_stalled();
    ensure!(
        inspect_a.released()
            && inspect_b.released()
            && inspect_a.actor_stopped.get()
            && inspect_b.actor_stopped.get()
    );
    let diagnostics = host.kernel().diagnostics();
    let after_close = json!({"roots":diagnostics.roots,"calls":diagnostics.calls,"tasks":diagnostics.tasks,"resources":diagnostics.resources,"objects":diagnostics.objects,"orbs":diagnostics.orbs,"references":diagnostics.references});
    ensure!(
        after_close
            .as_object()
            .is_some_and(|map| map.values().all(|value| value == 0))
    );
    ensure!(runtime.status()["retained_callbacks"] == 0);
    checks.push("sessions_import_actors_leases_and_kernel_released");
    let report = json!({"semantic_pass":true,"checks":checks,"version_a":first_report,"version_b":second_report,"declared_errors":errors,"requests":*requests.borrow(),"progress":*progress.borrow(),"kernel_after_close":after_close,"adapter_status":runtime.status(),"actors_stopped":true,"sessions_released":true,"invocation_leases_empty":true,"scope":"Actual Tembra kernel plus Wasmtime, reusable QuickJS component and bundled TypeScript; local HTTP API, not a product migration or mobile qualification"});
    fs::write(&args[4], serde_json::to_string_pretty(&report)? + "\n")?;
    println!("Version A: {}", report["version_a"]);
    println!("Version B: {}", report["version_b"]);
    println!(
        "{} semantic checks passed; HTTP and progress traces retained; kernel empty after close",
        checks.len()
    );
    Ok(())
}
