use futures::FutureExt;
use serde_json::json;
use std::{cell::RefCell, collections::BTreeMap, error::Error, rc::Rc};
use tembra_core::{
    ConflictPolicy, Host, HostOptions, Implementation, Lifetime, LoadOptions, ManualDriver,
    NativeFacet, Registry, ServeOptions,
    contracts::{Contract, Member},
};
use tembra_hooks::{
    HookArgs, HookDescriptor, HookManager, HookManagerOptions, HookTerminal, HookType,
    MethodHandler, RustHookDefinition, Strategy, TerminalProfile, hook_handler,
};

fn main() -> Result<(), Box<dyn Error>> {
    let driver = ManualDriver::new();
    let host = Host::new(
        HostOptions::new("hooks-example", json!(null)),
        driver.clone(),
    );
    let integer = HookType::<i32>::serde(json!({"type":"integer"}))?;
    let descriptor = HookDescriptor::new(
        "example.calculate",
        1,
        Strategy::Middleware,
        integer.profile().clone(),
        integer.profile().clone(),
        Some(TerminalProfile {
            input: integer.profile().clone(),
            result: integer.profile().clone(),
        }),
    )?;
    let definition =
        RustHookDefinition::from_descriptor(descriptor, integer.clone(), integer.clone())?;
    let terminals = Registry::new("example.terminals", ConflictPolicy::Error)?;
    let token = Rc::new(Contract::local(
        "example.contributions",
        1,
        json!({}),
        BTreeMap::from([
            (
                "intercept".into(),
                definition.descriptor().contribution_member(),
            ),
            (
                "calculate".into(),
                Member::Method {
                    args: integer.profile().clone(),
                    result: integer.profile().clone(),
                },
            ),
        ]),
    )?);
    let captured = Rc::new(RefCell::new(None));
    let saved = captured.clone();
    let (registered, terminal_registry) = (definition.clone(), terminals.clone());
    let facet = NativeFacet::new("example.hooks", vec![], move |scope, setup| {
        *saved.borrow_mut() = Some(scope.clone());
        let (registered, terminal_registry) = (registered.clone(), terminal_registry.clone());
        setup.serve(
            token.clone(),
            Rc::new(move |_, _, declaration| {
                registered.register(declaration, "around-calculation", 0, "intercept")?;
                declaration.register_method(
                    &terminal_registry,
                    "calculate",
                    json!(null),
                    "calculate",
                )?;
                let handler: MethodHandler<HookArgs<i32, i32>, i32> = Rc::new(|args, _| {
                    async move {
                        let (input, next) = args.into_parts();
                        let result = next
                            .expect("middleware receives Next")
                            .call(input + 1)
                            .await?;
                        Ok(result * 2)
                    }
                    .boxed_local()
                });
                let handler = hook_handler(handler);
                Ok(Implementation::new()
                    .method("intercept", move |args: HookArgs<i32, i32>, ctx| {
                        handler(args, ctx)
                    })
                    .method("calculate", |value: i32, _| {
                        async move { Ok(value + 10) }.boxed_local()
                    }))
            }),
            ServeOptions {
                lifetime: Lifetime::Host,
                ..Default::default()
            },
        )?;
        Ok(())
    })?;
    driver.run_until(host.install(facet, LoadOptions::default())?)?;
    let scope = captured.borrow_mut().take().expect("setup captured scope");
    let table = scope.registry(&terminals)?.table(None)?;
    let operation = table
        .get("calculate")
        .and_then(|entry| entry.operation())
        .expect("startup installed the ordinary terminal")
        .clone();
    let terminal = HookTerminal::new(operation, integer.clone(), integer)?;
    let manager = HookManager::new(scope, driver.clone(), HookManagerOptions::default())?;
    let result = driver.run_until(host.root(move |context| {
        manager
            .run_middleware(&definition, 3, &context, terminal)
            .boxed_local()
    }))?;
    assert_eq!(result, 28);
    println!("Result: {result}");
    driver.run_until(host.close())?;
    Ok(())
}
