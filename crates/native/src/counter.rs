//! A reusable remote counter for transport and real-browser acceptance.

use std::{collections::BTreeMap, rc::Rc};

use futures::FutureExt;
use serde_json::{Value, json};
use tembra_core::{
    Host, Implementation, ReplaceOptions, Result, RuntimeError, ServeOptions, StateDeclaration,
    contracts::{Contract, Member, Placement, ValueSchema},
};

/// Authors the named-server counter contract used by the native/browser fixture.
///
/// # Errors
/// Returns the descriptor error if this fixture's portable schema is invalid.
pub fn counter_contract() -> tembra_core::contracts::Result<Contract> {
    let integer = json!({"type":"integer"});
    let nullable = json!({"anyOf":[{"type":"integer"},{"type":"null"}]});
    let method = |args: Option<Value>, result: Option<Value>| Member::Method {
        args: args.map(ValueSchema::Portable),
        result: result.map(ValueSchema::Portable),
    };
    Contract::remote(
        "tembra.counter",
        1,
        json!({}),
        Placement::Named {
            name: "server".into(),
        },
        BTreeMap::from([(
            "default".into(),
            BTreeMap::from([
                (
                    "count".into(),
                    Member::State {
                        schema: integer.clone(),
                    },
                ),
                (
                    "double".into(),
                    Member::State {
                        schema: integer.clone(),
                    },
                ),
                (
                    "wait_started".into(),
                    Member::State {
                        schema: json!({"type":"boolean"}),
                    },
                ),
                ("add".into(), method(Some(integer.clone()), Some(integer))),
                (
                    "nullable".into(),
                    method(Some(nullable.clone()), Some(nullable)),
                ),
                ("void".into(), method(None, None)),
                ("who".into(), method(None, Some(json!({"type":"string"})))),
                ("wait".into(), method(None, None)),
            ]),
        )]),
    )
}

/// Serves the actual Rust counter and returns its validated consumer contract.
///
/// `who` exposes the factory's accepted principal, independently of the Host's
/// self principal. `wait` cooperatively settles only after call cancellation.
///
/// # Errors
/// Refuses invalid fixture declarations, conflicting registration or Host state.
pub fn serve_counter(host: &Host) -> Result<Rc<Contract>> {
    let contract = Rc::new(
        counter_contract()
            .map_err(|error| RuntimeError::invalid("fixture_contract", error.to_string()))?,
    );
    host.serve(
        contract.clone(),
        Rc::new(|_, principal, declaration| {
            let count = declaration.state(
                "count",
                StateDeclaration::new(json!({"type":"integer"}), json!(0)),
            )?;
            let double = declaration.derive(
                vec![count.readable()],
                json!({"type":"integer"}),
                |values| {
                    let doubled = values[0]
                        .value()
                        .as_i64()
                        .unwrap()
                        .checked_mul(2)
                        .ok_or_else(|| {
                            RuntimeError::invalid("counter_overflow", "counter double overflowed")
                        })?;
                    Ok(json!(doubled))
                },
            )?;
            let wait_started = declaration.state(
                "wait_started",
                StateDeclaration::new(json!({"type":"boolean"}), json!(false)),
            )?;
            Ok(Implementation::new()
                .variant("default")
                .state("count", count.readable())
                .state("double", double)
                .state("wait_started", wait_started.readable())
                .json_method("add", move |argument, context| {
                    let count = count.clone();
                    async move {
                        let by = argument.and_then(|value| value.as_i64()).ok_or_else(|| {
                            RuntimeError::invalid("counter_argument", "add requires an integer")
                        })?;
                        let snapshot = count.change(&context, |draft| {
                            let next =
                                draft.as_i64().unwrap().checked_add(by).ok_or_else(|| {
                                    RuntimeError::invalid(
                                        "counter_overflow",
                                        "counter addition overflowed",
                                    )
                                })?;
                            *draft = json!(next);
                            Ok(())
                        })?;
                        Ok(Some(snapshot.value().clone()))
                    }
                    .boxed_local()
                })
                .json_method("nullable", |argument, _| {
                    async move { Ok(argument) }.boxed_local()
                })
                .json_method("void", |_, _| async { Ok(None) }.boxed_local())
                .json_method("who", move |_, _| {
                    let principal = principal.clone();
                    async move { Ok(Some(principal)) }.boxed_local()
                })
                .json_method("wait", move |_, context| {
                    let wait_started = wait_started.clone();
                    async move {
                        wait_started.replace(&context, json!(true), ReplaceOptions::default())?;
                        Err(context.cancellation().cancelled().await)
                    }
                    .boxed_local()
                }))
        }),
        ServeOptions::default(),
    )?;
    Ok(contract)
}
