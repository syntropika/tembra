use std::{cell::RefCell, collections::BTreeMap, rc::Rc};

use futures::FutureExt;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize, Serializer};
use serde_json::json;
use tembra::{Cell, Context, Declaration, Readable, RuntimeError, StateDeclaration, core};

#[derive(Debug, Deserialize, Serialize, JsonSchema, PartialEq)]
struct User {
    name: String,
    age: u32,
}

fn contract(name: &str) -> Rc<tembra::contracts::Contract> {
    Rc::new(tembra::contracts::Contract::local(name, 1, json!({}), BTreeMap::new()).unwrap())
}

fn host() -> (core::Host, Rc<core::ManualDriver>) {
    let driver = core::ManualDriver::new();
    let host = core::Host::new(
        core::HostOptions::new("typed-state", json!(null)),
        driver.clone(),
    );
    (host, driver)
}

#[test]
fn typed_drafts_and_tuple_derivations_preserve_atomic_revisions_and_snapshots() {
    let (host, driver) = host();
    let contract = contract("typed-state.atomic");
    let saved = Rc::new(RefCell::new(None));
    let handles = saved.clone();
    host.serve(
        contract.clone(),
        Rc::new(move |_, _, declaration| {
            let mut declaration = Declaration::from_core(declaration);
            let count = declaration.state("count", 0_u64)?;
            let double = declaration.derive((count.readable(),), |(count,)| {
                if count > 4 {
                    return Err(RuntimeError::invalid(
                        "double",
                        "count exceeds projection limit",
                    ));
                }
                Ok(count * 2)
            })?;
            let combined = declaration
                .derive((count.readable(), double.clone()), |(count, double)| {
                    Ok(count + double)
                })?;
            let user = declaration.state(
                "user",
                User {
                    name: "Alice".into(),
                    age: 30,
                },
            )?;
            *handles.borrow_mut() = Some((count, double, combined, user));
            Ok(core::Implementation::new())
        }),
        core::ServeOptions::default(),
    )
    .unwrap();
    driver
        .run_until(async {
            let reference = host
                .get(contract, json!({}), core::GetOptions::default())
                .await?;
            let (count, double, combined, user) = saved.borrow_mut().take().unwrap();
            let initial_count = count.snapshot()?;
            let initial_user = user.snapshot()?;
            let observations = Rc::new(RefCell::new(Vec::new()));
            let events = observations.clone();
            let peer = double.clone();
            let listener = count.readable().subscribe(move |snapshot| {
                events.borrow_mut().push((snapshot.value()?, peer.value()?));
                Ok(())
            })?;
            host.root(move |ctx| {
                async move {
                    let ctx = Context::from(ctx);
                    count.change(&ctx, |count| {
                        *count += 2;
                        Ok(())
                    })?;
                    assert_eq!(double.value()?, 4);
                    assert_eq!(combined.value()?, 6);
                    let before = count.snapshot()?;
                    let failure = count.change(&ctx, |count| {
                        *count = 3;
                        Err(RuntimeError::invalid("draft", "refuse private draft"))
                    });
                    assert!(failure.is_err());
                    assert!(count.replace(&ctx, 5).is_err());
                    assert!(count.replace(&ctx, u64::MAX).is_err());
                    assert_eq!(count.value()?, 2);
                    assert_eq!(count.snapshot()?.revision(), before.revision());
                    assert_eq!(double.value()?, 4);
                    assert_eq!(*observations.borrow(), [(0, 0), (2, 4)]);
                    user.change(&ctx, |user| {
                        user.age += 1;
                        Ok(())
                    })?;
                    assert_eq!(user.value()?.age, 31);
                    assert_eq!(initial_user.value()?.age, 30);
                    assert_eq!(initial_count.value()?, 0);

                    let prepared = count.prepare_change(&ctx, |count| {
                        *count += 1;
                        Ok(())
                    })?;
                    // Accepted no-ops still invalidate competing preparations.
                    count.replace(&ctx, 2)?;
                    assert!(count.adopt(&ctx, &prepared).is_err());
                    let prepared = count.prepare_change(&ctx, |count| {
                        *count += 1;
                        Ok(())
                    })?;
                    prepared.abort();
                    prepared.abort();
                    assert!(count.adopt(&ctx, &prepared).is_err());
                    assert_eq!(count.value()?, 2);
                    Ok(())
                }
                .boxed_local()
            })
            .await?;
            drop(listener);
            reference.release();
            host.close().await
        })
        .unwrap();
}

