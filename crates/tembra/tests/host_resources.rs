//! A host resource read as another Rust type than the value it opened is refused with a text that
//! names the resource, not with the text of a method's mismatched argument or result.
//! A resource whose value was released reads as gone.
use futures::{FutureExt, executor::LocalPool};
use tembra::{
    Host,
    core::{Code, Owner},
};

#[test]
fn a_resource_read_as_another_type_names_the_resource() {
    let mut pool = LocalPool::new();
    let host = Host::new(pool.spawner());
    let port = host
        .declare(|declaration| {
            declaration.resource(
                "port",
                1,
                || Ok(8080_u32),
                |_, _| async { Ok(()) }.boxed_local(),
            )
        })
        .unwrap();
    pool.run_until_stalled();
    assert_eq!(*port.value().unwrap(), 8080);
    let error = host
        .resource::<String>("port")
        .unwrap()
        .value()
        .unwrap_err();
    assert_eq!(error.reason, "rust_type");
    assert_eq!(
        error.message,
        "the resource `port` holds a value of another Rust type than the one it is read as"
    );
    pool.run_until(host.close()).unwrap();
}

/// A resource whose value the host released reads as gone, not as one of another Rust type.
#[test]
fn a_released_resource_value_reads_as_gone() {
    let mut pool = LocalPool::new();
    let host = Host::new(pool.spawner());
    let port = host
        .declare(|declaration| {
            declaration.resource(
                "port",
                1,
                || Ok(8080_u32),
                |_, _| async { Ok(()) }.boxed_local(),
            )
        })
        .unwrap();
    pool.run_until_stalled();
    let value = host
        .kernel()
        .resource(Owner::Host, "port".into(), 1, false, Vec::new())
        .unwrap()
        .value
        .unwrap();
    host.drop_value(value);
    let error = port.value().unwrap_err();
    assert_eq!(
        (error.code, error.reason.as_str()),
        (Code::Gone, "resource")
    );
    assert_eq!(
        error.message,
        "the value of the resource `port` was released"
    );
    pool.run_until(host.close()).unwrap();
}
