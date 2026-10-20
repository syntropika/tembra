#![allow(
    dead_code,
    reason = "shared integration helpers are used by distinct focused suites"
)]
use futures::{FutureExt, future::LocalBoxFuture};
use serde_json::json;
use std::{cell::RefCell, collections::BTreeMap, rc::Rc};
use tembra_core::{
    contracts::{Contract, Member},
    *,
};
use tembra_hooks::*;
pub struct Harness {
    pub host: Host,
    pub driver: Rc<ManualDriver>,
    pub manager: HookManager,
}
impl Harness {
    pub fn new() -> Self {
        Self::options(HookManagerOptions::default())
    }
    pub fn options(options: HookManagerOptions) -> Self {
        Self::checked_options(options).unwrap()
    }
    pub fn checked_options(options: HookManagerOptions) -> Result<Self> {
        let driver = ManualDriver::new();
        Self::with_driver(driver.clone(), driver, options)
    }
    pub fn with_driver(
        driver: Rc<ManualDriver>,
        scheduler: Rc<dyn Driver>,
        options: HookManagerOptions,
    ) -> Result<Self> {
        let host = Host::new(HostOptions::new("hooks", json!("host")), scheduler.clone());
        let saved = Rc::new(RefCell::new(None));
        let target = saved.clone();
        let facet = NativeFacet::new("scope", vec![], move |scope, _| {
            *target.borrow_mut() = Some(scope.clone());
            Ok(())
        })
        .unwrap();
        driver
            .run_until(host.install(facet, LoadOptions::default()).unwrap())
            .unwrap();
        let scope = saved.borrow_mut().take().unwrap();
        let manager = HookManager::new(scope, scheduler, options)?;
        Ok(Self {
            host,
            driver,
            manager,
        })
    }
    pub fn run<T: 'static>(&self, f: impl FnOnce(Context) -> HookRun<T> + 'static) -> Result<T> {
        self.driver
            .run_until(self.host.root(move |ctx| f(ctx).boxed_local()))
    }
    pub fn ready(&self) {
        self.driver.run_until(self.host.ready()).unwrap();
    }
    pub fn close(&self) {
        self.driver.run_until(self.host.close()).unwrap();
    }
    pub fn add<A: 'static, R: 'static, H: 'static>(
        &self,
        definition: &RustHookDefinition<A, R>,
        key: &str,
        priority: i64,
        handler: MethodHandler<HookArgs<A, H>, H>,
    ) {
        self.add_raw(definition, key, priority, hook_handler(handler));
    }
    pub fn add_raw<A: 'static, R: 'static, H: 'static>(
        &self,
        definition: &RustHookDefinition<A, R>,
        key: &str,
        priority: i64,
        handler: MethodHandler<HookArgs<A, H>, H>,
    ) {
        let contract = Rc::new(
            Contract::local(
                format!("extension.{key}"),
                1,
                json!({}),
                BTreeMap::from([("run".into(), definition.descriptor().contribution_member())]),
            )
            .unwrap(),
        );
        let (definition, key) = (definition.clone(), key.to_owned());
        self.host
            .serve(
                contract,
                Rc::new(move |_, _, declaration| {
                    definition.register(declaration, key.clone(), priority, "run")?;
                    let handler = handler.clone();
                    Ok(
                        Implementation::new()
                            .method("run", move |args: HookArgs<A, H>, context| {
                                handler(args, context)
                            }),
                    )
                }),
                ServeOptions {
                    lifetime: Lifetime::Host,
                    ..Default::default()
                },
            )
            .unwrap();
    }
    pub fn terminal<A: 'static, R: 'static>(
        &self,
        key: &str,
        input: HookType<A>,
        result: HookType<R>,
        handler: MethodHandler<A, R>,
    ) -> HookTerminal<A, R> {
        let registry = Registry::new(format!("terminal.{key}"), ConflictPolicy::Error).unwrap();
        let view = self.host.registry(&registry).unwrap();
        let contract = Rc::new(
            Contract::local(
                format!("terminal.{key}"),
                1,
                json!({}),
                BTreeMap::from([(
                    "run".into(),
                    Member::Method {
                        args: input.profile().clone(),
                        result: result.profile().clone(),
                    },
                )]),
            )
            .unwrap(),
        );
        self.host
            .serve(
                contract,
                Rc::new(move |_, _, declaration| {
                    declaration.register_method(&registry, "terminal", json!(null), "run")?;
                    let handler = handler.clone();
                    Ok(Implementation::new()
                        .method("run", move |args: A, context| handler(args, context)))
                }),
                ServeOptions {
                    lifetime: Lifetime::Host,
                    ..Default::default()
                },
            )
            .unwrap();
        self.ready();
        let snapshot = view.table(None).unwrap();
        HookTerminal::new(
            snapshot
                .get("terminal")
                .unwrap()
                .operation()
                .unwrap()
                .clone(),
            input,
            result,
        )
        .unwrap()
    }
}
pub fn integer() -> HookType<i32> {
    HookType::serde(json!({"type":"integer"})).unwrap()
}
pub fn definition<A: 'static, R: 'static>(
    name: &str,
    strategy: Strategy,
    input: HookType<A>,
    result: HookType<R>,
    terminal: Option<TerminalProfile>,
) -> RustHookDefinition<A, R> {
    RustHookDefinition::from_descriptor(
        HookDescriptor::new(
            name,
            1,
            strategy,
            input.profile().clone(),
            result.profile().clone(),
            terminal,
        )
        .unwrap(),
        input,
        result,
    )
    .unwrap()
}
pub fn middleware(name: &str) -> RustHookDefinition<i32, i32> {
    definition(
        name,
        Strategy::Middleware,
        integer(),
        integer(),
        Some(TerminalProfile {
            input: integer().profile().clone(),
            result: integer().profile().clone(),
        }),
    )
}
pub fn identity() -> MethodHandler<i32, i32> {
    Rc::new(|value, _| async move { Ok(value) }.boxed_local())
}
pub fn method<A: 'static, R: 'static>(
    f: impl Fn(A, Context) -> LocalBoxFuture<'static, Result<R>> + 'static,
) -> MethodHandler<A, R> {
    Rc::new(f)
}
impl Harness {
    pub fn add_owned<A: 'static, R: 'static, H: 'static>(
        &self,
        definition: &RustHookDefinition<A, R>,
        key: &str,
        build: impl Fn(InvocationOwner) -> MethodHandler<HookArgs<A, H>, H> + 'static,
    ) {
        let contract = Rc::new(
            Contract::local(
                format!("owned.{key}"),
                1,
                json!({}),
                BTreeMap::from([("run".into(), definition.descriptor().contribution_member())]),
            )
            .unwrap(),
        );
        let (definition, key) = (definition.clone(), key.to_owned());
        self.host
            .serve(
                contract,
                Rc::new(move |_, _, declaration| {
                    definition.register(declaration, key.clone(), 0, "run")?;
                    let handler = hook_handler(build(declaration.invocation_owner()));
                    Ok(Implementation::new()
                        .method("run", move |args: HookArgs<A, H>, ctx| handler(args, ctx)))
                }),
                ServeOptions {
                    lifetime: Lifetime::Host,
                    ..Default::default()
                },
            )
            .unwrap();
    }
}
