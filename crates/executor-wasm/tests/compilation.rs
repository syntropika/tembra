//! Compile in a separate process so another test cannot initialize Rayon's global pool first.
use tembra_executor_wasm::{Contract, Engine, Install, Limits};

#[test]
fn compilation_leaves_the_hosts_global_pool_available() {
    // Loading a real component compiles it on the engine's own bounded workers.
    let contract =
        Contract::from_source("empty.wit", "package test:empty; world run {}", None).unwrap();
    let artifact = contract
        .component_artifact(
            &wat::parse_str("(component)").unwrap(),
            "empty",
            1,
            &Default::default(),
        )
        .unwrap();
    let engine = Engine::new(Limits::default()).unwrap();
    drop(engine.load(&artifact, Install::default()).unwrap());
    rayon::ThreadPoolBuilder::new()
        .num_threads(1)
        .build_global()
        .unwrap();
}
