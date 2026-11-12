//! Contract authoring through the same dependency used to install and call Orbs.
use tembra::contracts::{Contract, PreparedContract};

#[test]
fn authored_contracts_share_the_prepared_runtime_model() {
    let contract = Contract::from_source(
        "facade.wit",
        "package example:facade@1.0.0; interface counter { value: func() -> u64; } world app { export counter; }",
        Some("app"),
    )
    .unwrap();
    #[cfg(all(feature = "wasm", not(target_arch = "wasm32")))]
    let _: &tembra::wasm::Contract = &contract;
    let prepared = PreparedContract::decode(&contract.prepared().encode()).unwrap();
    assert_eq!(prepared.world(), "app");
    assert_eq!(prepared.model(), contract.prepared().model());
}
