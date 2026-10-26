use futures::{FutureExt, channel::oneshot, executor::LocalPool};
use std::{cell::RefCell, rc::Rc};
use tembra::{Host, Orb, Service, core::Lifetime};

#[test]
fn native_linger_expires_without_manual_kernel_ticks() {
    let mut pool = LocalPool::new();
    let host = Host::new(pool.spawner());
    let (send, closed) = oneshot::channel();
    let send = Rc::new(RefCell::new(Some(send)));
    host.install(vec![
        Orb::new("linger", 1).service(
            Service::new("linger", move |self_| {
                let send = send.clone();
                self_.resource(
                    "connection",
                    1,
                    || Ok(()),
                    move |_, _| {
                        async move {
                            let _ = send.borrow_mut().take().unwrap().send(());
                            Ok(())
                        }
                        .boxed_local()
                    },
                )?;
                Ok(())
            })
            .lifetime(Lifetime::Referenced, 10),
        ),
    ])
    .unwrap();
    let reference = host.get("linger", vec!["main".into()], "alice").unwrap();
    pool.run_until(reference.ready()).unwrap();
    reference.release().unwrap();
    assert_eq!(host.kernel().diagnostics().objects, 1);
    pool.run_until(closed).unwrap();
    pool.run_until(host.close()).unwrap();
    assert_eq!(host.kernel().diagnostics().objects, 0);
}

#[test]
fn linger_starts_when_the_final_physical_call_settles() {
    use std::cell::Cell;
    #[derive(Clone)]
    struct Clock(Rc<Cell<u64>>);
    impl tembra::Clock for Clock {
        fn now_ms(&self) -> u64 {
            self.0.get()
        }
        fn sleep(&self, _milliseconds: u64) -> futures::future::LocalBoxFuture<'static, ()> {
            futures::future::pending().boxed_local()
        }
    }
    let mut pool = LocalPool::new();
    let clock = Clock(Rc::new(Cell::new(0)));
    let host = Host::with_clock(tembra::core::Host::new(), pool.spawner(), clock.clone());
    let (send, receive) = oneshot::channel();
    let receive = Rc::new(RefCell::new(Some(receive)));
    host.install(vec![
        Orb::new("pending", 1).service(
            Service::new("pending", move |_| Ok(receive.clone()))
                .method("wait", |receive, _, (): ()| {
                    let receive = receive.borrow_mut().take().unwrap();
                    async move {
                        receive.await.unwrap();
                        Ok(())
                    }
                })
                .lifetime(Lifetime::Referenced, 100),
        ),
    ])
    .unwrap();
    let reference = Rc::new(host.get("pending", vec!["main".into()], "alice").unwrap());
    pool.run_until(reference.ready()).unwrap();
    use std::{future::Future, task::Poll};
    let mut waiting = Box::pin(host.root("alice", {
        let reference = reference.clone();
        move |ctx| async move { reference.call::<_, ()>(&ctx, "wait", ()).await }.boxed_local()
    }));
    pool.run_until(futures::future::poll_fn(|ctx| {
        assert!(waiting.as_mut().poll(ctx).is_pending());
        Poll::Ready(())
    }));
    pool.run_until_stalled();
    reference.release().unwrap();
    clock.0.set(1000);
    send.send(()).unwrap();
    pool.run_until(waiting).unwrap();
    assert_eq!(host.kernel().next_idle(), Some(100));
    clock.0.set(1100);
    host.flush();
    pool.run_until(host.close()).unwrap();
}
