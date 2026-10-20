use futures::{FutureExt, channel::oneshot};
use serde_json::json;
use std::{cell::RefCell, collections::BTreeMap, rc::Rc};
use tembra_core::{
    contracts::{Contract, Member},
    *,
};

#[test]
fn removal_revokes_public_claims_but_preserves_owned_dependency_for_ordered_cleanup() {
    let driver = ManualDriver::new();
    let host = Host::new(HostOptions::new("removal", json!(null)), driver.clone());
    let a = Rc::new(Contract::local("removal-owner", 1, json!({}), BTreeMap::new()).unwrap());
    let b = Rc::new(
        Contract::local(
            "removal-dependency",
            1,
            json!({}),
            BTreeMap::from([(
                "flush".into(),
                Member::Method {
                    args: None,
                    result: None,
                },
            )]),
        )
        .unwrap(),
    );
    let events = Rc::new(RefCell::new(Vec::new()));
    let (closed, closing) = oneshot::channel();
    let closed = Rc::new(RefCell::new(Some(closed)));
    let b_token = b.clone();
    let observed = events.clone();
    let provider = ExecutableFacet::foreign("removed-provider", Vec::new(), move |_, setup| {
        let observed = observed.clone();
        let closed = closed.clone();
        setup.serve(
            b_token.clone(),
            Rc::new(move |_, _, declaration| {
                let observed_close = observed.clone();
                let closed = closed.clone();
                declaration.resource(
                    "dependency",
                    ResourceDeclaration::new(|_| async { Ok(()) }.boxed_local()).with_close(
                        move |_| {
                            observed_close.borrow_mut().push("dependency-close");
                            if let Some(closed) = closed.borrow_mut().take() {
                                let _ = closed.send(());
                            }
                            async { Ok(()) }.boxed_local()
                        },
                    ),
                )?;
                let observed = observed.clone();
                Ok(
                    Implementation::new().method("flush", move |(): (), context| {
                        assert_eq!(
                            context.code_version(),
                            2,
                            "cleanup uses a real current root"
                        );
                        observed.borrow_mut().push("flush");
                        async { Ok(()) }.boxed_local()
                    }),
                )
            }),
            ServeOptions {
                lifetime: Lifetime::Host,
                ..Default::default()
            },
        )
    })
    .unwrap();
    let a_token = a.clone();
    let b_token = b.clone();
    let observed = events.clone();
    let owner = ExecutableFacet::foreign("remaining-owner", Vec::new(), move |scope, setup| {
        let scope = scope.clone();
        let b_token = b_token.clone();
        let observed = observed.clone();
        setup.serve(
            a_token.clone(),
            Rc::new(move |_, _, declaration| {
                let dependency =
                    declaration.get(b_token.clone(), json!({}), GetOptions::default())?;
                let scope = scope.clone();
                let observed = observed.clone();
                let mut resource = ResourceDeclaration::new(|_| async { Ok(()) }.boxed_local())
                    .with_close(move |_| {
                        let dependency = dependency.clone();
                        let observed = observed.clone();
                        scope.root(move |context| {
                            async move {
                                observed.borrow_mut().push("owner-close");
                                dependency
                                    .call::<(), ()>("flush", (), Some(&context))
                                    .await?;
                                observed.borrow_mut().push("owner-finished");
                                Ok(())
                            }
                            .boxed_local()
                        })
                    });
                // The declared claim is owned by this object independently from public wrappers.
                resource
                    .deps
                    .push(ResourceDependency::Reference(declaration.get(
                        b_token.clone(),
                        json!({}),
                        GetOptions::default(),
                    )?));
                declaration.resource("owner", resource)?;
                Ok(Implementation::new())
            }),
            ServeOptions {
                lifetime: Lifetime::Host,
                ..Default::default()
            },
        )
    })
    .unwrap();
    driver
        .run_until(async {
            host.load_executable(vec![owner, provider], LoadOptions::default())?
                .await?;
            let public_owner = host.get(a, json!({}), GetOptions::default()).await?;
            let public_dependency = host
                .get(b.clone(), json!({}), GetOptions::default())
                .await?;
            host.reload(
                Vec::new(),
                ReloadOptions {
                    remove: vec!["removed-provider".into()],
                    ..Default::default()
                },
            )
            .await?;
            assert_eq!(public_dependency.status(), RefStatus::Gone);
            assert_eq!(public_owner.status(), RefStatus::Gone);
            assert_eq!(
                host.hold(b, json!({}), GetOptions::default())
                    .err()
                    .unwrap()
                    .reason,
                "unknown_type"
            );
            closing.await.unwrap();
            assert_eq!(
                &*events.borrow(),
                &["owner-close", "flush", "owner-finished", "dependency-close"]
            );
            assert!(host.errors().is_empty());
            host.close().await
        })
        .unwrap();
}
