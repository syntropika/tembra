use std::{cell::RefCell, rc::Rc};

use futures::FutureExt;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::json;
use tembra::{Cell, Context, ContractToken, Host, Readable, Result, core};

const LARGE: u64 = 1_792_536_326_221;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
struct Clock {
    wake_at_ms: u64,
    deadlines: Vec<Option<u64>>,
}

fn clock() -> Clock {
    Clock {
        wake_at_ms: LARGE,
        deadlines: vec![Some(LARGE), None],
    }
}

fn javascript_clock() -> serde_json::Value {
    json!({"wake_at_ms":1_792_536_326_221.0,"deadlines":[1_792_536_326_221.0,null]})
}

#[tembra::contract(local, name = "typed.portable-decode", version = 1, input = Clock)]
trait Numbers {
    #[state]
    fn clock(&self) -> u64;

    #[portable]
    async fn echo(&self, value: Clock) -> Result<Clock>;
}

struct Service {
    clock: Cell<u64>,
}

impl NumbersProvider for Service {
    fn clock(&self) -> Readable<u64> {
        self.clock.readable()
    }
    async fn echo(&self, value: Clock, _context: Context) -> Result<Clock> {
        Ok(value)
    }
}

#[test]
fn macro_provider_accepts_large_integral_float_arguments_and_factory_inputs() -> Result<()> {
    let driver = core::ManualDriver::new();
    let host = Host::with_driver("typed-portable-decode-provider", (), driver.clone())?;
    (|| -> tembra::Result<()> {
        let provider_contract = Numbers;
        host.install(
            tembra::NativeFacet::new("numbers-provider", vec![], move |_, setup| {
                let clock_definition = setup.state("clock")?;
                setup.serve(provider_contract, move |input, _, declaration| {
                    assert_eq!(input, clock());
                    Ok(Service {
                        clock: declaration.state(clock_definition.value(input.wake_at_ms)?)?,
                    })
                })
            })?,
            Default::default(),
        )
        .map(drop)
    })()?;
    driver.run_until(async {
        let reference = host
            .core()
            .get(
                Numbers.descriptor()?,
                javascript_clock(),
                core::GetOptions::default(),
            )
            .await?;
        let raw = reference
            .call_json("echo", Some(javascript_clock()), None)
            .await?
            .expect("method result");
        assert_eq!(
            tembra_contracts::from_portable_value::<Clock>(&raw).unwrap(),
            clock()
        );
        let typed = Numbers.bind(reference)?;
        assert_eq!(typed.echo(clock()).await?, clock());
        assert_eq!(typed.clock().value()?, LARGE);
        let mut fractional = javascript_clock();
        fractional["wake_at_ms"] = json!(1.25);
        assert!(
            typed
                .core()
                .call_json("echo", Some(fractional), None)
                .await
                .is_err()
        );
        host.close().await
    })
}

#[test]
fn macro_consumer_and_state_views_decode_actual_portable_number_values() -> Result<()> {
    let driver = core::ManualDriver::new();
    let host = Host::with_driver("typed-portable-decode-consumer", (), driver.clone())?;
    let saved = Rc::new(RefCell::new(None));
    let saved_factory = saved.clone();
    host.core().serve(
        Numbers.descriptor()?,
        Rc::new(move |_, _, declaration| {
            let cell = declaration.state(
                "clock",
                core::StateDeclaration::new(
                    json!({"type":"integer","minimum":0}),
                    json!(1_792_536_326_221.0),
                ),
            )?;
            *saved_factory.borrow_mut() = Some(Cell::<u64>::from_core(cell.clone()));
            Ok(core::Implementation::new()
                .state("clock", cell.readable())
                .json_method("echo", |_, _| {
                    async { Ok(Some(javascript_clock())) }.boxed_local()
                }))
        }),
        core::ServeOptions::default(),
    )?;
    driver.run_until(async {
        let reference = host.get(Numbers, clock()).await?;
        assert_eq!(reference.echo(clock()).await?, clock());
        assert_eq!(reference.clock().value()?, LARGE);
        let cell = saved.borrow_mut().take().expect("declared raw state cell");
        host.core()
            .root(move |context| {
                async move {
                    cell.change(&Context::from_core(context), |value| {
                        assert_eq!(*value, LARGE);
                        *value += 1;
                        Ok(())
                    })?;
                    assert_eq!(cell.value()?, LARGE + 1);
                    Ok(())
                }
                .boxed_local()
            })
            .await?;
        host.close().await
    })
}

#[test]
fn core_serde_method_accepts_integral_floats_before_invoking_typed_code() -> Result<()> {
    let driver = core::ManualDriver::new();
    let host = Host::with_driver("core-portable-decode", (), driver.clone())?;
    host.core().serve(
        Numbers.descriptor()?,
        Rc::new(|_, _, declaration| {
            let state = declaration.state(
                "clock",
                core::StateDeclaration::new(json!({"type":"integer","minimum":0}), json!(LARGE)),
            )?;
            Ok(core::Implementation::new()
                .state("clock", state.readable())
                .serde_method("echo", |value: Clock, _| {
                    async move { Ok(value) }.boxed_local()
                }))
        }),
        core::ServeOptions::default(),
    )?;
    driver.run_until(async {
        let reference = host.get(Numbers, clock()).await?;
        let raw = reference
            .core()
            .call_json("echo", Some(javascript_clock()), None)
            .await?
            .expect("portable result");
        assert_eq!(
            tembra_contracts::from_portable_value::<Clock>(&raw).unwrap(),
            clock()
        );
        host.close().await
    })
}

#[cfg(feature = "embedded-executor")]
#[test]
fn actual_javascript_large_numbers_decode_as_typed_u64_results_and_state() -> Result<()> {
    use tembra_executor_js::{BundledJsArtifact, Executor, ExecutorOptions};
    let driver = tembra_native::TokioDriver::new();
    let host = Host::with_driver("javascript-portable-decode", (), driver.clone())?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(driver.run_until(async {
        let descriptor = serde_json::to_string(Numbers.descriptor()?.descriptor()).unwrap();
        let source = format!(r#"
const token = {descriptor};
export default {{name:'portable-decode', uses:[], setup(scope,declare) {{
  declare.serve(token,(input,self) => {{
    const clock = self.cell('clock', {{schema:token.variants[''].clock.schema,initial:1792536326221}});
    return {{state:{{clock}},methods:{{echo(context,value) {{return {{wake_at_ms:1792536326221,deadlines:[value.wake_at_ms,null]}};}}}}}};
  }});
}}}};
"#);
        let executor = Executor::new(driver.clone(), ExecutorOptions::default())?;
        let facet = executor.prepare_for_host(host.core(), BundledJsArtifact::new("portable-decode", source, vec!["default".into()])?, "default".into()).await?;
        host.core().install_executable(facet, core::LoadOptions::default())?.await?;
        let reference = host.get(Numbers, clock()).await?;
        assert_eq!(reference.echo(clock()).await?, clock());
        assert_eq!(reference.clock().value()?, LARGE);
        host.close().await?;
        assert!(executor.take_errors().is_empty());
        Ok(())
    }))
}
