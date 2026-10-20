//! Run checked migration and kept-resource adoption with `just run-example reload`.
use futures::FutureExt;
use serde_json::json;
use std::{cell::Cell, collections::BTreeMap, rc::Rc};
use tembra_core::{
    contracts::{Contract, Member},
    *,
};

fn facet(
    contract: Rc<Contract>,
    version: u64,
    allow_adoption: bool,
    opens: Rc<Cell<usize>>,
    closes: Rc<Cell<usize>>,
) -> Result<ExecutableFacet> {
    ExecutableFacet::foreign("counter", vec![], move |_, setup| {
        let (opens, closes) = (opens.clone(), closes.clone());
        setup.serve(
            contract.clone(),
            Rc::new(move |_, _, declaration| {
                let count = declaration.state(
                    "count",
                    StateDeclaration::new(json!({"type":"integer","minimum":0}), json!(3))
                        .version(version)
                        .with_migrate(|value, prior_version| {
                            assert_eq!(prior_version, 1);
                            Ok(json!(value.as_u64().unwrap() + 4))
                        }),
                )?;
                let (opens, closes) = (opens.clone(), closes.clone());
                declaration.resource(
                    "session",
                    ResourceDeclaration::new(move |_| {
                        opens.set(opens.get() + 1);
                        async { Ok(37_u64) }.boxed_local()
                    })
                    .with_adoption_check(move |value| {
                        if allow_adoption && *value == 37 {
                            Ok(())
                        } else {
                            Err(RuntimeError::invalid(
                                "session_adoption",
                                "session is incompatible",
                            ))
                        }
                    })
                    .with_close(move |_| {
                        closes.set(closes.get() + 1);
                        async { Ok(()) }.boxed_local()
                    }),
                )?;
                Ok(Implementation::new().state("count", count.readable()))
            }),
            ServeOptions {
                lifetime: Lifetime::Host,
                ..Default::default()
            },
        )
    })
}

fn main() -> std::result::Result<(), Box<dyn std::error::Error>> {
    let driver = ManualDriver::new();
    driver.run_until(async {
        let host = Host::new(
            HostOptions::new("reload-example", json!(null)),
            driver.clone(),
        );
        let contract = Rc::new(Contract::local(
            "example.reload",
            1,
            json!({}),
            BTreeMap::from([(
                "count".into(),
                Member::State {
                    schema: json!({"type":"integer","minimum":0}),
                },
            )]),
        )?);
        let (opens, closes) = (Rc::new(Cell::new(0)), Rc::new(Cell::new(0)));
        host.install_executable(
            facet(contract.clone(), 1, true, opens.clone(), closes.clone())?,
            LoadOptions::default(),
        )?
        .await?;
        let reference = host
            .get(contract.clone(), json!({}), GetOptions::default())
            .await?;
        let id = reference.object_id();
        let count = reference.readable("count")?;
        let before = count.snapshot()?;
        let error = host
            .reload(
                vec![facet(
                    contract.clone(),
                    2,
                    false,
                    opens.clone(),
                    closes.clone(),
                )?],
                ReloadOptions::default(),
            )
            .await
            .unwrap_err();
        assert_eq!(error.reason, "session_adoption");
        assert_eq!(count.snapshot()?.value(), before.value());
        assert_eq!(count.snapshot()?.revision(), before.revision());
        assert_eq!((opens.get(), closes.get()), (1, 0));
        let report = host
            .reload(
                vec![facet(
                    contract.clone(),
                    2,
                    true,
                    opens.clone(),
                    closes.clone(),
                )?],
                ReloadOptions::default(),
            )
            .await?;
        assert_eq!(report.current_version(), 2);
        assert_eq!(count.snapshot()?.value(), &json!(7));
        let current = host.get(contract, json!({}), GetOptions::default()).await?;
        assert_eq!(current.object_id(), id);
        assert_eq!((opens.get(), closes.get()), (1, 0));
        let empty = host.reload(vec![], ReloadOptions::default()).await?;
        assert_eq!(empty.current_version(), 3);
        assert!(empty.prepared_objects().is_empty());
        assert_eq!(count.snapshot()?.value(), &json!(7));
        current.release();
        reference.release();
        host.close().await?;
        assert_eq!((opens.get(), closes.get()), (1, 1));
        println!(
            "code epoch {}; retained count 7; session opened and closed once",
            empty.current_version()
        );
        Ok(())
    })
}