#[derive(Deserialize, JsonSchema)]
struct Checked {
    amount: u64,
}

impl Serialize for Checked {
    fn serialize<S: Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        if self.amount == 13 {
            return Err(serde::ser::Error::custom("refused test value"));
        }
        #[derive(Serialize)]
        struct Fields {
            amount: u64,
        }
        Fields {
            amount: self.amount,
        }
        .serialize(serializer)
    }
}

#[derive(Serialize, JsonSchema)]
struct Decoded {
    amount: u64,
}

impl<'de> Deserialize<'de> for Decoded {
    fn deserialize<D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> std::result::Result<Self, D::Error> {
        #[derive(Deserialize)]
        struct Fields {
            amount: u64,
        }
        let fields = Fields::deserialize(deserializer)?;
        if fields.amount == 13 {
            return Err(serde::de::Error::custom("refused decoded test value"));
        }
        Ok(Self {
            amount: fields.amount,
        })
    }
}

#[test]
fn conversion_and_nonfinite_derived_output_refuse_the_complete_preparation() {
    let (host, driver) = host();
    let contract = contract("typed-state.conversions");
    let saved = Rc::new(RefCell::new(None));
    let handles = saved.clone();
    host.serve(
        contract.clone(),
        Rc::new(move |_, _, declaration| {
            let mut declaration = Declaration::from_core(declaration);
            let checked = declaration.state("checked", Checked { amount: 1 })?;
            let decoded = declaration.state("decoded", Decoded { amount: 1 })?;
            let count = declaration.state("count", 0_u64)?;
            let projection = declaration.derive((count.readable(),), |(count,)| {
                Ok(if count == 2 { f64::NAN } else { count as f64 })
            })?;
            *handles.borrow_mut() = Some((checked, decoded, count, projection));
            Ok(core::Implementation::new())
        }),
        core::ServeOptions::default(),
    )
    .unwrap();
    driver
        .run_until(async {
            let reference = host
                .get(contract, json!({}), core::GetOptions::default())
                .await?;
            let (checked, decoded, count, projection) = saved.borrow_mut().take().unwrap();
            host.root(move |ctx| {
                async move {
                    let ctx = Context::from(ctx);
                    let checked_before = checked.snapshot()?;
                    let count_before = count.snapshot()?;
                    let events = Rc::new(RefCell::new(Vec::new()));
                    let log = events.clone();
                    let listener = count.readable().subscribe(move |snapshot| {
                        log.borrow_mut().push(snapshot.value()?);
                        Ok(())
                    })?;
                    let error = checked
                        .change(&ctx, |value| {
                            value.amount = 13;
                            Ok(())
                        })
                        .unwrap_err();
                    assert_eq!(error.reason, "serialization");
                    assert_eq!(checked.value()?.amount, 1);
                    assert_eq!(checked.snapshot()?.revision(), checked_before.revision());
                    let decoded_before = decoded.snapshot()?;
                    let decoded_events = Rc::new(RefCell::new(Vec::new()));
                    let log = decoded_events.clone();
                    let decoded_listener = decoded.readable().subscribe(move |snapshot| {
                        log.borrow_mut().push(snapshot.value()?.amount);
                        Ok(())
                    })?;
                    let error = decoded
                        .change(&ctx, |value| {
                            value.amount = 13;
                            Ok(())
                        })
                        .unwrap_err();
                    assert_eq!(error.reason, "state_decode");
                    assert_eq!(decoded.value()?.amount, 1);
                    assert_eq!(decoded.snapshot()?.revision(), decoded_before.revision());
                    assert_eq!(*decoded_events.borrow(), [1]);
                    let error = count.replace(&ctx, 2).unwrap_err();
                    assert_eq!(error.reason, "serialization");
                    assert_eq!(count.value()?, 0);
                    assert_eq!(projection.value()?, 0.0);
                    assert_eq!(count.snapshot()?.revision(), count_before.revision());
                    assert_eq!(*events.borrow(), [0]);
                    drop(listener);
                    drop(decoded_listener);
                    Ok(())
                }
                .boxed_local()
            })
            .await?;
            reference.release();
            host.close().await
        })
        .unwrap();
}

