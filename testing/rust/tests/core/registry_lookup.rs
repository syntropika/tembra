//! Lookup caches preserve registry order, conflict selection and immutable epoch snapshots.
use futures::FutureExt;
use serde_json::{Value, json};
use std::{collections::BTreeMap, rc::Rc};
use tembra_core::{contracts::Contract, *};

fn fixture() -> (Host, Rc<ManualDriver>) {
    let driver = ManualDriver::new();
    let host = Host::new(
        HostOptions::new("registry-lookup", json!(null)),
        driver.clone(),
    );
    (host, driver)
}

fn definition(name: &str, registry: Registry, entries: Vec<(String, Value)>) -> ExecutableFacet {
    let token = Rc::new(Contract::local(name, 1, json!({}), BTreeMap::new()).unwrap());
    ExecutableFacet::foreign(name, vec![], move |_, setup| {
        let registry = registry.clone();
        let entries = entries.clone();
        setup.serve(
            token.clone(),
            Rc::new(move |_, _, declaration| {
                for (key, value) in &entries {
                    declaration.register(&registry, key.clone(), value.clone())?;
                }
                Ok(Implementation::new())
            }),
            ServeOptions {
                lifetime: Lifetime::Host,
                ..Default::default()
            },
        )
    })
    .unwrap()
}

fn entries(count: usize, version: u64, reverse: bool) -> Vec<(String, Value)> {
    let mut values = (0..count)
        .map(|index| (format!("k{index:04}"), json!([version, index])))
        .collect::<Vec<_>>();
    if reverse {
        values.reverse();
    }
    values
}

fn check(table: &RegistrySnapshot, count: usize, version: u64, reverse: bool) {
    assert_eq!(table.code_version(), version);
    assert_eq!(table.len(), count);
    assert_eq!(table.is_empty(), count == 0);
    for index in 0..count {
        assert_eq!(
            table.get(&format!("k{index:04}")).unwrap().data(),
            &json!([version, index])
        );
    }
    let ordered = table
        .entries()
        .map(|(key, entry)| (key.to_owned(), entry.data().clone()))
        .collect::<Vec<_>>();
    assert_eq!(ordered, entries(count, version, reverse));
    for missing in [
        String::new(),
        "k".into(),
        "k00000".into(),
        "Ｋ0000".into(),
        format!("k{count:04}"),
    ] {
        assert!(
            table.get(&missing).is_none(),
            "unexpected lookup for {missing:?}"
        );
    }
}

#[test]
fn small_and_wide_tables_keep_declaration_order_and_exact_missing_key_behavior() {
    for count in [0, 1, 8, 31, 32, 33, 256] {
        let (host, driver) = fixture();
        let registry = Registry::new("ordered", ConflictPolicy::Error).unwrap();
        let view = host.registry(&registry).unwrap();
        assert!(view.table(None).unwrap().get("k0000").is_none());
        driver
            .run_until(
                host.install_executable(
                    definition("owner", registry, entries(count, 1, true)),
                    LoadOptions::default(),
                )
                .unwrap(),
            )
            .unwrap();
        let table = view.table(None).unwrap();
        check(&table, count, 1, true);
        driver.run_until(host.close()).unwrap();
        check(&table, count, 1, true);
    }
}

#[test]
fn immutable_wide_snapshots_survive_reload_shrink_empty_and_epoch_retirement() {
    let (host, driver) = fixture();
    let registry = Registry::new("epochs", ConflictPolicy::Error).unwrap();
    let view = host.registry(&registry).unwrap();
    driver
        .run_until(async {
            host.install_executable(
                definition("owner", registry.clone(), entries(256, 1, true)),
                LoadOptions::default(),
            )?
            .await?;
            let old = view.table(None)?;
            let next = definition("owner", registry.clone(), entries(40, 2, false));
            let selected = view.clone();
            let core = host.clone();
            let old_in_root = old.clone();
            let wide = host
                .root(move |context| {
                    async move {
                        check(&selected.table(Some(&context))?, 256, 1, true);
                        core.reload(vec![next], ReloadOptions::default()).await?;
                        // First-registration ordinals survive a changed factory declaration order.
                        check(&selected.table(None)?, 40, 2, true);
                        check(&selected.table(Some(&context))?, 256, 1, true);
                        check(&old_in_root, 256, 1, true);
                        selected.table(None)
                    }
                    .boxed_local()
                })
                .await?;
            host.reload(
                vec![definition("owner", registry.clone(), entries(8, 3, false))],
                ReloadOptions::default(),
            )
            .await?;
            check(&view.table(None)?, 8, 3, true);
            check(&wide, 40, 2, true);
            check(&old, 256, 1, true);
            host.reload(
                vec![definition("owner", registry, Vec::new())],
                ReloadOptions::default(),
            )
            .await?;
            check(&view.table(None)?, 0, 4, true);
            host.close().await?;
            check(&old, 256, 1, true);
            check(&wide, 40, 2, true);
            Ok::<(), RuntimeError>(())
        })
        .unwrap();
}

#[test]
fn wide_conflict_policies_keep_the_selected_value_and_old_snapshot() {
    for policy in [ConflictPolicy::Error, ConflictPolicy::Last] {
        let (host, driver) = fixture();
        let registry = Registry::new("conflicts", policy).unwrap();
        let view = host.registry(&registry).unwrap();
        driver
            .run_until(async {
                host.install_executable(
                    definition("first", registry.clone(), entries(256, 1, true)),
                    LoadOptions::default(),
                )?
                .await?;
                let prior = view.table(None)?;
                let conflict =
                    definition("second", registry, vec![("k0255".into(), json!([2, 999]))]);
                let installed = host
                    .install_executable(conflict, LoadOptions::default())?
                    .await;
                match policy {
                    ConflictPolicy::Error => {
                        assert_eq!(installed.unwrap_err().reason, "registry_conflict");
                        check(&view.table(None)?, 256, 1, true);
                    }
                    ConflictPolicy::Last => {
                        installed?;
                        let current = view.table(None)?;
                        assert_eq!(current.len(), 256);
                        assert_eq!(current.get("k0255").unwrap().data(), &json!([2, 999]));
                        assert_eq!(current.entries().next().unwrap().0, "k0255");
                        assert!(current.get("k0256").is_none());
                    }
                }
                check(&prior, 256, 1, true);
                host.close().await
            })
            .unwrap();
    }
}
