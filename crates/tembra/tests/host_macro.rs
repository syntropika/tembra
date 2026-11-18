//! `tembra::__host!` expands its input when the facade links the native runtime, as it does in
//! this build. Without `host` it expands to nothing, which
//! `cargo check -p tembra --no-default-features --features macros` checks on a use in `src/lib.rs`.

::tembra::__host! {
    /// Several items pass through as written.
    struct Calculator;

    impl Calculator {
        fn orb() -> ::tembra::Result<::tembra::Orb> {
            Ok(::tembra::Orb::new("calculator", 1).service(
                ::tembra::Service::new("calculator::math", |_| Ok(()))
                    .method("add", |_, _, (a, b): (u32, u32)| async move { Ok(a + b) }),
            ))
        }
    }
}

#[test]
fn the_input_is_expanded_with_host() {
    let orb = Calculator::orb().unwrap();
    let declaration = orb.declaration();
    assert_eq!(declaration.name, "calculator");
    assert!(declaration.services[0].methods.contains("add"));
}