#[test]
fn retained_typed_readables_do_not_keep_the_reference_claim_alive() {
    let (host, driver) = host();
    let contract = Rc::new(
        tembra::contracts::Contract::local(
            "typed-state.weak",
            1,
            json!({}),
            BTreeMap::from([(
                "count".into(),
                tembra::contracts::Member::State {
                    schema: tembra::contracts::schema_for::<u64>().unwrap(),
                },
            )]),
        )
        .unwrap(),
    );
    host.serve(
        contract.clone(),
        Rc::new(|_, _, declaration| {
            let count = Declaration::from_core(declaration).state("count", 7_u64)?;
            Ok(core::Implementation::new().state("count", count.readable().core()?))
        }),
        core::ServeOptions::default(),
    )
    .unwrap();
    let (readable, snapshot) = driver
        .run_until(async {
            let reference = host
                .get(contract, json!({}), core::GetOptions::default())
                .await?;
            let readable = Readable::<u64>::from_reference(reference.clone(), "count");
            let snapshot = readable.snapshot()?;
            assert_eq!(readable.value()?, 7);
            drop(reference);
            Ok::<_, RuntimeError>((readable, snapshot))
        })
        .unwrap();
    driver.run_until_stalled();
    assert!(readable.value().is_err());
    assert_eq!(snapshot.value().unwrap(), 7);
    driver.run_until(host.close()).unwrap();
}

#[derive(Debug, Deserialize, Serialize, JsonSchema, PartialEq)]
struct PreviousUser {
    name: String,
}

fn user_facet(
    contract: Rc<tembra::contracts::Contract>,
    migrated: Rc<RefCell<Option<Cell<User>>>>,
    refuse: bool,
) -> core::ExecutableFacet {
    core::ExecutableFacet::foreign("typed-users", Vec::new(), move |_, setup| {
        let migrated = migrated.clone();
        setup.serve(
            contract.clone(),
            Rc::new(move |_, _, declaration| {
                let state = StateDeclaration::new(User {
                    name: "ignored replacement initializer".into(),
                    age: 0,
                })?
                .version(2)
                .with_migrate::<PreviousUser>(move |previous, version| {
                    assert_eq!(version, 1);
                    if refuse {
                        return Err(RuntimeError::invalid("migration", "refuse candidate"));
                    }
                    Ok(User {
                        name: previous.name,
                        age: 30,
                    })
                });
                let user = Declaration::from_core(declaration).state_with("user", state)?;
                *migrated.borrow_mut() = Some(user);
                Ok(core::Implementation::new())
            }),
            core::ServeOptions::default(),
        )
    })
    .unwrap()
}

