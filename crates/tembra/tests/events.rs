#![cfg(feature = "subscriptions")]
use futures::{channel::oneshot, executor::LocalPool};
use std::{
    cell::{Cell, RefCell},
    rc::Rc,
};
use tembra::{EventHandler, EventOptions, EventSource, Host, Orb, Service};

type TestSource<T> = (
    EventSource<T>,
    Rc<RefCell<Option<EventHandler<T>>>>,
    Rc<Cell<usize>>,
);
fn source<T: 'static>() -> TestSource<T> {
    let callback = Rc::new(RefCell::new(None));
    let captured = callback.clone();
    let detached = Rc::new(Cell::new(0));
    let counted = detached.clone();
    let source = EventSource::new(move |handle| {
        *captured.borrow_mut() = Some(handle);
        let captured = captured.clone();
        let counted = counted.clone();
        Ok(Box::new(move || {
            captured.borrow_mut().take();
            counted.set(counted.get() + 1);
        }))
    });
    (source, callback, detached)
}

#[test]
fn bounded_events_detach_on_overflow_and_close_waits_for_the_active_handler() {
    use std::{future::Future, task::Poll};
    let mut pool = LocalPool::new();
    let host = Host::new(pool.spawner());
    let (source, emit, detached) = source::<String>();
    let seen = Rc::new(RefCell::new(Vec::new()));
    let captured = seen.clone();
    let (send, receive) = oneshot::channel();
    let receive = Rc::new(RefCell::new(Some(receive)));
    host.install(vec![Orb::new("events", 1).service(Service::new(
        "events",
        move |self_| {
            let captured = captured.clone();
            let receive = receive.clone();
            self_.on(
                "input",
                source.clone(),
                EventOptions::default()
                    .pending(1)
                    .bytes(4, |value: &String| Ok(value.len())),
                move |ctx, value| {
                    captured.borrow_mut().push(value.to_string());
                    assert_eq!(ctx.principal().unwrap(), "alice");
                    let receive = receive.borrow_mut().take();
                    async move {
                        if let Some(receive) = receive {
                            receive.await.unwrap();
                        }
                        Ok(())
                    }
                },
            )?;
            Ok(())
        },
    ))])
    .unwrap();
    let reference = host.get("events", vec!["main".into()], "alice").unwrap();
    pool.run_until(reference.ready()).unwrap();
    let publish = emit.borrow().clone().unwrap();
    publish(Rc::new("one".into()));
    pool.run_until_stalled();
    publish(Rc::new("two".into()));
    publish(Rc::new("three".into()));
    assert_eq!(detached.get(), 1);
    assert_eq!(host.errors()[0].reason, "subscription_queue");
    let mut closing = Box::pin(host.close());
    pool.run_until(futures::future::poll_fn(|ctx| {
        assert!(closing.as_mut().poll(ctx).is_pending());
        Poll::Ready(())
    }));
    assert_eq!(host.kernel().diagnostics().calls, 1);
    send.send(()).unwrap();
    pool.run_until(closing).unwrap();
    assert_eq!(*seen.borrow(), ["one"]);
    assert_eq!(detached.get(), 1);
}

#[test]
fn coalescing_delivers_the_latest_queued_value_and_reports_handler_errors() {
    let mut pool = LocalPool::new();
    let host = Host::new(pool.spawner());
    let (source, emit, detached) = source::<i32>();
    let seen = Rc::new(RefCell::new(Vec::new()));
    let captured = seen.clone();
    host.install(vec![Orb::new("events", 1).service(Service::new(
        "events",
        move |self_| {
            let captured = captured.clone();
            self_.on(
                "input",
                source.clone(),
                EventOptions::default().coalesce(),
                move |_, value| {
                    captured.borrow_mut().push(*value);
                    async { Err(tembra::Error::invalid("event", "handler failed")) }
                },
            )?;
            Ok(())
        },
    ))])
    .unwrap();
    let reference = host.get("events", vec!["main".into()], "alice").unwrap();
    pool.run_until(reference.ready()).unwrap();
    let publish = emit.borrow().clone().unwrap();
    publish(Rc::new(1));
    publish(Rc::new(2));
    publish(Rc::new(3));
    pool.run_until_stalled();
    assert_eq!(*seen.borrow(), [1, 3]);
    assert_eq!(host.errors().len(), 2);
    reference.release().unwrap();
    pool.run_until(host.close()).unwrap();
    assert_eq!(detached.get(), 1);
}
