//! Compile in a separate process so another test cannot initialize Rayon's global pool first.
use tembra_executor_wasm::{Engine, Limits};

#[test]
fn compilation_leaves_the_hosts_global_pool_available() {
    // One defined function forces compilation before the private interpreter ABI check refuses it.
    assert!(
        Engine::new(Limits::default())
            .unwrap()
            .interpreter(
                b"\0asm\x01\0\0\0\x01\x04\x01\x60\x00\x00\x03\x02\x01\x00\x0a\x04\x01\x02\x00\x0b",
            )
            .is_err()
    );
    rayon::ThreadPoolBuilder::new()
        .num_threads(1)
        .build_global()
        .unwrap();
}