#[test]
fn typed_migration_failure_preserves_authority_and_success_uses_the_old_rust_shape() {
    let (host, driver) = host();
    let contract = contract("typed-state.migration");
    let saved = Rc::new(RefCell::new(None));
    let handles = saved.clone();
    let initial_contract = contract.clone();
    let initial = core::ExecutableFacet::foreign("typed-users", Vec::new(), move |_, setup| {
        let handles = handles.clone();
        setup.serve(
            initial_contract.clone(),
            Rc::new(move |_, _, declaration| {
                let user = Declaration::from_core(declaration).state(
                    "user",
                    PreviousUser {
                        name: "Alice".into(),
                    },
                )?;
                *handles.borrow_mut() = Some(user);
                Ok(core::Implementation::new())
            }),
            core::ServeOptions::default(),
        )
    })
    .unwrap();
    driver
        .run_until(async {
            host.install_executable(initial, core::LoadOptions::default())?
                .await?;
            let reference = host
                .get(contract.clone(), json!({}), core::GetOptions::default())
                .await?;
            let original = saved.borrow_mut().take().unwrap();
            let snapshot = original.snapshot()?;
            let migrated = Rc::new(RefCell::new(None));
            let rejected = user_facet(contract.clone(), migrated.clone(), true);
            assert!(
                host.reload(vec![rejected], core::ReloadOptions::default())
                    .await
                    .is_err()
            );
            assert_eq!(original.value()?.name, "Alice");
            assert_eq!(original.snapshot()?.revision(), snapshot.revision());
            assert_eq!(original.version(), 1);
            let accepted = user_facet(contract, migrated.clone(), false);
            host.reload(vec![accepted], core::ReloadOptions::default())
                .await?;
            let user = migrated.borrow_mut().take().unwrap();
            assert_eq!(user.version(), 2);
            assert_eq!(user.value()?.name, "Alice");
            assert_eq!(user.value()?.age, 30);
            assert_eq!(snapshot.value()?.name, "Alice");
            reference.release();
            host.close().await
        })
        .unwrap();
}

#[test]
fn typed_async_initialization_remains_managed_by_core_activation() {
    let (host, driver) = host();
    let contract = contract("typed-state.initializer");
    let saved = Rc::new(RefCell::new(None));
    let handles = saved.clone();
    host.serve(
        contract.clone(),
        Rc::new(move |_, _, declaration| {
            let state = StateDeclaration::with_initializer(|ctx| async move {
                assert!(ctx.core().root_id() > 0);
                Ok(42_u64)
            })?;
            let count = Declaration::from_core(declaration).state_with("count", state)?;
            assert!(count.value().is_err());
            *handles.borrow_mut() = Some(count);
            Ok(core::Implementation::new())
        }),
        core::ServeOptions::default(),
    )
    .unwrap();
    driver
        .run_until(async {
            let reference = host
                .get(contract, json!({}), core::GetOptions::default())
                .await?;
            assert_eq!(saved.borrow().as_ref().unwrap().value()?, 42);
            reference.release();
            host.close().await
        })
        .unwrap();
}

// Deliberately lacks JsonSchema, as DTOs emitted from a portable descriptor may do.
#[derive(Debug, Deserialize, Serialize, PartialEq)]
struct DescriptorState {
    amount: u64,
}

fn descriptor_state_schema(maximum: u64) -> serde_json::Value {
    json!({
        "type": "object",
        "properties": {
            "amount": {"type": "integer", "minimum": 0, "maximum": maximum}
        },
        "required": ["amount"],
        "additionalProperties": false
    })
}

fn descriptor_state_facet(
    contract: Rc<tembra::contracts::Contract>,
    handles: Rc<RefCell<Option<Cell<DescriptorState>>>>,
    version: u64,
    migrated_amount: u64,
) -> core::ExecutableFacet {
    core::ExecutableFacet::foreign("descriptor-state", Vec::new(), move |_, setup| {
        let handles = handles.clone();
        setup.serve(
            contract.clone(),
            Rc::new(move |_, _, declaration| {
                let state = if version == 1 {
                    StateDeclaration::with_schema_initializer(
                        descriptor_state_schema(10),
                        |_| async { Ok(DescriptorState { amount: 6 }) },
                    )?
                } else {
                    StateDeclaration::with_schema(
                        descriptor_state_schema(20),
                        DescriptorState { amount: 0 },
                    )?
                    .version(version)
                    .with_migrate::<DescriptorState>(
                        move |old, old_version| {
                            assert_eq!(old_version, 1);
                            assert_eq!(old.amount, 6);
                            Ok(DescriptorState {
                                amount: migrated_amount,
                            })
                        },
                    )
                };
                let state = Declaration::from_core(declaration).state_with("state", state)?;
                *handles.borrow_mut() = Some(state);
                Ok(core::Implementation::new())
            }),
            core::ServeOptions::default(),
        )
    })
    .unwrap()
}

