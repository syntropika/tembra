use futures::FutureExt;
use serde_json::json;
use std::{cell::RefCell, collections::BTreeMap, rc::Rc};
use tembra_core::{
    contracts::{Contract, Member, ValueSchema},
    *,
};

type Catalog = Vec<(String, FacetKind)>;

fn catalog(host: &Host) -> Catalog {
    host.installed_facets()
        .into_iter()
        .map(|facet| (facet.name().to_owned(), facet.kind()))
        .collect()
}

fn token() -> Rc<Contract> {
    Rc::new(
        Contract::local(
            "catalog-service",
            1,
            json!({}),
            BTreeMap::from([(
                "read".into(),
                Member::Method {
                    args: None,
                    result: Some(ValueSchema::Portable(json!({"type":"integer"}))),
                },
            )]),
        )
        .unwrap(),
    )
}

fn provider(name: &str, contract: Rc<Contract>, value: u64, refuse: bool) -> ExecutableFacet {
    ExecutableFacet::foreign(name, vec![], move |_, setup| {
        setup.serve(
            contract.clone(),
            Rc::new(move |_, _, _| {
                if refuse {
                    return Err(RuntimeError::invalid(
                        "candidate_activation",
                        "activation refused",
                    ));
                }
                Ok(Implementation::new().json_method("read", move |_, _| {
                    async move { Ok(Some(json!(value))) }.boxed_local()
                }))
            }),
            ServeOptions {
                lifetime: Lifetime::Host,
                ..Default::default()
            },
        )
    })
    .unwrap()
}

#[test]
fn partial_synchronous_failure_reports_only_surviving_admissions() -> Result<()> {
    let driver = ManualDriver::new();
    let host = Host::new(HostOptions::new("catalog", json!(null)), driver.clone());
    let native = NativeFacet::new("zeta", vec![], |_, _| Ok(()))?;
    let foreign = ExecutableFacet::foreign("alpha", vec![], |_, _| Ok(()))?;
    let broken = ExecutableFacet::foreign("broken", vec![], |_, _| {
        Err(RuntimeError::invalid("setup_refused", "setup refused"))
    })?;
    let skipped = ExecutableFacet::foreign("skipped", vec![], |_, _| Ok(()))?;
    assert!(host.installed_facets().is_empty());
    let error = host
        .load_executable(
            vec![native.into(), foreign, broken, skipped],
            LoadOptions::default(),
        )
        .err()
        .expect("later setup refuses the initial batch");
    assert_eq!(error.reason, "setup_refused");
    assert_eq!(
        catalog(&host),
        vec![
            ("alpha".into(), FacetKind::Foreign),
            ("zeta".into(), FacetKind::Native),
        ]
    );
    let admitted = ExecutableFacet::foreign("alpha", vec![], |_, _| Ok(()))?;
    assert_eq!(
        host.install_executable(admitted, LoadOptions::default())
            .err()
            .unwrap()
            .reason,
        "duplicate_facet"
    );
    let withdrawn = ExecutableFacet::foreign("broken", vec![], |_, _| Ok(()))?;
    driver.run_until(host.install_executable(withdrawn, LoadOptions::default())?)?;
    let snapshot = host.installed_facets();
    driver.run_until(host.close())?;
    assert!(host.installed_facets().is_empty());
    assert_eq!(snapshot.len(), 3);
    assert_eq!(snapshot[0].name(), "alpha");
    Ok(())
}

#[test]
fn eager_failure_does_not_erase_an_admitted_declaration() -> Result<()> {
    let driver = ManualDriver::new();
    let host = Host::new(HostOptions::new("catalog", json!(null)), driver.clone());
    let startup = host.install_executable(
        provider("failing", token(), 0, true),
        LoadOptions::default(),
    )?;
    assert_eq!(catalog(&host), vec![("failing".into(), FacetKind::Foreign)]);
    assert_eq!(
        host.install_executable(
            provider("failing", token(), 0, false),
            LoadOptions::default()
        )
        .err()
        .unwrap()
        .reason,
        "duplicate_facet"
    );
    assert_eq!(
        driver.run_until(startup).unwrap_err().reason,
        "candidate_activation"
    );
    assert_eq!(catalog(&host), vec![("failing".into(), FacetKind::Foreign)]);
    driver.run_until(host.close())?;
    assert!(host.installed_facets().is_empty());
    Ok(())
}

