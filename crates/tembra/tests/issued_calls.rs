//! A caller cancels exactly the call it issued (`IssuedCall`) and waits for its physical end.
//!
//! Native providers only: the issued call observes its own signal, a call it issued observes
//! it too, the cancelled call can no longer issue calls, and an unrelated call of the same root
//! is untouched. `settled` resolves only after the issued call physically ended.
use futures::{
    FutureExt, channel::oneshot, executor::LocalPool, future::Either, task::LocalSpawnExt,
};
use std::{cell::RefCell, rc::Rc};
use tembra::{Host, IssuedCall, Orb, Reference, Service};

#[derive(Default)]
struct Probe {
    log: RefCell<Vec<String>>,
    leaf: RefCell<Option<Rc<Reference>>>,
    /// Held by the issued call after it observed its cancellation: its physical end.
    physical: RefCell<Option<oneshot::Receiver<()>>>,
    /// Held by the unrelated call.
    unrelated: RefCell<Option<oneshot::Receiver<()>>>,
}

impl Probe {
    fn note(&self, entry: impl Into<String>) {
        self.log.borrow_mut().push(entry.into());
    }

    fn logged(&self, entry: &str) -> bool {
        self.log.borrow().iter().any(|line| line == entry)
    }
}

fn orb(probe: Rc<Probe>) -> Orb {
    let worker = probe.clone();
    let unrelated = probe.clone();
    let leaf = probe.clone();
    Orb::new("calls", 1)
        .service(
            Service::new("worker", |_| Ok(()))
                .method("wait", move |_, context, (): ()| {
                    let probe = worker.clone();
                    async move {
                        probe.note("wait:start");
                        let reference = probe.leaf.borrow().clone().unwrap();
                        let nested = reference
                            .call_portable::<_, bool>(&context, "hold", ())
                            .boxed_local();
                        let signal = context.signal();
                        let cancelled = std::pin::pin!(signal.cancelled());
                        let Either::Right((_, nested)) =
                            futures::future::select(nested, cancelled).await
                        else {
                            panic!("the nested call ended before the cancellation");
                        };
                        probe.note("wait:signalled");
                        // The call it issued observed the same cancellation.
                        probe.note(format!("wait:nested:{}", nested.await?));
                        // A cancelled call gains no fresh authority.
                        let fresh = reference
                            .call_portable::<_, bool>(&context, "hold", ())
                            .await
                            .unwrap_err();
                        probe.note(format!("wait:fresh:{:?}", fresh.code));
                        let physical = probe.physical.borrow_mut().take().unwrap();
                        let _ = physical.await;
                        probe.note("wait:end");
                        Ok(())
                    }
                })
                .method("unrelated", move |_, context, (): ()| {
                    let probe = unrelated.clone();
                    async move {
                        probe.note("unrelated:start");
                        let gate = probe.unrelated.borrow_mut().take().unwrap();
                        let _ = gate.await;
                        Ok(context.signal().is_cancelled())
                    }
                }),
        )
        .service(
            Service::new("leaf", |_| Ok(())).method("hold", move |_, context, (): ()| {
                let probe = leaf.clone();
                async move {
                    probe.note("hold:start");
                    context.signal().cancelled().await;
                    probe.note("hold:signalled");
                    Ok(context.signal().is_cancelled())
                }
            }),
        )
}

#[test]
fn a_caller_cancels_only_its_issued_call_and_waits_for_its_physical_end() {
    let mut pool = LocalPool::new();
    let host = Host::new(pool.spawner());
    let probe = Rc::new(Probe::default());
    host.install(vec![orb(probe.clone())]).unwrap();
    let worker = Rc::new(host.get("worker", vec!["main".into()], "alice").unwrap());
    let leaf = Rc::new(host.get("leaf", vec!["main".into()], "alice").unwrap());
    *probe.leaf.borrow_mut() = Some(leaf.clone());
    let (release_physical, physical) = oneshot::channel();
    *probe.physical.borrow_mut() = Some(physical);
    let (release_unrelated, unrelated) = oneshot::channel();
    *probe.unrelated.borrow_mut() = Some(unrelated);
    pool.run_until(worker.ready()).unwrap();
    pool.run_until(leaf.ready()).unwrap();

    // One root issues the call it will cancel and, beside it, an unrelated call.
    let issued: Rc<RefCell<Option<IssuedCall>>> = Rc::default();
    let root_signal: Rc<RefCell<Option<tembra::Cancellation>>> = Rc::default();
    let root = {
        let (host, worker, issued, root_signal) = (
            host.clone(),
            worker.clone(),
            issued.clone(),
            root_signal.clone(),
        );
        pool.spawner()
            .spawn_local_with_handle(async move {
                host.root("alice", move |context| {
                    async move {
                        *root_signal.borrow_mut() = Some(context.signal());
                        let unrelated = worker
                            .call_portable::<_, bool>(&context, "unrelated", ())
                            .boxed_local();
                        let (call, result) =
                            worker.issue_portable::<_, ()>(&context, "wait", ()).await?;
                        *issued.borrow_mut() = Some(call);
                        let (unrelated, result) = futures::future::join(unrelated, result).await;
                        Ok((unrelated?, result))
                    }
                    .boxed_local()
                })
                .await
            })
            .unwrap()
    };
    pool.run_until_stalled();
    assert!(probe.logged("hold:start") && probe.logged("unrelated:start"));
    assert_eq!(host.kernel().diagnostics().calls, 3);

    let call = issued.borrow().clone().unwrap();
    call.cancel();
    let settled = pool
        .spawner()
        .spawn_local_with_handle({
            let call = call.clone();
            async move { call.settled().await }
        })
        .unwrap()
        .shared();
    pool.run_until_stalled();
    // Logically cancelled, with the call it issued; physically still running and counted.
    assert!(probe.logged("hold:signalled"));
    assert!(probe.logged("wait:signalled"));
    assert!(probe.logged("wait:nested:true"));
    assert!(probe.logged("wait:fresh:Cancelled"));
    assert!(!probe.logged("wait:end"));
    assert!(settled.clone().now_or_never().is_none());
    assert_eq!(host.kernel().diagnostics().calls, 2);
    // Nothing else in the root was cancelled, and cancelling again does nothing.
    assert!(!root_signal.borrow().as_ref().unwrap().is_cancelled());
    call.cancel();

    release_physical.send(()).unwrap();
    pool.run_until(settled);
    assert!(probe.logged("wait:end"));
    assert_eq!(host.kernel().diagnostics().calls, 1);

    release_unrelated.send(()).unwrap();
    let (unrelated_cancelled, waited) = pool.run_until(root).unwrap();
    assert!(
        !unrelated_cancelled,
        "the unrelated call's signal stayed live"
    );
    assert!(
        waited.is_ok(),
        "the issued call returned after its cancellation"
    );
    assert_eq!(host.kernel().diagnostics().calls, 0);
    drop(worker);
    drop(leaf);
    probe.leaf.borrow_mut().take();
    pool.run_until(host.close()).unwrap();
    let diagnostics = host.kernel().diagnostics();
    assert_eq!((diagnostics.calls, diagnostics.roots), (0, 0));
}