#[test]
fn descriptor_schema_state_without_json_schema_checks_initialization_and_atomic_migration() {
    let schema = descriptor_state_schema(10);
    assert!(StateDeclaration::with_schema(schema.clone(), DescriptorState { amount: 10 }).is_ok());
    assert!(StateDeclaration::with_schema(schema.clone(), DescriptorState { amount: 11 }).is_err());
    assert!(StateDeclaration::with_schema(schema, DescriptorState { amount: u64::MAX }).is_err());
    assert!(
        StateDeclaration::with_schema(
            json!({"$ref": "#/$defs/unsupported"}),
            DescriptorState { amount: 1 },
        )
        .is_err()
    );
    assert!(
        StateDeclaration::with_schema_initializer(
            json!({"$ref": "#/$defs/unsupported"}),
            |_| async { Ok(DescriptorState { amount: 1 }) },
        )
        .is_err()
    );
    let (host, driver) = host();
    let contract = contract("typed-state.descriptor");
    let handles = Rc::new(RefCell::new(None));
    let initial = descriptor_state_facet(contract.clone(), handles.clone(), 1, 0);
    driver
        .run_until(async {
            host.install_executable(initial, core::LoadOptions::default())?
                .await?;
            let reference = host
                .get(contract.clone(), json!({}), core::GetOptions::default())
                .await?;
            let original = handles.borrow_mut().take().unwrap();
            let before = original.snapshot()?;
            assert_eq!(before.value()?, DescriptorState { amount: 6 });
            let events = Rc::new(RefCell::new(Vec::new()));
            let captured = events.clone();
            let listener = original.readable().subscribe(move |snapshot| {
                captured.borrow_mut().push(snapshot.value()?.amount);
                Ok(())
            })?;
            let refused = descriptor_state_facet(contract.clone(), handles.clone(), 2, 21);
            assert!(
                host.reload(vec![refused], core::ReloadOptions::default())
                    .await
                    .is_err()
            );
            assert_eq!(original.value()?.amount, 6);
            assert_eq!(original.snapshot()?.revision(), before.revision());
            assert_eq!(original.version(), 1);
            assert_eq!(*events.borrow(), [6]);
            let accepted = descriptor_state_facet(contract, handles.clone(), 2, 15);
            host.reload(vec![accepted], core::ReloadOptions::default())
                .await?;
            let migrated = handles.borrow_mut().take().unwrap();
            assert_eq!(migrated.version(), 2);
            assert_eq!(migrated.value()?.amount, 15);
            assert_eq!(before.value()?.amount, 6);
            drop(listener);
            reference.release();
            host.close().await
        })
        .unwrap();
}

#[test]
fn descriptor_schema_initializer_refusal_never_publishes_an_invalid_binding() {
    let (host, driver) = host();
    let contract = Rc::new(
        tembra::contracts::Contract::local(
            "typed-state.descriptor-initializer",
            1,
            json!({}),
            BTreeMap::from([(
                "state".into(),
                tembra::contracts::Member::State {
                    schema: descriptor_state_schema(10),
                },
            )]),
        )
        .unwrap(),
    );
    host.serve(
        contract.clone(),
        Rc::new(|_, _, declaration| {
            let state = StateDeclaration::with_schema_initializer(
                descriptor_state_schema(10),
                |_| async { Ok(DescriptorState { amount: 11 }) },
            )?;
            let state = Declaration::from_core(declaration).state_with("state", state)?;
            Ok(core::Implementation::new().state("state", state.readable().core()?))
        }),
        core::ServeOptions::default(),
    )
    .unwrap();
    let reference = host
        .hold(contract, json!({}), core::GetOptions::default())
        .unwrap();
    let readable = Readable::<DescriptorState>::from_reference(reference.clone(), "state");
    let events = Rc::new(RefCell::new(Vec::new()));
    let captured = events.clone();
    let listener = readable
        .subscribe(move |snapshot| {
            captured.borrow_mut().push(snapshot.value()?.amount);
            Ok(())
        })
        .unwrap();
    assert!(driver.run_until(reference.ready()).is_err());
    assert!(events.borrow().is_empty());
    assert!(readable.value().is_err());
    drop(listener);
    reference.release();
    driver.run_until(host.close()).unwrap();
}

