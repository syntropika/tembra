//! A module the native side does not cover yet compiles where the runtime is linked, as this crate's
//! modules `dice` and `desk` do here, since Cargo enables the facade's
//! `host` for an Orb crate whenever another crate of the build does. Their `orb()` answers an error
//! with the code `unsupported` and the reason `native`, naming what the native side does not
//! cover, so only a host that installs such an Orb fails.
use futures::executor::LocalPool;
use tembra::{Host, Orb, core::Code};
use tembra_orb_native_fixture::{desk, dice};

/// Install the Orb `orb()` answers in a host, as a native host does, and return the error the host
/// gets: `unsupported/native`.
fn install(orb: fn() -> tembra::Result<Orb>) -> String {
    let pool = LocalPool::new();
    let host = Host::new(pool.spawner());
    let error = orb()
        .and_then(|orb| host.install(vec![orb]))
        .expect_err("the native side does not build the Orb");
    assert_eq!(
        (error.code, error.reason.as_str()),
        (Code::Unsupported, "native"),
        "{error}"
    );
    error.message
}

#[test]
fn a_wasi_import_fails_the_installation() {
    assert_eq!(
        install(dice::orb),
        "the import of wasi:random/random@0.3.0 is not supported natively yet"
    );
}

#[test]
fn a_record_holding_a_resource_fails_the_installation() {
    assert_eq!(
        install(desk::orb),
        "the field `session` of `Lease` is `Session`, a resource; a record or a variant holding a resource is component-only, since the native boundary carries a resource beside the data, as a handle, never inside the JSON of a record: to return the resource natively too, return it in a tuple beside the data, such as `-> (Session, String)`"
    );
}
