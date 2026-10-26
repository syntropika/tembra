#![cfg(feature = "hooks")]
use futures::{FutureExt, executor::LocalPool};
use std::{cell::Cell, rc::Rc, time::Duration};
use tembra::{Hook, Host, Orb, Service};

#[test]
fn disposing_a_temporary_dependency_does_not_cancel_terminal_observation() {
    let mut pool = LocalPool::new();
    let host = Host::new(pool.spawner());
    let terminal = Hook::<(), ()>::series("terminal");
    let declared = terminal.clone();
    let later = Rc::new(Cell::new(false));
    let observed = later.clone();
    host.install(vec![
        Orb::new("provider", 1)
            .hook(terminal.clone())
            .service(
                Service::new("temporary", |_| Ok(()))
                    .method("read", |_, _, (): ()| async { Ok(42_i32) }),
            )
            .service(
                Service::new("provider", move |self_| self_.hook(&declared)).method(
                    "run",
                    |hook, ctx, (): ()| async move {
                        ctx.cancel()?;
                        hook.observe(&ctx, ()).await
                    },
                ),
            )
            .contribute("provider", &terminal, 0, |ctx, ()| async move {
                let reference =
                    ctx.get("temporary", vec!["transient".into()], Default::default())?;
                assert_eq!(reference.call::<_, i32>(&ctx, "read", ()).await?, 42);
                reference.release()?;
                futures_timer::Delay::new(Duration::from_millis(1)).await;
                ctx.check()?;
                Ok(())
            })
            .contribute("provider", &terminal, 1, move |ctx, ()| {
                let observed = observed.clone();
                async move {
                    ctx.check()?;
                    observed.set(true);
                    Ok(())
                }
            }),
    ])
    .unwrap();
    let reference = Rc::new(host.get("provider", vec!["main".into()], "alice").unwrap());
    pool.run_until(host.root("alice", move |ctx| {
        async move { reference.call::<_, ()>(&ctx, "run", ()).await }.boxed_local()
    }))
    .expect("terminal observation stays valid after unrelated temporary disposal");
    assert!(later.get(), "the later terminal participant must run");
    pool.run_until(host.close()).unwrap();
    let diagnostics = host.kernel().diagnostics();
    assert_eq!(
        (diagnostics.roots, diagnostics.calls, diagnostics.objects),
        (0, 0, 0)
    );
}