#[test]
fn descriptor_schema_projection_without_json_schema_refuses_the_complete_mutation() {
    let (host, driver) = host();
    let contract = contract("typed-state.descriptor-projection");
    let saved = Rc::new(RefCell::new(None));
    let handles = saved.clone();
    let invalid_calls = Rc::new(std::cell::Cell::new(0));
    let calls = invalid_calls.clone();
    host.serve(
        contract.clone(),
        Rc::new(move |_, _, declaration| {
            let mut declaration = Declaration::from_core(declaration);
            let source = declaration.state("source", 0_u64)?;
            let calls = calls.clone();
            let mut invalid_schema = descriptor_state_schema(10);
            invalid_schema["default"] = json!({"amount": 11});
            assert!(
                declaration
                    .derive_with_schema((source.readable(),), invalid_schema, move |(amount,)| {
                        calls.set(calls.get() + 1);
                        Ok(DescriptorState { amount })
                    })
                    .is_err()
            );
            let projection = declaration.derive_with_schema(
                (source.readable(),),
                descriptor_state_schema(10),
                |(source,)| Ok(DescriptorState { amount: source * 2 }),
            )?;
            *handles.borrow_mut() = Some((source, projection));
            Ok(core::Implementation::new())
        }),
        core::ServeOptions::default(),
    )
    .unwrap();
    driver
        .run_until(async {
            let reference = host
                .get(contract, json!({}), core::GetOptions::default())
                .await?;
            assert_eq!(invalid_calls.get(), 0);
            let (source, projection) = saved.borrow_mut().take().unwrap();
            host.root(move |context| {
                async move {
                    let context = Context::from(context);
                    source.replace(&context, 2)?;
                    assert_eq!(projection.value()?, DescriptorState { amount: 4 });
                    let source_before = source.snapshot()?;
                    let projection_before = projection.snapshot()?;
                    let source_events = Rc::new(RefCell::new(Vec::new()));
                    let log = source_events.clone();
                    let source_listener = source.readable().subscribe(move |snapshot| {
                        log.borrow_mut().push(snapshot.value()?);
                        Ok(())
                    })?;
                    let projection_events = Rc::new(RefCell::new(Vec::new()));
                    let log = projection_events.clone();
                    let projection_listener = projection.subscribe(move |snapshot| {
                        log.borrow_mut().push(snapshot.value()?.amount);
                        Ok(())
                    })?;
                    assert!(source.replace(&context, 6).is_err());
                    assert_eq!(source.value()?, 2);
                    assert_eq!(source.snapshot()?.revision(), source_before.revision());
                    assert_eq!(projection.value()?.amount, 4);
                    assert_eq!(
                        projection.snapshot()?.revision(),
                        projection_before.revision()
                    );
                    assert_eq!(*source_events.borrow(), [2]);
                    assert_eq!(*projection_events.borrow(), [4]);
                    drop(source_listener);
                    drop(projection_listener);
                    Ok(())
                }
                .boxed_local()
            })
            .await?;
            reference.release();
            host.close().await
        })
        .unwrap();
}

#[derive(Serialize, JsonSchema)]
struct OwnershipSensitive(String);

impl<'de> Deserialize<'de> for OwnershipSensitive {
    fn deserialize<D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> std::result::Result<Self, D::Error> {
        struct StringVisitor;
        impl serde::de::Visitor<'_> for StringVisitor {
            type Value = OwnershipSensitive;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("a string whose ownership is observable")
            }

            fn visit_string<E: serde::de::Error>(
                self,
                value: String,
            ) -> std::result::Result<Self::Value, E> {
                Ok(OwnershipSensitive(value))
            }

            fn visit_str<E: serde::de::Error>(
                self,
                value: &str,
            ) -> std::result::Result<Self::Value, E> {
                Ok(OwnershipSensitive(format!("borrowed:{value}")))
            }
        }
        deserializer.deserialize_string(StringVisitor)
    }
}

