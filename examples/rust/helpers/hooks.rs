//! Shared plumbing for the hook examples: string-valued hook definitions, plugin registration
//! and terminal registration. The examples themselves contain only hook logic.
#![allow(
    dead_code,
    reason = "each hook example uses a different subset of these helpers"
)]

use std::{collections::BTreeMap, rc::Rc};
use tembra::contracts::{Contract, ContractError, Member};
use tembra::core::{
    ConflictPolicy, Context, FacetScope, Factory, Implementation, Lifetime, RegisteredOperation,
    Registry,
};
use tembra::futures::{FutureExt, future::LocalBoxFuture};
use tembra::serde_json::json;
use tembra::{FacetDeclaration, Result, RuntimeError, ServeOptions};
use tembra_hooks::{
    HookArgs, HookDescriptor, HookType, Next, RustHookDefinition, Strategy, TerminalProfile,
    hook_handler,
};

/// The portable value carried by every hook in these examples: a string.
pub fn text() -> Result<HookType<String>> {
    HookType::serde(json!({ "type": "string" }))
}

/// A hook definition over strings: name, version 1, strategy, result type and terminal shape.
pub fn define<R: 'static>(
    name: &str,
    strategy: Strategy,
    result: HookType<R>,
    terminal: Option<TerminalProfile>,
) -> Result<RustHookDefinition<String, R>> {
    let input = text()?;
    let descriptor = HookDescriptor::new(
        name,
        1,
        strategy,
        input.profile().clone(),
        result.profile().clone(),
        terminal,
    )?;
    RustHookDefinition::from_descriptor(descriptor, input, result)
}

/// A waterfall over strings: each handler receives the current text and returns the next one.
pub fn waterfall(name: &str) -> Result<RustHookDefinition<String, String>> {
    define(name, Strategy::Waterfall, text()?, None)
}

/// A middleware over strings whose terminal also maps a string to a string.
pub fn middleware(name: &str) -> Result<RustHookDefinition<String, String>> {
    let profile = text()?.profile().clone();
    let terminal = TerminalProfile {
        input: profile.clone(),
        result: profile,
    };
    define(name, Strategy::Middleware, text()?, Some(terminal))
}

/// Wraps an already computed value as the future a handler must return.
pub fn ready<T: 'static>(value: Result<T>) -> LocalBoxFuture<'static, Result<T>> {
    async move { value }.boxed_local()
}

/// A plugin handler: it receives the pipeline input and, for middleware, the continuation.
pub type Handler<H> =
    Rc<dyn Fn(String, Option<Next<String, H>>) -> LocalBoxFuture<'static, Result<H>>>;

/// Registers a host-lifetime object whose single method `run` contributes to `hook`.
///
/// `H` is the handler's result type and must match the strategy: the hook's own result type
/// for waterfall, parallel and middleware, `BailDecision` for bail and `()` for series.
pub fn register_plugin<R: 'static, H: 'static>(
    setup: &mut FacetDeclaration<'_, '_>,
    hook: RustHookDefinition<String, R>,
    name: &str,
    priority: i64,
    handler: impl Fn(String, Option<Next<String, H>>) -> LocalBoxFuture<'static, Result<H>> + 'static,
) -> Result<()> {
    let members = BTreeMap::from([("run".to_string(), hook.descriptor().contribution_member())]);
    let contract =
        Contract::local(format!("plugin.{name}"), 1, json!({}), members).map_err(contract_error)?;
    let name = name.to_string();
    let handler: Handler<H> = Rc::new(handler);
    let factory: Factory = Rc::new(move |_, _, declaration| {
        // The contribution: one row in the hook's registry naming this object's `run` method.
        hook.register(declaration, name.clone(), priority, "run")?;
        let (label, handler) = (name.clone(), handler.clone());
        let run = hook_handler(Rc::new(move |args: HookArgs<String, H>, _: Context| {
            let (input, next) = args.into_parts();
            println!("      [plugin {label}] received {input:?}");
            handler(input, next)
        }));
        Ok(Implementation::new().method("run", move |args, context| run(args, context)))
    });
    setup
        .core_mut()
        .serve(Rc::new(contract), factory, host_lifetime())
}

/// Registers a plugin whose handler is a plain string transformation, for waterfall hooks.
pub fn register_transform(
    setup: &mut FacetDeclaration<'_, '_>,
    hook: RustHookDefinition<String, String>,
    name: &str,
    priority: i64,
    transform: fn(String) -> String,
) -> Result<()> {
    register_plugin(setup, hook, name, priority, move |input, _| {
        ready(Ok(transform(input)))
    })
}

/// A registry in which terminals are published for middleware owners to select.
pub fn terminal_registry(name: &str) -> Result<Registry> {
    Registry::new(name, ConflictPolicy::Error)
}

/// Registers a host-lifetime object whose method `run` is published in `registry` under `key`.
pub fn register_terminal(
    setup: &mut FacetDeclaration<'_, '_>,
    registry: &Registry,
    key: &str,
    run: fn(String) -> String,
) -> Result<()> {
    let profile = text()?.profile().clone();
    let member = Member::Method {
        args: profile.clone(),
        result: profile,
    };
    let members = BTreeMap::from([("run".to_string(), member)]);
    let contract = Contract::local(format!("terminal.{key}"), 1, json!({}), members)
        .map_err(contract_error)?;
    let (registry, key) = (registry.clone(), key.to_string());
    let factory: Factory = Rc::new(move |_, _, declaration| {
        declaration.register_method(&registry, key.clone(), json!(null), "run")?;
        Ok(
            Implementation::new().method("run", move |input: String, _: Context| {
                ready(Ok(run(input)))
            }),
        )
    });
    setup
        .core_mut()
        .serve(Rc::new(contract), factory, host_lifetime())
}

/// Selects the terminal published under `key`, as seen by the current context.
///
/// Owners select it on every run instead of caching it, so a replaced terminal is picked up.
pub fn select_terminal(
    scope: &FacetScope,
    registry: &Registry,
    key: &str,
    context: &Context,
) -> Result<RegisteredOperation> {
    scope
        .registry(registry)?
        .table(Some(context))?
        .get(key)
        .and_then(|entry| entry.operation())
        .cloned()
        .ok_or_else(|| {
            RuntimeError::unavailable("terminal", format!("no terminal registered under {key:?}"))
        })
}

fn host_lifetime() -> ServeOptions {
    ServeOptions {
        lifetime: Lifetime::Host,
        ..Default::default()
    }
}

fn contract_error(error: ContractError) -> RuntimeError {
    RuntimeError::invalid(error.reason, error.message)
}
