use tembra::{Orb, Service};
use tembra_executor_wasm::Contract;

pub fn native_math(contract: &Contract, name: &str) -> Orb {
    let provider =
        Contract::from_bytes(&contract.encode().unwrap(), Some("arithmetic-provider")).unwrap();
    let metadata = provider
        .plan(name, 1, &Default::default())
        .unwrap()
        .orb
        .services
        .remove(0);
    Orb::new(name, 1).service(
        Service::new(metadata.name, |_| Ok(()))
            .signature(metadata.signature)
            .method(
                "double",
                |_, _, (value,): (i32,)| async move { Ok(value * 2) },
            ),
    )
}
