//! Lifecycle regressions identified by independent review.
use futures::{FutureExt, executor::LocalPool};
use tembra::{Host, Orb, Service};

#[test]
fn an_existing_kernel_clock_and_manual_advances_remain_monotonic() {
    let mut pool = LocalPool::new();
    let kernel = tembra::core::Host::new();
    kernel.tick(10_000).unwrap();
    let host = Host::with_kernel(kernel, pool.spawner());
    host.install(vec![
        Orb::new("clock", 1).service(
            Service::new("clock", |_| Ok(()))
                .method("read", |_, _, (): ()| async { Ok(42) })
                .lifetime(tembra::core::Lifetime::Referenced, 10),
        ),
    ])
    .unwrap();
    let reference = host.get("clock", vec!["main".into()], "alice").unwrap();
    pool.run_until(reference.ready()).unwrap();
    reference.release().unwrap();
    host.kernel().tick(20_000).unwrap();
    host.flush();
    pool.run_until_stalled();
    assert_eq!(host.kernel().diagnostics().objects, 0);
    pool.run_until(host.close()).unwrap();
}

#[test]
fn failed_host_declarations_do_not_become_parents_or_reserve_resource_names() {
    let mut pool = LocalPool::new();
    let host = Host::new(pool.spawner());
    let result: tembra::Result<()> = host.declare(|declaration| {
        declaration.resource("retry", 1, || Ok(()), |_, _| async { Ok(()) }.boxed_local())?;
        Err(tembra::Error::invalid(
            tembra::core::Reason::Scope,
            "declaration failed",
        ))
    });
    assert!(result.is_err());
    pool.run_until_stalled();
    host.declare(|declaration| {
        declaration.resource("retry", 1, || Ok(()), |_, _| async { Ok(()) }.boxed_local())
    })
    .unwrap();
    host.install(vec![
        Orb::new("healthy", 1).service(Service::new("healthy", |_| Ok(()))),
    ])
    .unwrap();
    let reference = host.get("healthy", vec!["main".into()], "alice").unwrap();
    pool.run_until(reference.ready()).unwrap();
    reference.release().unwrap();
    pool.run_until(host.close()).unwrap();
}

#[test]
fn failed_orb_factories_stay_failed_without_executing_with_a_dummy_environment() {
    let mut pool = LocalPool::new();
    let host = Host::new(pool.spawner());
    let bad = Orb::new("failed", 1)
        .create(|_| {
            Err::<(), _>(tembra::Error::invalid(
                tembra::core::Reason::Scope,
                "orb factory failed",
            ))
        })
        .service(Service::new("failed", |_| Ok(())));
    assert!(host.install(vec![bad]).is_err());
    pool.run_until_stalled();
    let error = host
        .get("failed", vec!["main".into()], "alice")
        .err()
        .expect("failed orb admitted an object");
    assert_eq!(error.message, "orb factory failed");
    host.install(vec![
        Orb::new("healthy", 1).service(Service::new("healthy", |_| Ok(()))),
    ])
    .unwrap();
    let reference = host.get("healthy", vec!["main".into()], "alice").unwrap();
    pool.run_until(reference.ready()).unwrap();
    reference.release().unwrap();
    pool.run_until(host.close()).unwrap();
}

#[test]
fn panicking_callbacks_settle_their_invocations_and_do_not_poison_shutdown() {
    let mut pool = LocalPool::new();
    let host = Host::new(pool.spawner());
    host.install(vec![
        Orb::new("panic", 1).service(
            Service::new("panic", |_| Ok(()))
                .method("panic", |_, _, (): ()| async {
                    panic!("user callback panic");
                    #[allow(unreachable_code)]
                    Ok(())
                })
                .method("read", |_, _, (): ()| async { Ok(42_u32) }),
        ),
    ])
    .unwrap();
    let reference = std::rc::Rc::new(host.get("panic", vec!["main".into()], "alice").unwrap());
    let r = reference.clone();
    let error = pool
        .run_until(host.root("alice", move |ctx| {
            async move { r.call::<_, ()>(&ctx, "panic", ()).await }.boxed_local()
        }))
        .unwrap_err();
    assert_eq!(error.reason, "callback");
    let r = reference.clone();
    assert_eq!(
        pool.run_until(host.root("alice", move |ctx| {
            async move { r.call::<_, u32>(&ctx, "read", ()).await }.boxed_local()
        }))
        .unwrap(),
        42
    );
    reference.release().unwrap();
    assert_eq!(host.kernel().diagnostics().calls, 0);
    pool.run_until(host.close()).unwrap();
}
