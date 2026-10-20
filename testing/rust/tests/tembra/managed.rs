#[cfg(feature = "embedded-executor")]
use futures::{FutureExt, channel::oneshot};
use std::{
    cell::{Cell as Flag, RefCell},
    rc::Rc,
};
#[cfg(feature = "embedded-executor")]
use tembra::ContractToken;
use tembra::prelude::*;
use tembra::{ErrorCode, ManualDriver, core};

#[tembra::contract(local, name = "dx.context-probe", version = 1)]
trait Probe {
    async fn inspect(&self) -> Result<(u64, u64, String)>;
    async fn wait(&self) -> Result<()>;
}

struct ProbeService {
    calls: Rc<Flag<usize>>,
    entered: Rc<RefCell<Option<Context>>>,
}
impl ProbeProvider for ProbeService {
    async fn inspect(&self, ctx: Context) -> Result<(u64, u64, String)> {
        self.calls.set(self.calls.get() + 1);
        Ok((
            ctx.root_id(),
            ctx.code_version(),
            ctx.principal().as_str().unwrap().into(),
        ))
    }
    async fn wait(&self, ctx: Context) -> Result<()> {
        *self.entered.borrow_mut() = Some(ctx.clone());
        Err(ctx.cancellation().cancelled().await)
    }
}

#[test]
fn context_views_preserve_roots_principals_expiry_cancellation_and_owned_claims() -> Result<()> {
    let driver = ManualDriver::new();
    let host = Host::with_driver("dx-context", "host-principal", driver.clone())?;
    let calls = Rc::new(Flag::new(0));
    let entered = Rc::new(RefCell::new(None));
    let (service_calls, service_entered) = (calls.clone(), entered.clone());
    host.serve(Probe, move |_, _, _| {
        Ok(ProbeService {
            calls: service_calls.clone(),
            entered: service_entered.clone(),
        })
    })?;
    driver.run_until(async {
        let reference = host.get(Probe, ProbeInput {}).await?;
        let cloned = reference.clone();
        let scoped = reference.clone();
        let (expired, view) = host
            .root_as("ingress-principal", move |ctx| async move {
                let view = scoped.with_context(&ctx);
                let (root, version, principal) = view.inspect().await?;
                assert_eq!(root, ctx.root_id());
                assert_eq!(version, ctx.code_version());
                // Object execution acts as its Host while retaining ingress root/version.
                assert_eq!(principal, "host-principal");
                Ok((ctx, view))
            })
            .await?;
        assert!(expired.check(host.core().id()).is_err());
        assert!(view.inspect().await.is_err());
        assert_eq!(calls.get(), 1);

        let waiting = reference.clone();
        host.root(move |ctx| async move {
            let view = waiting.with_context(&ctx);
            let call = view.wait();
            futures::pin_mut!(call);
            assert!(futures::poll!(call.as_mut()).is_pending());
            ctx.cancel(RuntimeError::aborted("test", "caller cancellation"));
            assert_eq!(call.await.unwrap_err().code, ErrorCode::Aborted);
            Ok(())
        })
        .await?;
        assert!(
            entered
                .borrow()
                .as_ref()
                .unwrap()
                .check(host.core().id())
                .is_err()
        );

        let retained = host
            .root(|ctx| async move { ctx.get(Probe, ProbeInput {}).await })
            .await?;
        assert_eq!(retained.core().status(), core::RefStatus::Released);
        assert!(retained.inspect().await.is_err());
        reference.core().release();
        assert_eq!(cloned.core().status(), core::RefStatus::Released);
        assert_eq!(
            cloned.inspect().await.unwrap_err().code,
            ErrorCode::Released
        );
        host.close().await
    })
}

// Serialization remains portable while the contract's unsigned schema is stricter.
#[derive(Debug, Serialize, Deserialize, PartialEq)]
#[serde(crate = "tembra::serde", transparent)]
struct NonNegative(i64);
impl JsonSchema for NonNegative {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "NonNegative".into()
    }
    fn json_schema(generator: &mut tembra::schemars::SchemaGenerator) -> tembra::schemars::Schema {
        u64::json_schema(generator)
    }
}

