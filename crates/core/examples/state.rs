//! Observe core-owned state through managed references with `just run-example state`.

use std::{collections::BTreeMap, rc::Rc};

use futures::FutureExt;
use serde_json::json;
use tembra_core::{
    GetOptions, Host, HostOptions, Implementation, ManualDriver, ServeOptions, StateDeclaration,
    contracts::{Contract, Member, ValueSchema},
};

fn main() -> std::result::Result<(), Box<dyn std::error::Error>> {
    let driver = ManualDriver::new();
    driver.run_until(async {
        let count_schema = json!({"type":"integer","minimum":0,"maximum":100});
        let double_schema = json!({"type":"integer","minimum":0,"maximum":200});
        let contract = Rc::new(Contract::local(
            "example.state",
            1,
            json!({}),
            BTreeMap::from([
                (
                    "count".into(),
                    Member::State {
                        schema: count_schema.clone(),
                    },
                ),
                (
                    "double".into(),
                    Member::State {
                        schema: double_schema.clone(),
                    },
                ),
                (
                    "increment".into(),
                    Member::Method {
                        args: Some(ValueSchema::Portable(
                            json!({"type":"integer","minimum":1,"maximum":100}),
                        )),
                        result: Some(ValueSchema::Portable(count_schema.clone())),
                    },
                ),
            ]),
        )?);
        let host = Host::new(HostOptions::new("example", json!("host")), driver.clone());
        host.serve(
            contract.clone(),
            Rc::new(move |_, _, declaration| {
                let count = declaration.state(
                    "count",
                    StateDeclaration::new(count_schema.clone(), json!(0)),
                )?;
                let double = declaration.derive(
                    vec![count.readable()],
                    double_schema.clone(),
                    |sources| Ok(json!(sources[0].value().as_u64().unwrap() * 2)),
                )?;
                Ok(Implementation::new()
                    .state("count", count.readable())
                    .state("double", double)
                    .json_method("increment", move |args, ctx| {
                        let count = count.clone();
                        async move {
                            let by = args.unwrap().as_u64().unwrap();
                            let snapshot = count.change(&ctx, |draft| {
                                *draft = json!(draft.as_u64().unwrap() + by);
                                Ok(())
                            })?;
                            Ok(Some(snapshot.value().clone()))
                        }
                        .boxed_local()
                    }))
            }),
            ServeOptions::default(),
        )?;
        let reference = host.get(contract, json!({}), GetOptions::default()).await?;
        let count = reference.readable("count")?;
        let double = reference.readable("double")?;
        let peer = double.clone();
        let observation = count.subscribe(move |snapshot| {
            println!(
                "count={}, double={}, revision={}",
                snapshot.value(),
                peer.snapshot()?.value(),
                snapshot.revision()
            );
            Ok(())
        })?;
        let initial = count.snapshot()?;
        reference
            .call_json("increment", Some(json!(3)), None)
            .await?;
        reference
            .call_json("increment", Some(json!(2)), None)
            .await?;
        assert_eq!(initial.value(), &json!(0));
        assert_eq!(count.snapshot()?.value(), &json!(5));
        assert_eq!(double.snapshot()?.value(), &json!(10));
        observation.unsubscribe();
        reference.release();
        host.close().await?;
        Ok(())
    })
}
