//! The signal of a call answers the same whenever it is first read: a call that outlives its root,
//! which ended normally, reads its signal uncancelled before and after the root ends, and a call
//! of a root that was cancelled reads it cancelled, also once the root and the call are gone. A
//! root reads its own signal alike.
use futures::{FutureExt, channel::oneshot, executor::LocalPool};
use std::{cell::RefCell, rc::Rc};
use tembra::{Context, Host, Orb, Service};

/// What the running call reports once it read its signal, and the gate it then waits behind,
/// which the test opens once the root has ended.
type Gate = Rc<RefCell<Option<(oneshot::Sender<()>, oneshot::Receiver<()>)>>>;

/// A worker whose `hold` reads its signal, reports it did, waits behind the gate, and reads it
/// again: each read is a first one, as the signal of the first is dropped before the second.
fn orb(gate: Gate) -> Orb {
    Orb::new("calls", 1).service(Service::new("worker", |_| Ok(())).method(
        "hold",
        move |_, context, (): ()| {
            let (read, gate) = gate.borrow_mut().take().unwrap();
            async move {
                let before = context.signal().is_cancelled();
                read.send(()).unwrap();
                let _ = gate.await;
                let after = context.signal().is_cancelled();
                Ok((before, after))
            }
        },
    ))
}

/// The root that issued the call ends normally while the call runs, which reads its signal as
/// uncancelled before the root ends and after it.
#[test]
fn a_call_reads_its_signal_alike_before_and_after_its_root_ends() {
    let mut pool = LocalPool::new();
    let host = Host::new(pool.spawner());
    let gate = Gate::default();
    let (read, first_read) = oneshot::channel();
    let (open, closed) = oneshot::channel();
    *gate.borrow_mut() = Some((read, closed));
    host.install(vec![orb(gate)]).unwrap();
    let worker = Rc::new(host.get("worker", vec!["main".into()], "alice").unwrap());
    pool.run_until(worker.ready()).unwrap();
    let issuer = worker.clone();
    let (issued, result) = pool
        .run_until(host.root("alice", move |context| {
            async move {
                let issued = issuer
                    .issue_portable::<_, (bool, bool)>(&context, "hold", ())
                    .await?;
                first_read.await.unwrap();
                Ok(issued)
            }
            .boxed_local()
        }))
        .unwrap();
    pool.run_until_stalled();
    open.send(()).unwrap();
    assert_eq!(pool.run_until(result).unwrap(), (false, false));
    drop(issued);
    pool.run_until(host.close()).unwrap();
}

/// The root that issued a call is cancelled after the call ended, and then ends: the context the
/// call kept reads its signal cancelled before the root ends and after the kernel forgot the root.
#[test]
fn a_call_of_a_cancelled_root_reads_its_signal_cancelled_after_the_root_ends() {
    let mut pool = LocalPool::new();
    let host = Host::new(pool.spawner());
    let kept: Rc<RefCell<Option<Context>>> = Rc::default();
    let keeper = kept.clone();
    host.install(vec![Orb::new("calls", 1).service(
        Service::new("keeper", |_| Ok(())).method("keep", move |_, context, (): ()| {
            *keeper.borrow_mut() = Some(context);
            async { Ok(()) }
        }),
    )])
    .unwrap();
    let reference = Rc::new(host.get("keeper", vec!["main".into()], "alice").unwrap());
    pool.run_until(reference.ready()).unwrap();
    let (called, held) = (reference.clone(), kept.clone());
    let before = pool
        .run_until(host.root("alice", move |context| {
            async move {
                called.call::<_, ()>(&context, "keep", ()).await?;
                context.cancel()?;
                let call = held.borrow().clone().unwrap();
                Ok(call.signal().is_cancelled())
            }
            .boxed_local()
        }))
        .unwrap();
    let call = kept.borrow_mut().take().unwrap();
    assert_eq!((before, call.signal().is_cancelled()), (true, true));
    drop(call);
    reference.release().unwrap();
    pool.run_until(host.close()).unwrap();
}

/// Run a root that reads its own signal, with no call, and is cancelled or not, as `cancel` says;
/// answer what it read then and what its context reads after the root ended.
fn root_signal(cancel: bool) -> (bool, bool) {
    let mut pool = LocalPool::new();
    let host = Host::new(pool.spawner());
    let (context, before) = pool
        .run_until(host.root("alice", move |context| {
            async move {
                if cancel {
                    context.cancel()?;
                }
                let before = context.signal().is_cancelled();
                Ok((context, before))
            }
            .boxed_local()
        }))
        .unwrap();
    let after = context.signal().is_cancelled();
    pool.run_until(host.close()).unwrap();
    (before, after)
}

/// A root that ended normally reads its own signal as uncancelled before it ends and after it.
#[test]
fn a_root_reads_its_signal_alike_before_and_after_it_ends() {
    assert_eq!(root_signal(false), (false, false));
}

/// A root that was cancelled and then ended reads its own signal as cancelled before it ends and
/// after the kernel forgot it.
#[test]
fn a_cancelled_root_reads_its_signal_cancelled_after_it_ends() {
    assert_eq!(root_signal(true), (true, true));
}
