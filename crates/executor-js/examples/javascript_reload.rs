//! Replace prepared JavaScript while keeping the reference and its core-owned count.
use futures::FutureExt;
use serde_json::json;
use std::{collections::BTreeMap, rc::Rc};
use tembra_core::{
    contracts::{Contract, Member, ValueSchema},
    *,
};
use tembra_executor_js::{BundledJsArtifact, Executor, ExecutorOptions};
use tembra_native::TokioDriver;

fn token() -> Result<Rc<Contract>> {
    let integer = json!({"type":"integer"});
    Contract::local(
        "example.reload.counter",
        1,
        json!({}),
        BTreeMap::from([
            (
                "count".into(),
                Member::State {
                    schema: integer.clone(),
                },
            ),
            (
                "step".into(),
                Member::Method {
                    args: None,
                    result: Some(ValueSchema::Portable(integer)),
                },
            ),
        ]),
    )
    .map(Rc::new)
    .map_err(|error| RuntimeError::invalid("example_contract", error.message))
}

async fn prepare(
    executor: &Executor,
    host: &Host,
    contract: &Contract,
    increment: u64,
) -> Result<ExecutableFacet> {
    let source = r#"
const token = __TOKEN__;
export default {
    name: 'counter', uses: [],
    setup(scope, declare) {
        declare.serve(token, (input, self) => {
            const count = self.cell('count', { schema: { type: 'integer' }, initial: 0 });
            return {
                state: { count },
                methods: {
                    step(ctx) {
                        const next = count.value + __INCREMENT__;
                        count.replace(ctx, next);
                        return next;
                    }
                }
            };
        });
    }
};
"#
    .replace(
        "__TOKEN__",
        &serde_json::to_string(contract.descriptor())
            .map_err(|error| RuntimeError::invalid("example_descriptor", error.to_string()))?,
    )
    .replace("__INCREMENT__", &increment.to_string());
    let artifact = BundledJsArtifact::new("counter", source, vec!["default".into()])?;
    executor
        .prepare(host.id(), artifact, "default".into())
        .await
}

async fn example(driver: Rc<TokioDriver>) -> Result<()> {
    let host = Host::new(
        HostOptions::new("reload", json!("application")),
        driver.clone(),
    );
    let executor = Executor::new(driver, ExecutorOptions::default())?;
    let contract = token()?;
    let first = prepare(&executor, &host, &contract, 1).await?;
    host.install_executable(first, LoadOptions::default())?
        .await?;
    let reference = host
        .get(contract.clone(), json!({}), GetOptions::default())
        .await?;
    let readable = reference.readable("count")?;
    assert_eq!(
        reference.call_json("step", None, None).await?,
        Some(json!(1))
    );

    // Artifact preparation is outside the core reload transaction.
    let second = prepare(&executor, &host, &contract, 10).await?;
    let report = host.reload(vec![second], ReloadOptions::default()).await?;
    assert_eq!(
        reference.call_json("step", None, None).await?,
        Some(json!(11))
    );
    assert_eq!(readable.snapshot()?.value(), &json!(11));
    println!("code epoch {}; retained count 11", report.current_version());
    reference.release();
    host.close().await?;
    assert_eq!(executor.stats().physical_jobs, 0);
    assert_eq!(executor.stats().callbacks, 0);
    Ok(())
}

fn main() -> Result<()> {
    let driver = TokioDriver::new();
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| RuntimeError::unavailable("reactor", error.to_string()))?
        .block_on(driver.run_until(example(driver.clone()).boxed_local()))
}
