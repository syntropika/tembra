use std::{
    cell::{Cell, RefCell},
    collections::BTreeMap,
    rc::Rc,
};

use futures::{FutureExt, channel::oneshot};
use serde_json::json;
use tembra_core::{
    contracts::{Contract, Member},
    *,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Action {
    ReleaseFirst,
    ReleaseIndependent,
    CancelReceiver,
    ReturnBody,
    DropPhysical,
}

fn schedules() -> Vec<Vec<Action>> {
    fn extend(prefix: &mut Vec<Action>, remaining: &mut Vec<Action>, out: &mut Vec<Vec<Action>>) {
        if remaining.is_empty() {
            out.push(prefix.clone());
            return;
        }
        for position in 0..remaining.len() {
            let action = remaining.remove(position);
            prefix.push(action);
            extend(prefix, remaining, out);
            prefix.pop();
            remaining.insert(position, action);
        }
    }
    let mut result = Vec::new();
    extend(
        &mut Vec::new(),
        &mut vec![
            Action::ReleaseFirst,
            Action::ReleaseIndependent,
            Action::CancelReceiver,
            Action::ReturnBody,
            Action::DropPhysical,
        ],
        &mut result,
    );
    result
}

#[test]
fn all_release_cancel_return_and_physical_settlement_orders_preserve_owned_cleanup() {
    let orders = schedules();
    assert_eq!(orders.len(), 120);
    for (seed, order) in orders.iter().enumerate() {
        for abandon in [false, true] {
            for body_failure in [false, true] {
                run(seed, order, abandon, body_failure);
            }
        }
    }
}

fn run(seed: usize, order: &[Action], abandon: bool, body_failure: bool) {
    let trace = format!("seed={seed}, abandon={abandon}, failure={body_failure}, {order:?}");
    let driver = ManualDriver::new();
    let host = Host::new(HostOptions::new("schedule", json!(null)), driver.clone());
    let contract = Rc::new(
        Contract::local(
            "owner",
            1,
            json!({}),
            BTreeMap::from([(
                "run".into(),
                Member::Method {
                    args: None,
                    result: None,
                },
            )]),
        )
        .unwrap(),
    );
    let factories = Rc::new(Cell::new(0));
    let closes = Rc::new(Cell::new(0));
    let entered = Rc::new(RefCell::new(None::<Context>));
    let physical = Rc::new(RefCell::new(None::<InvocationLease>));
    let (finish, finished) = oneshot::channel();
    let finished = Rc::new(RefCell::new(Some(finished)));
    let (factory_calls, close_calls, saved_context, saved_lease) = (
        factories.clone(),
        closes.clone(),
        entered.clone(),
        physical.clone(),
    );
    host.serve(
        contract.clone(),
        Rc::new(move |_, _, declaration| {
            factory_calls.set(factory_calls.get() + 1);
            let closed = close_calls.clone();
            declaration.resource(
                "resource",
                ResourceDeclaration::new(|_| async { Ok(()) }.boxed_local()).with_close(
                    move |_| {
                        closed.set(closed.get() + 1);
                        async { Ok(()) }.boxed_local()
                    },
                ),
            )?;
            let owner = declaration.invocation_owner();
            let (context, lease, body) =
                (saved_context.clone(), saved_lease.clone(), finished.clone());
            Ok(Implementation::new().method("run", move |(): (), ctx| {
                *lease.borrow_mut() = Some(owner.retain(&ctx).unwrap());
                *context.borrow_mut() = Some(ctx.clone());
                let body = body.borrow_mut().take().unwrap();
                async move {
                    body.await.unwrap();
                    ctx.check(ctx.host_id())?;
                    if body_failure {
                        Err(RuntimeError::invalid(
                            "body_failure",
                            "scripted body refusal",
                        ))
                    } else {
                        Ok(())
                    }
                }
                .boxed_local()
            }))
        }),
        ServeOptions::default(),
    )
    .unwrap();
    let first = driver
        .run_until(host.get(contract.clone(), json!({}), GetOptions::default()))
        .unwrap();
    let shared = first.clone();
    let independent = host
        .hold(contract, json!({}), GetOptions::default())
        .unwrap();
    assert_eq!(first.object_id(), independent.object_id(), "{trace}");
    assert_eq!(factories.get(), 1, "{trace}");

    let (context_send, context_recv) = oneshot::channel();
    let (end_root, root_ended) = oneshot::channel();
    let parent_host = host.clone();
    driver.spawn(
        async move {
            parent_host
                .root(move |context| {
                    async move {
                        assert!(context_send.send(context).is_ok());
                        root_ended.await.unwrap();
                        Ok(())
                    }
                    .boxed_local()
                })
                .await
                .unwrap();
        }
        .boxed_local(),
    );
    let ancestor = driver.run_until(context_recv).unwrap();
    let tracked = first.call_tracked::<(), ()>("run", (), Some(&ancestor));
    let receipt = tracked.receipt();
    let mut call = Some(tracked.boxed_local());
    assert!(
        call.as_mut().unwrap().as_mut().now_or_never().is_none(),
        "{trace}"
    );
    let receiver = entered.borrow().as_ref().unwrap().clone();
    let mut finish = Some(finish);
    let (mut released_first, mut released_independent) = (false, false);
    let mut outcome = None;
    for action in order {
        match action {
            Action::ReleaseFirst => {
                first.release();
                shared.release();
                released_first = true;
                assert_eq!(shared.status(), RefStatus::Released, "{trace}");
            }
            Action::ReleaseIndependent => {
                independent.release();
                released_independent = true;
            }
            Action::CancelReceiver => {
                receiver.cancel(RuntimeError::aborted(
                    "schedule_cancel",
                    "scripted child cancellation",
                ));
                if abandon {
                    drop(call.take());
                }
            }
            Action::ReturnBody => {
                // A dropped observer may already have dropped the parked Rust method future.
                let _ = finish.take().unwrap().send(());
            }
            Action::DropPhysical => {
                let lease = physical.borrow_mut().take();
                drop(lease);
            }
        }
        let ready = call.as_mut().and_then(|call| call.as_mut().now_or_never());
        if let Some(result) = ready {
            outcome = Some(result);
            drop(call.take());
        }
        driver.run_until_stalled();
        ancestor.check(host.id()).unwrap();
        let should_close =
            released_first && released_independent && call.is_none() && physical.borrow().is_none();
        assert_eq!(
            closes.get(),
            usize::from(should_close),
            "after {action:?}: {trace}"
        );
        if !released_independent {
            assert_eq!(independent.status(), RefStatus::Live, "{trace}");
        }
    }
    assert!(call.is_none(), "{trace}");
    assert_eq!(
        receiver.check(host.id()).unwrap_err().reason,
        "expired",
        "{trace}"
    );
    driver.run_until(receipt.settled());
    assert_eq!(closes.get(), 1, "{trace}");
    if let Some(result) = outcome {
        let cancelled_before_return = order.iter().position(|a| *a == Action::CancelReceiver)
            < order.iter().position(|a| *a == Action::ReturnBody);
        if cancelled_before_return {
            assert_eq!(result.unwrap_err().reason, "schedule_cancel", "{trace}");
        } else if body_failure {
            assert_eq!(result.unwrap_err().reason, "body_failure", "{trace}");
        } else {
            result.unwrap();
        }
    } else {
        assert!(abandon, "{trace}");
    }
    end_root.send(()).unwrap();
    driver.run_until_stalled();
    assert_eq!(
        ancestor.check(host.id()).unwrap_err().reason,
        "expired",
        "{trace}"
    );
    driver.run_until(host.close()).unwrap();
    assert_eq!(closes.get(), 1, "{trace}");
    assert_eq!(factories.get(), 1, "{trace}");
}
