//! Old registry operations keep their installed token across an owner contract bump.
use futures::FutureExt;
use serde_json::json;
use std::{collections::BTreeMap, rc::Rc};
use tembra_core::{
    contracts::{Contract, Member, ValueSchema},
    *,
};

#[test]
fn old_table_invokes_old_owner_contract_after_host_pin_retokens_without_legacy_adapter() {
    let driver = ManualDriver::new();
    let host = Host::new(
        HostOptions::new("table-contract-versions", json!(null)),
        driver.clone(),
    );
    let registry = Registry::new("versioned-operations", ConflictPolicy::Error).unwrap();
    let view = host.registry(&registry).unwrap();
    let definition =
        |version: u64| {
            let registry = registry.clone();
            let contract = Rc::new(
                Contract::local(
                    "versioned-table-owner",
                    version,
                    json!({}),
                    BTreeMap::from([(
                        "read".into(),
                        Member::Method {
                            args: None,
                            result: Some(ValueSchema::Native("versioned-table.value@1".into())),
                        },
                    )]),
                )
                .unwrap(),
            );
            ExecutableFacet::foreign("versioned-table-facet", Vec::new(), move |_, setup| {
                let registry = registry.clone();
                setup.serve(
                    contract.clone(),
                    Rc::new(move |_, _, declaration| {
                        declaration.register_method(&registry, "entry", json!({}), "read")?;
                        Ok(Implementation::new().method("read", move |(): (), context| {
                        async move { Ok((version, context.code_version(), context.root_id())) }
                            .boxed_local()
                    }))
                    }),
                    ServeOptions {
                        lifetime: Lifetime::Host,
                        ..Default::default()
                    },
                )
            })
            .unwrap()
        };
    driver
        .run_until(async {
            host.install_executable(definition(1), LoadOptions::default())?
                .await?;
            let replacement = definition(2);
            let core = host.clone();
            host.root(move |old_context| {
                async move {
                    // Tables retain operations, not external consumer claims. The host pin is
                    // therefore allowed to retoken without a V1 adapter on the V2 provider.
                    let old_table = view.table(Some(&old_context))?;
                    let old = old_table.get("entry").unwrap().operation().unwrap().clone();
                    core.reload(vec![replacement], ReloadOptions::default())
                        .await?;
                    let current_table = view.table(None)?;
                    let current = current_table
                        .get("entry")
                        .unwrap()
                        .operation()
                        .unwrap()
                        .clone();
                    assert_eq!(old.contract().version(), 1);
                    assert_eq!(current.contract().version(), 2);
                    assert_eq!(old_table.code_version(), 1);
                    assert_eq!(current_table.code_version(), 2);
                    assert_eq!(
                        old.call::<(), (u64, u64, u64)>(&old_context, ()).await?,
                        (1, 1, old_context.root_id())
                    );
                    core.root(move |current_context| {
                        async move {
                            assert_eq!(
                                current
                                    .call::<(), (u64, u64, u64)>(&current_context, ())
                                    .await?,
                                (2, 2, current_context.root_id())
                            );
                            Ok(())
                        }
                        .boxed_local()
                    })
                    .await
                }
                .boxed_local()
            })
            .await?;
            host.close().await
        })
        .unwrap();
}