#[test]
fn rejected_candidates_stay_private_and_cross_facet_commit_changes_the_catalog() -> Result<()> {
    let driver = ManualDriver::new();
    let host = Host::new(HostOptions::new("catalog", json!(null)), driver.clone());
    let contract = token();
    driver.run_until(async {
        host.install_executable(
            provider("provider-a", contract.clone(), 1, false),
            LoadOptions::default(),
        )?
        .await?;
        let reference = host
            .get(contract.clone(), json!({}), GetOptions::default())
            .await?;
        let id = reference.object_id();
        let snapshot = host.installed_facets();
        let observed = Rc::new(RefCell::new(Vec::new()));
        let observe = observed.clone();
        let queried = host.clone();
        let rejected = ExecutableFacet::foreign("provider-b", vec![], move |_, _| {
            *observe.borrow_mut() = catalog(&queried);
            Err(RuntimeError::invalid("setup_refused", "candidate refused"))
        })?;
        let error = host
            .reload(
                vec![rejected],
                ReloadOptions {
                    remove: vec!["provider-a".into()],
                    ..Default::default()
                },
            )
            .await
            .unwrap_err();
        assert_eq!(error.reason, "setup_refused");
        assert_eq!(
            &*observed.borrow(),
            &[("provider-a".into(), FacetKind::Foreign)]
        );
        assert_eq!(
            catalog(&host),
            vec![("provider-a".into(), FacetKind::Foreign)]
        );
        let error = host
            .reload(
                vec![provider("provider-b", contract.clone(), 2, true)],
                ReloadOptions {
                    remove: vec!["provider-a".into()],
                    ..Default::default()
                },
            )
            .await
            .unwrap_err();
        assert_eq!(error.reason, "candidate_activation");
        assert_eq!(
            catalog(&host),
            vec![("provider-a".into(), FacetKind::Foreign)]
        );
        assert_eq!(
            reference.call_json("read", None, None).await?,
            Some(json!(1))
        );
        let queried = Rc::new(RefCell::new(Some(host.clone())));
        let queried_factory = queried.clone();
        let before_commit = Rc::new(RefCell::new(None));
        let recorded = before_commit.clone();
        let next_token = contract.clone();
        let successful = ExecutableFacet::foreign("provider-b", vec![], move |_, setup| {
            let queried = queried_factory.clone();
            let recorded = recorded.clone();
            setup.serve(
                next_token.clone(),
                Rc::new(move |_, _, _| {
                    *recorded.borrow_mut() = Some(catalog(queried.borrow().as_ref().unwrap()));
                    Ok(Implementation::new()
                        .json_method("read", |_, _| async { Ok(Some(json!(2))) }.boxed_local()))
                }),
                ServeOptions {
                    lifetime: Lifetime::Host,
                    ..Default::default()
                },
            )
        })?;
        let report = host
            .reload(
                vec![successful],
                ReloadOptions {
                    remove: vec!["provider-a".into()],
                    ..Default::default()
                },
            )
            .await?;
        queried.borrow_mut().take();
        assert_eq!(
            before_commit.borrow().as_ref(),
            Some(&vec![("provider-a".into(), FacetKind::Foreign)])
        );
        assert_eq!(report.changed(), &["provider-b"]);
        assert_eq!(report.removed(), &["provider-a"]);
        assert_eq!(
            catalog(&host),
            vec![("provider-b".into(), FacetKind::Foreign)]
        );
        assert_eq!(snapshot[0].name(), "provider-a");
        assert_eq!(
            reference.call_json("read", None, None).await?,
            Some(json!(2))
        );
        let current = host
            .get(contract.clone(), json!({}), GetOptions::default())
            .await?;
        assert_eq!(current.object_id(), id);
        let same_name = host.installed_facets();
        host.reload(
            vec![provider("provider-b", contract, 3, false)],
            ReloadOptions::default(),
        )
        .await?;
        assert_eq!(host.installed_facets(), same_name);
        assert_eq!(
            reference.call_json("read", None, None).await?,
            Some(json!(3))
        );
        reference.release();
        current.release();
        host.close().await?;
        assert!(host.installed_facets().is_empty());
        Ok(())
    })
}