#[tembra::contract(remote, name = "dx.remote-counter", version = 1, host = "dx-server")]
trait RemoteCounter {
    #[state]
    fn count(&self) -> u64;
    async fn increment(&self, by: u64) -> Result<u64>;
    async fn invalid_result(&self) -> Result<u64>;
    async fn constrained(&self, value: NonNegative) -> Result<NonNegative>;
    async fn invalid_schema_result(&self) -> Result<NonNegative>;
    async fn nullable(&self, value: Option<u64>) -> Result<Option<u64>>;
    async fn ping(&self) -> Result<()>;
}
struct CounterService {
    count: Cell<u64>,
    calls: Rc<Flag<usize>>,
}
impl RemoteCounterProvider for CounterService {
    fn count(&self) -> Readable<u64> {
        self.count.readable()
    }
    async fn increment(&self, by: u64, ctx: Context) -> Result<u64> {
        self.calls.set(self.calls.get() + 1);
        self.count.change(&ctx, |count| {
            *count += by;
            Ok(())
        })?;
        self.count.value()
    }
    async fn invalid_result(&self, _ctx: Context) -> Result<u64> {
        Ok(u64::MAX)
    }
    async fn constrained(&self, value: NonNegative, _ctx: Context) -> Result<NonNegative> {
        self.calls.set(self.calls.get() + 1);
        Ok(value)
    }
    async fn invalid_schema_result(&self, _ctx: Context) -> Result<NonNegative> {
        self.calls.set(self.calls.get() + 1);
        Ok(NonNegative(-1))
    }
    async fn nullable(&self, value: Option<u64>, _ctx: Context) -> Result<Option<u64>> {
        Ok(value)
    }
    async fn ping(&self, _ctx: Context) -> Result<()> {
        Ok(())
    }
}

#[test]
fn typed_remote_acquisition_calls_state_and_validation_use_the_shared_rpc_core() -> Result<()> {
    use core::rpc::*;
    let driver = ManualDriver::new();
    let server = Host::with_driver("dx-server", "server", driver.clone())?;
    let client = Host::with_driver("dx-client", "client", driver.clone())?;
    server.core().set_rpc_identity(RootOrigin::new(
        "dx-server",
        "11111111111111111111111111111111",
    )?)?;
    client.core().set_rpc_identity(RootOrigin::new(
        "dx-client",
        "22222222222222222222222222222222",
    )?)?;
    let (left, right, _) = memory_link(driver.clone(), MemoryLinkOptions::default())?;
    let outbound = client
        .core()
        .connect_client(left, "dx-server", ConnectionOptions::default())?;
    let _inbound = server.core().accept_client(
        right,
        "authenticated-user".into(),
        ConnectionOptions::default(),
    )?;
    client.core().set_route("dx-server", outbound)?;
    let calls = Rc::new(Flag::new(0));
    let invoked = calls.clone();
    server.serve(RemoteCounter, move |_, _, declaration| {
        Ok(CounterService {
            count: declaration.state("count", 0_u64)?,
            calls: invoked.clone(),
        })
    })?;
    driver.run_until(async {
        let reference = client.get(RemoteCounter, RemoteCounterInput {}).await?;
        assert_eq!(reference.count().value()?, 0);
        assert_eq!(reference.increment(2).await?, 2);
        assert_eq!(reference.count().value()?, 2);
        assert!(reference.increment(u64::MAX).await.is_err());
        assert_eq!(calls.get(), 1);
        assert!(reference.invalid_result().await.is_err());
        assert_eq!(reference.count().value()?, 2);
        let same_host = server.get(RemoteCounter, RemoteCounterInput {}).await?;
        for reference in [&reference, &same_host] {
            assert_eq!(reference.constrained(NonNegative(3)).await?, NonNegative(3));
            let before = calls.get();
            let error = reference.constrained(NonNegative(-1)).await.unwrap_err();
            assert_eq!(error.code, ErrorCode::Invalid);
            assert_eq!(error.reason, "args");
            assert_eq!(
                calls.get(),
                before,
                "schema refusal must precede provider entry"
            );
            let error = reference.invalid_schema_result().await.unwrap_err();
            assert_eq!(error.code, ErrorCode::Invalid);
            assert_eq!(error.reason, "result");
            assert_eq!(
                calls.get(),
                before + 1,
                "result refusal does not undo handler effects"
            );
            assert_eq!(reference.nullable(None).await?, None);
            assert_eq!(reference.nullable(Some(7)).await?, Some(7));
            reference.ping().await?;
            assert_eq!(reference.core().call_json("ping", None, None).await?, None);
            assert!(
                reference
                    .core()
                    .call_json("nullable", None, None)
                    .await
                    .is_err()
            );
        }
        client.close().await?;
        server.close().await
    })
}