#[test]
fn private_draft_decoding_preserves_owned_visitors_and_refuses_without_publication() {
    let (host, driver) = host();
    let contract = contract("typed-state.owned-draft");
    let saved = Rc::new(RefCell::new(None));
    let handles = saved.clone();
    host.serve(
        contract.clone(),
        Rc::new(move |_, _, declaration| {
            let mut declaration = Declaration::from_core(declaration);
            let visitor = declaration.state("visitor", OwnershipSensitive("seed".into()))?;
            let zero = declaration.state("zero", -0.0_f64)?;
            // A portable provider can supply schema-valid data refused by custom Serde.
            let raw = declaration.core_mut().state(
                "refused",
                core::StateDeclaration::new(
                    tembra::contracts::schema_for::<Decoded>().unwrap(),
                    json!({"amount": 13}),
                ),
            )?;
            let refused = Cell::<Decoded>::from_core(raw);
            *handles.borrow_mut() = Some((visitor, zero, refused));
            Ok(core::Implementation::new())
        }),
        core::ServeOptions::default(),
    )
    .unwrap();
    driver
        .run_until(async {
            let reference = host
                .get(contract, json!({}), core::GetOptions::default())
                .await?;
            let (visitor, zero, refused) = saved.borrow_mut().take().unwrap();
            host.root(move |context| {
                async move {
                    let context = Context::from(context);
                    assert_eq!(visitor.value()?.0, "seed");
                    let initial = visitor.snapshot()?;
                    let events = Rc::new(RefCell::new(Vec::new()));
                    let log = events.clone();
                    let listener = visitor.readable().subscribe(move |snapshot| {
                        log.borrow_mut().push(snapshot.value()?.0);
                        Ok(())
                    })?;
                    visitor.change(&context, |value| {
                        value.0.push('!');
                        Ok(())
                    })?;
                    assert_eq!(visitor.value()?.0, "seed!");
                    assert_eq!(initial.value()?.0, "seed");
                    let committed = visitor.snapshot()?;
                    let error = visitor
                        .change(&context, |value| {
                            value.0.push_str(" refused");
                            Err(RuntimeError::invalid("draft", "refuse changed candidate"))
                        })
                        .unwrap_err();
                    assert_eq!(error.reason, "draft");
                    assert_eq!(visitor.value()?.0, "seed!");
                    assert_eq!(visitor.snapshot()?.revision(), committed.revision());
                    assert_eq!(*events.borrow(), ["seed", "seed!"]);

                    let before = refused.core().snapshot()?;
                    let failed_events = Rc::new(RefCell::new(Vec::new()));
                    let log = failed_events.clone();
                    let failed_listener = refused.core().readable().subscribe(move |snapshot| {
                        log.borrow_mut().push(snapshot.value().clone());
                        Ok(())
                    })?;
                    let callback_calls = std::cell::Cell::new(0);
                    let error = refused
                        .change(&context, |_| {
                            callback_calls.set(callback_calls.get() + 1);
                            Ok(())
                        })
                        .unwrap_err();
                    assert_eq!(error.reason, "state_decode");
                    assert_eq!(callback_calls.get(), 0);
                    assert_eq!(refused.core().snapshot()?.value(), before.value());
                    assert_eq!(refused.snapshot()?.revision(), before.revision());
                    assert_eq!(*failed_events.borrow(), [json!({"amount": 13})]);

                    assert!(!zero.value()?.is_sign_negative());
                    let zero_before = zero.snapshot()?;
                    let zero_events = Rc::new(RefCell::new(Vec::new()));
                    let log = zero_events.clone();
                    let zero_listener = zero.readable().subscribe(move |snapshot| {
                        log.borrow_mut().push(snapshot.value()?.to_bits());
                        Ok(())
                    })?;
                    zero.replace(&context, -0.0)?;
                    assert_eq!(zero.snapshot()?.revision(), zero_before.revision());
                    assert_eq!(*zero_events.borrow(), [0_f64.to_bits()]);
                    drop((listener, failed_listener, zero_listener));
                    Ok(())
                }
                .boxed_local()
            })
            .await?;
            reference.release();
            host.close().await
        })
        .unwrap();
}
