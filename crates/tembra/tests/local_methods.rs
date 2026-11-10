//! Host-local methods accept only typed in-process calls; every portable path is refused.
use futures::{FutureExt, executor::LocalPool};
use std::{cell::Cell, rc::Rc};
use tembra::{Host, Orb, Service};

/// A value with no serde implementation, standing in for owned host resources.
struct Owned(u32);

fn service(entered: Rc<Cell<u32>>) -> Service<()> {
    Service::new("ingress", |_| Ok(()))
        .local_method("handle", move |_, _, Owned(value): Owned| {
            entered.set(entered.get() + 1);
            async move { Ok(Owned(value + 1)) }
        })
        .method("ping", |_, _, (value,): (u32,)| async move { Ok(value) })
}

fn check(isolated: bool) {
    let mut pool = LocalPool::new();
    let host = Host::new(pool.spawner());
    let entered = Rc::new(Cell::new(0));
    let orb = Orb::new("web", 1).service(service(entered.clone()));
    host.install(vec![if isolated { orb.isolated() } else { orb }])
        .unwrap();
    let reference = Rc::new(host.get("ingress", vec!["main".into()], "host").unwrap());
    let called = reference.clone();
    let counted = entered.clone();
    pool.run_until(host.root("host", move |context| {
        async move {
            // The typed local call works, alongside an ordinary portable method.
            let Owned(value) = called
                .call::<_, Owned>(&context, "handle", Owned(1))
                .await?;
            assert_eq!(value, 2);
            assert_eq!(counted.get(), 1);
            assert_eq!(
                called
                    .call_portable::<_, u32>(&context, "ping", (7_u32,))
                    .await?,
                7
            );
            // Every portable boundary is refused deterministically, before the callback runs.
            for error in [
                called
                    .call_portable::<_, u32>(&context, "handle", (1_u32,))
                    .await
                    .unwrap_err(),
                called
                    .call_value(&context, "handle", serde_json::json!([1]))
                    .await
                    .unwrap_err(),
            ] {
                assert_eq!(error.reason, "local_value", "{error}");
                assert!(error.message.contains("host-local"), "{error}");
            }
            // The local handle was never entered by a portable caller.
            assert_eq!(counted.get(), 1);
            // A local call with a different Rust type is still a type error, not a conversion.
            let error = called
                .call::<_, Owned>(&context, "handle", 1_u32)
                .await
                .err()
                .unwrap();
            assert_eq!(error.reason, "rust_type");
            Ok(())
        }
        .boxed_local()
    }))
    .unwrap();
    reference.release().unwrap();
    pool.run_until(host.close()).unwrap();
}

#[test]
fn local_methods_refuse_portable_callers() {
    check(false);
}

#[test]
fn isolated_local_methods_refuse_portable_callers() {
    check(true);
}