#[cfg(feature = "embedded-executor")]
#[tembra::contract(local, name = "dx.gate", version = 1)]
trait Gate {
    #[portable]
    async fn wait(&self) -> Result<()>;
}
#[cfg(feature = "embedded-executor")]
struct GateService {
    receive: Rc<RefCell<Option<oneshot::Receiver<()>>>>,
}
#[cfg(feature = "embedded-executor")]
impl GateProvider for GateService {
    async fn wait(&self, _ctx: Context) -> Result<()> {
        let receive = self.receive.borrow_mut().take().unwrap();
        receive
            .await
            .map_err(|_| RuntimeError::aborted("gate", "gate dropped"))
    }
}
#[cfg(feature = "embedded-executor")]
#[tembra::contract(local, name = "dx.plugin", version = 1)]
trait Plugin {
    #[state]
    fn count(&self) -> u64;
    #[portable]
    async fn version(&self) -> Result<u64>;
    #[portable]
    async fn delayed(&self) -> Result<u64>;
}

#[cfg(feature = "embedded-executor")]
fn source(version: u64, incompatible: bool) -> Result<String> {
    let mut descriptor = Plugin.descriptor()?.descriptor().clone();
    if incompatible {
        descriptor.variants.get_mut("").unwrap().remove("version");
        // Rebuild canonical metadata rather than trusting a stale fingerprint.
        descriptor.fingerprint.clear();
        descriptor = tembra::contracts::Contract::from_descriptor(descriptor)
            .map_err(|error| RuntimeError::invalid(error.reason, error.message))?
            .descriptor()
            .clone();
    }
    let token = tembra::serde_json::to_string(&descriptor).unwrap();
    let gate = tembra::serde_json::to_string(Gate.descriptor()?.descriptor()).unwrap();
    Ok(format!(
        r#"
const token = {token}, gate = {gate};
export default {{name: 'dx.extension', uses: [gate], setup(scope, declare) {{
  declare.serve(token, (input, self) => {{
    const dependency = self.get(gate, {{}});
    const count = self.cell('count', {{schema: token.variants[''].count.schema, initial: 0}});
    return {{state: {{count}}, methods: {{
      version(ctx) {{count.change(ctx, draft => {{draft.value++;}}); return {version};}},
      async delayed(ctx) {{await dependency.wait(ctx); return {version};}}
    }}}};
  }});
}}}};
"#
    ))
}

#[cfg(feature = "embedded-executor")]
#[test]
fn typed_references_follow_actual_javascript_reload_and_retain_entered_versions() -> Result<()> {
    use tembra_executor_js::{BundledJsArtifact, Executor, ExecutorOptions};
    let driver = tembra_native::TokioDriver::new();
    let host = Host::with_driver("dx-reload", (), driver.clone())?;
    let (complete, receive) = oneshot::channel();
    let receive = Rc::new(RefCell::new(Some(receive)));
    host.serve(Gate, move |_, _, _| {
        Ok(GateService {
            receive: receive.clone(),
        })
    })?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(driver.run_until(async {
        let executor = Executor::new(driver.clone(), ExecutorOptions::default())?;
        let prepare = |version, incompatible| {
            executor.prepare(
                host.core().id(),
                BundledJsArtifact::new(
                    "dx.extension",
                    source(version, incompatible).unwrap(),
                    vec!["default".into()],
                )
                .unwrap(),
                "default".into(),
            )
        };
        let initial = prepare(1, false).await?;
        host.core()
            .install_executable(initial, core::LoadOptions::default())?
            .await?;
        let reference = host.get(Plugin, PluginInput {}).await?;
        let object = reference.core().object_id();
        assert_eq!(reference.version().await?, 1);
        let readable = reference.count();
        assert_eq!(readable.value()?, 1);
        let old = reference.delayed().boxed_local();
        futures::pin_mut!(old);
        assert!(futures::poll!(old.as_mut()).is_pending());
        let rejected = prepare(2, true).await?;
        assert!(
            host.core()
                .reload(vec![rejected], core::ReloadOptions::default())
                .await
                .is_err()
        );
        assert_eq!(reference.version().await?, 1);
        let replacement = prepare(2, false).await?;
        host.core()
            .reload(vec![replacement], core::ReloadOptions::default())
            .await?;
        assert_eq!(reference.core().object_id(), object);
        assert_eq!(readable.value()?, 2);
        assert_eq!(reference.version().await?, 2);
        complete.send(()).unwrap();
        assert_eq!(old.await?, 1);
        assert_eq!(readable.value()?, 3);
        host.close().await?;
        assert!(executor.take_errors().is_empty());
        Ok(())
    }))
}