#[test]
fn reentrant_initial_setup_observes_its_scoped_admission() -> Result<()> {
    let driver = ManualDriver::new();
    let host = Host::new(HostOptions::new("catalog", json!(null)), driver.clone());
    let queried = host.clone();
    let observed = Rc::new(RefCell::new(Vec::new()));
    let record = observed.clone();
    let definition = NativeFacet::new("scoped", vec![], move |_, _| {
        *record.borrow_mut() = catalog(&queried);
        Err(RuntimeError::invalid("setup_refused", "setup refused"))
    })?;
    assert_eq!(
        host.install(definition, LoadOptions::default())
            .err()
            .unwrap()
            .reason,
        "setup_refused"
    );
    assert_eq!(&*observed.borrow(), &[("scoped".into(), FacetKind::Native)]);
    assert!(host.installed_facets().is_empty());
    driver.run_until(host.close())
}

struct QueryOnDrop {
    name: String,
    host: Rc<RefCell<Option<Host>>>,
    observations: Rc<RefCell<Vec<(String, Catalog)>>>,
}

impl Drop for QueryOnDrop {
    fn drop(&mut self) {
        if let Some(host) = self.host.borrow().as_ref() {
            self.observations
                .borrow_mut()
                .push((self.name.clone(), catalog(host)));
        }
    }
}

fn querying_facet(
    name: &str,
    host: Rc<RefCell<Option<Host>>>,
    observations: Rc<RefCell<Vec<(String, Catalog)>>>,
    refuse: bool,
) -> Result<ExecutableFacet> {
    let owner = Rc::new(QueryOnDrop {
        name: name.into(),
        host,
        observations,
    });
    ExecutableFacet::foreign(name, vec![], move |_, setup| {
        setup.recorder().retain_run_owner(owner.clone())?;
        if refuse {
            return Err(RuntimeError::invalid("setup_refused", "setup refused"));
        }
        Ok(())
    })
}

#[test]
fn user_destructors_can_query_after_withdrawal_retirement_and_close() -> Result<()> {
    let driver = ManualDriver::new();
    let host = Host::new(HostOptions::new("catalog", json!(null)), driver.clone());
    let queried = Rc::new(RefCell::new(Some(host.clone())));
    let observations = Rc::new(RefCell::new(Vec::new()));
    let refused = querying_facet("withdrawn", queried.clone(), observations.clone(), true)?;
    assert_eq!(
        host.install_executable(refused, LoadOptions::default())
            .err()
            .unwrap()
            .reason,
        "setup_refused"
    );
    driver.run_until(async {
        let initial = querying_facet("old", queried.clone(), observations.clone(), false)?;
        host.install_executable(initial, LoadOptions::default())?
            .await?;
        let next = querying_facet("new", queried.clone(), observations.clone(), false)?;
        host.reload(
            vec![next],
            ReloadOptions {
                remove: vec!["old".into()],
                ..Default::default()
            },
        )
        .await?;
        host.close().await
    })?;
    queried.borrow_mut().take();
    assert_eq!(
        &*observations.borrow(),
        &[
            ("withdrawn".into(), vec![]),
            ("old".into(), vec![("new".into(), FacetKind::Foreign)]),
            ("new".into(), vec![]),
        ]
    );
    Ok(())
}

#[test]
fn cleanup_failure_and_shared_close_waiters_still_observe_an_empty_catalog() -> Result<()> {
    let driver = ManualDriver::new();
    let host = Host::new(HostOptions::new("catalog", json!(null)), driver.clone());
    let contract =
        Rc::new(Contract::local("cleanup-owner", 1, json!({}), BTreeMap::new()).unwrap());
    let definition = NativeFacet::new("cleanup", vec![], move |_, setup| {
        setup.serve(
            contract.clone(),
            Rc::new(|_, _, declaration| {
                declaration.resource(
                    "owned",
                    ResourceDeclaration::new(|_| async { Ok(()) }.boxed_local()).with_close(|_| {
                        async { Err(RuntimeError::invalid("close_refused", "cleanup failed")) }
                            .boxed_local()
                    }),
                )?;
                Ok(Implementation::new())
            }),
            ServeOptions {
                lifetime: Lifetime::Host,
                ..Default::default()
            },
        )
    })?;
    driver.run_until(async {
        host.install(definition, LoadOptions::default())?.await?;
        let (first, second) = futures::join!(host.close(), host.close());
        assert!(first.is_err());
        assert!(second.is_err());
        assert!(host.installed_facets().is_empty());
        Ok(())
    })
}
