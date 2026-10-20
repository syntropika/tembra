//! Run with `cargo run -p tembra-core --example local`.

use std::{cell::Cell, collections::BTreeMap, rc::Rc};

use futures::FutureExt;
use serde_json::json;
use tembra_contracts::{Contract, Member, ValueSchema};
use tembra_core::{GetOptions, Host, HostOptions, Implementation, ManualDriver, ServeOptions};

fn main() -> std::result::Result<(), Box<dyn std::error::Error>> {
    let driver = ManualDriver::new();
    driver.run_until(async {
        let contract = Rc::new(Contract::local(
            "example.native",
            1,
            json!({"type":"object", "properties":{"name":{"type":"string"}}, "required":["name"]}),
            BTreeMap::from([
                (
                    "label".into(),
                    Member::Method {
                        args: None,
                        result: Some(ValueSchema::Portable(json!({"type":"string"}))),
                    },
                ),
                (
                    "native".into(),
                    Member::Method {
                        args: Some(ValueSchema::Native("example.counter@1".into())),
                        result: Some(ValueSchema::Native("example.counter@1".into())),
                    },
                ),
            ]),
        )?);
        let host = Host::new(
            HostOptions::new("example", json!({"kind":"host"})),
            driver.clone(),
        );
        host.serve(
            contract.clone(),
            Rc::new(|input, _principal, _declaration| {
                let name = input["name"].as_str().unwrap().to_owned();
                Ok(Implementation::new()
                    .method::<(), String>("label", move |(), _ctx| {
                        let name = name.clone();
                        async move { Ok(format!("Hello, {name}")) }.boxed_local()
                    })
                    .method::<Rc<Cell<u64>>, Rc<Cell<u64>>>("native", |counter, _ctx| {
                        async move {
                            counter.set(counter.get() + 1);
                            Ok(counter)
                        }
                        .boxed_local()
                    }))
            }),
            ServeOptions::default(),
        )?;
        let first = host
            .get(
                contract.clone(),
                json!({"name":"world"}),
                GetOptions::default(),
            )
            .await?;
        let second = host
            .get(contract, json!({"name":"world"}), GetOptions::default())
            .await?;
        let label: String = first.call("label", (), None).await?;
        let native = Rc::new(Cell::new(0_u64));
        let returned: Rc<Cell<u64>> = second.call("native", native.clone(), None).await?;
        assert!(Rc::ptr_eq(&native, &returned));
        println!("{label}; native value: {}", returned.get());
        first.release();
        second.release();
        host.close().await?;
        Ok(())
    })
}
