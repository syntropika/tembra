//! `exchange::orb()` in a native host: the Orb imports `example:ledger/money@1.0.0`, an interface
//! of the package in `wit/` that holds types only. Such an import carries no authority, so its
//! contract plans no provider for it and `cargo tembra build` binds a component's to nothing:
//! natively too, `orb()` takes no provider, the Orb requires none, and a method takes and returns
//! a type of the import.
use futures::{FutureExt, executor::LocalPool};
use serde_json::{Value, json};
use std::{path::Path, rc::Rc};
use tembra::{Host, contracts::Contract};
use tembra_orb_native_fixture::exchange::{self, money};

const DESK: &str = "calculator::example:exchange/desk@1.0.0";

/// `orb()` declares what the kernel computes for a component of the same contract, whose plan
/// needs no provider: the desk's service, and no requirement.
#[test]
fn an_import_of_types_only_needs_no_provider() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("wit");
    let contract =
        Contract::from_source_in(dir, "exchange.wit", exchange::WIT, Some("exchange")).unwrap();
    let expected = contract
        .plan("calculator", 2, &Default::default())
        .unwrap()
        .orb;
    let orb = exchange::orb().unwrap();
    assert_eq!(orb.declaration(), &expected);
    assert!(orb.declaration().requires.is_empty());
    let mut pool = LocalPool::new();
    let host = Host::new(pool.spawner());
    host.install(vec![orb]).unwrap();
    let desk = Rc::new(host.get(DESK, vec!["main".into()], "host").unwrap());
    let called = desk.clone();
    let (portable, local) = pool
        .run_until(host.root("host", move |context| {
            async move {
                let portable = called
                    .call_portable::<_, Value>(&context, "swap", json!(["eur"]))
                    .await?;
                let local = exchange::DeskClient::new(called)
                    .swap(&context, money::Currency::Usd)
                    .await?;
                Ok((portable, local))
            }
            .boxed_local()
        }))
        .unwrap();
    assert_eq!(portable, json!("usd"));
    assert_eq!(local, money::Currency::Eur);
    desk.release().unwrap();
    pool.run_until(host.close()).unwrap();
}
