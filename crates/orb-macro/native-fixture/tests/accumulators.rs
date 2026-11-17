//! The WIT resource `Accumulator` of `calculator::orb()` in a native host: a native caller
//! constructs one, opens others from the service and from a static function, alone and nested in
//! a list, a tuple and a result, calls their methods through its `Owned` handles, an async one
//! too, which keeps its `self` across its await and a reload, and drops them, each value closing
//! once; and does it all again through the service's typed client and the accumulator's handle
//! type. A compiled caller does the same in `crates/executor-wasm/tests/orb_macro_native.rs`.
use futures::{FutureExt, executor::LocalPool};
use serde_json::{Value, json};
use std::{cell::Cell, rc::Rc};
use tembra::{
    Host, Orb, Reference, Service,
    core::Code,
    portable::{Delivered, Issuable, Nominal, Owned, Payload},
};
use tembra_orb_native_fixture::calculator::{
    self,
    accumulators::{AccumulatorHandle, Accumulators, AccumulatorsClient},
};

const ACCUMULATORS: &str = "calculator::example:calculator/accumulators@1.0.0";
const INTERFACE: &str = "example:calculator/accumulators@1.0.0";
const ADD_LATER: &str = "[method]accumulator.add-later";

/// Install the Orb in a host and run `test` under a root of the principal `host`, with a
/// reference to the `main` object of the accumulators' service.
fn with_accumulators(
    test: impl AsyncFnOnce(Rc<Reference>, tembra::Context) -> tembra::Result<()> + 'static,
) {
    let mut pool = LocalPool::new();
    let host = Host::new(pool.spawner());
    host.install(vec![calculator::orb().unwrap()]).unwrap();
    let reference = Rc::new(host.get(ACCUMULATORS, vec!["main".into()], "host").unwrap());
    let called = reference.clone();
    pool.run_until(host.root("host", move |context| test(called, context).boxed_local()))
        .unwrap();
    reference.release().unwrap();
    pool.run_until(host.close()).unwrap();
}

/// A native caller constructs an accumulator through its constructor and through a static
/// function, adds to it and reads it through its handle, as a method that reads its call does, and
/// drops it: the value closes with its last owner, once.
#[test]
fn a_native_caller_constructs_calls_and_drops_an_accumulator() {
    with_accumulators(async |reference, context| {
        let client = AccumulatorsClient::new(reference.clone());
        let dropped = client.dropped(&context).await?;
        let accumulator: Owned = reference
            .call_portable(&context, "[constructor]accumulator", (5_i64,))
            .await?;
        assert_eq!(accumulator.nominal().interface, INTERFACE);
        assert_eq!(accumulator.nominal().name, "accumulator");
        let added: i64 = accumulator
            .call_portable(&context, "[method]accumulator.add", (2_i64,))
            .await?;
        assert_eq!(added, 7);
        let total: i64 = accumulator
            .call_portable(&context, "[method]accumulator.total", ())
            .await?;
        assert_eq!(total, 7);
        // The method runs in the scope of its call, which the kernel's `context` reads.
        let reader: String = accumulator
            .call_portable(&context, "[method]accumulator.reader", ())
            .await?;
        assert_eq!(reader, "host");
        assert_eq!(client.dropped(&context).await?, dropped);
        drop(accumulator);
        assert_eq!(client.dropped(&context).await?, dropped + 1);
        let zero: Owned = reference
            .call_portable(&context, "[static]accumulator.zero", json!([]))
            .await?;
        let total: i64 = zero
            .call_portable(&context, "[method]accumulator.total", json!([]))
            .await?;
        assert_eq!(total, 0);
        drop(zero);
        assert_eq!(client.dropped(&context).await?, dropped + 2);
        Ok(())
    });
}

/// A service function's result holds its accumulators where the WIT does, in a list in a tuple or
/// in a result, as the indices of the call's attachments, in order; each is an owned handle of its
/// own value, closed with it.
#[test]
fn a_service_function_hands_out_accumulators_nested_in_its_result() {
    with_accumulators(async |reference, context| {
        let client = AccumulatorsClient::new(reference.clone());
        let dropped = client.dropped(&context).await?;
        let opened: Delivered = reference
            .call_portable(&context, "open", (vec![10_i64, 20],))
            .await?;
        assert_eq!(opened.value, json!([[0, 1], 2]));
        assert_eq!(opened.owned.len(), 2);
        let mut totals = Vec::new();
        for (accumulator, value) in opened.owned.iter().zip([1_i64, 2]) {
            totals.push(
                accumulator
                    .call_portable::<_, i64>(&context, "[method]accumulator.add", (value,))
                    .await?,
            );
        }
        assert_eq!(totals, [11, 22]);
        drop(opened);
        assert_eq!(client.dropped(&context).await?, dropped + 2);
        let checked: Delivered = reference
            .call_portable(&context, "checked", (3_i64,))
            .await?;
        assert_eq!(checked.value, json!({ "tag": "ok", "val": 0 }));
        assert_eq!(checked.owned.len(), 1);
        drop(checked);
        let refused: Delivered = reference
            .call_portable(&context, "checked", (-1_i64,))
            .await?;
        assert_eq!(
            refused.value,
            json!({ "tag": "err", "val": "-1 is negative" })
        );
        assert!(refused.owned.is_empty());
        assert_eq!(client.dropped(&context).await?, dropped + 3);
        Ok(())
    });
}

/// A function handing out a resource has only its portable entrypoint: a local call is refused,
/// before the function runs.
#[test]
fn a_function_handing_out_an_accumulator_has_no_local_entrypoint() {
    with_accumulators(async |reference, context| {
        let client = AccumulatorsClient::new(reference.clone());
        let dropped = client.dropped(&context).await?;
        let refused = reference
            .call::<_, calculator::accumulators::Accumulator>(
                &context,
                "[constructor]accumulator",
                (1_i64,),
            )
            .await
            .err()
            .expect("a local call is refused");
        assert_eq!(refused.code, Code::Invalid, "{refused}");
        assert_eq!(client.dropped(&context).await?, dropped);
        Ok(())
    });
}

/// A method of the resource runs on the object of the service that issued it, in the scopes of a
/// method of the service, as the executor gives a component's: it adds to the calculator's
/// memory, a resource of the Orb's root, and counts in that object's state, which the host reads.
#[test]
fn a_method_of_the_resource_reaches_its_objects_state_and_the_roots_resources() {
    with_accumulators(async |reference, context| {
        let accumulator: Owned = reference
            .call_portable(&context, "[constructor]accumulator", (5_i64,))
            .await?;
        let stored: (i64, u32) = accumulator
            .call_portable(&context, "[method]accumulator.store", ())
            .await?;
        assert_eq!(stored, (5, 1));
        accumulator
            .call_portable::<_, i64>(&context, "[method]accumulator.add", (2_i64,))
            .await?;
        let stored: (i64, u32) = accumulator
            .call_portable(&context, "[method]accumulator.store", ())
            .await?;
        assert_eq!(stored, (12, 2));
        assert_eq!(*reference.state::<Value>()?.value()?, json!({ "count": 2 }));
        Ok(())
    });
}

/// The service's client covers its resource: it constructs an accumulator and calls the resource's
/// static function, each answering an `AccumulatorHandle`, whose methods, the async one too, take
/// and return the Rust values the compiler checks; and the service's functions that hand
/// accumulators out nested, in a list in a tuple and in a result, answer handles in their place,
/// each naming its own value. Each value closes with its handle.
#[test]
fn the_client_constructs_calls_and_receives_accumulators_as_typed_handles() {
    with_accumulators(async |reference, context| {
        let client = AccumulatorsClient::new(reference);
        let dropped = client.dropped(&context).await?;
        let accumulator: AccumulatorHandle = client.new_accumulator(&context, 5).await?;
        assert_eq!(accumulator.add(&context, 2).await?, 7);
        assert_eq!(accumulator.add_later(&context, 3).await?, (7, 10));
        assert_eq!(accumulator.total(&context).await?, 10);
        assert_eq!(accumulator.reader(&context).await?, "host");
        let zero: AccumulatorHandle = client.accumulator_zero(&context).await?;
        assert_eq!(zero.total(&context).await?, 0);
        let (opened, count): (Vec<AccumulatorHandle>, u32) =
            client.open(&context, vec![10, 20]).await?;
        assert_eq!(count, 2);
        assert_eq!(opened[1].add(&context, 2).await?, 22);
        assert_eq!(opened[0].total(&context).await?, 10);
        let checked: Result<AccumulatorHandle, String> = client.checked(&context, 3).await?;
        let checked = checked.expect("3 is not negative");
        assert_eq!(checked.total(&context).await?, 3);
        assert_eq!(
            client.checked(&context, -1).await?.err().as_deref(),
            Some("-1 is negative")
        );
        assert_eq!(client.dropped(&context).await?, dropped);
        drop((accumulator, zero, opened, checked));
        assert_eq!(client.dropped(&context).await?, dropped + 5);
        Ok(())
    });
}

/// A ticket: a resource of another interface, which a native Orb written by hand issues. Its drop
/// counts the tickets closed.
struct Ticket(Rc<Cell<u32>>);

impl Issuable for Ticket {
    fn nominal() -> Nominal {
        Nominal {
            interface: "example:tickets/tickets@1.0.0".into(),
            name: "ticket".into(),
        }
    }
}

impl Drop for Ticket {
    fn drop(&mut self) {
        self.0.set(self.0.get() + 1);
    }
}

/// `AccumulatorHandle::try_from` wraps an owned accumulator, and gives an owned handle of another
/// resource type back as its error, still open, so its caller can try another handle type; and
/// `Owned::from` gives the owned handle of an accumulator back.
#[test]
fn a_handle_gives_back_an_owned_handle_of_another_resource_type() {
    let closed = Rc::new(Cell::new(0));
    let issued = closed.clone();
    let tickets =
        Orb::new("tickets", 1).service(Service::new("tickets", |_| Ok(())).payload_method(
            "issue",
            move |_, _, payload: Payload| {
                let ticket = Ticket(issued.clone());
                async move {
                    payload.into_data()?;
                    let mut result = Payload::data(Value::Null);
                    let ticket = result.issue(ticket);
                    result.encode(ticket)
                }
            },
        ));
    let mut pool = LocalPool::new();
    let host = Host::new(pool.spawner());
    host.install(vec![calculator::orb().unwrap(), tickets])
        .unwrap();
    let accumulators = Rc::new(host.get(ACCUMULATORS, vec!["main".into()], "host").unwrap());
    let tickets = Rc::new(host.get("tickets", vec!["main".into()], "host").unwrap());
    let (called, issuer, watched) = (accumulators.clone(), tickets.clone(), closed.clone());
    pool.run_until(host.root("host", move |context| {
        async move {
            let ticket: Owned = issuer.call_portable(&context, "issue", ()).await?;
            let Err(ticket) = AccumulatorHandle::try_from(ticket) else {
                panic!("a ticket is no accumulator");
            };
            assert_eq!(ticket.nominal().name, "ticket");
            assert_eq!(watched.get(), 0, "given back open");
            drop(ticket);
            assert_eq!(watched.get(), 1, "closed by its owner");
            let accumulator: Owned = called
                .call_portable(&context, "[constructor]accumulator", (5_i64,))
                .await?;
            let Ok(accumulator) = AccumulatorHandle::try_from(accumulator) else {
                panic!("an accumulator");
            };
            assert_eq!(accumulator.total(&context).await?, 5);
            assert_eq!(Owned::from(accumulator).nominal().name, "accumulator");
            Ok(())
        }
        .boxed_local()
    }))
    .unwrap();
    accumulators.release().unwrap();
    tickets.release().unwrap();
    pool.run_until(host.close()).unwrap();
}

/// An async method of the resource reads its `self` before and after its await: held there, the
/// call waits, and once released it adds to the value it read before.
#[test]
fn an_async_method_reads_its_self_before_and_after_its_await() {
    let mut pool = LocalPool::new();
    let host = Host::new(pool.spawner());
    host.install(vec![calculator::orb().unwrap()]).unwrap();
    let reference = Rc::new(host.get(ACCUMULATORS, vec!["main".into()], "host").unwrap());
    let called = reference.clone();
    tembra_orb_native_fixture::hold_additions();
    let added = host
        .spawn_root("host", move |context| {
            async move {
                let accumulator: Owned = called
                    .call_portable(&context, "[constructor]accumulator", (5_i64,))
                    .await?;
                let added: (i64, i64) = accumulator
                    .call_portable(&context, ADD_LATER, (2_i64,))
                    .await?;
                let total: i64 = accumulator
                    .call_portable(&context, "[method]accumulator.total", ())
                    .await?;
                Ok((added, total))
            }
            .boxed_local()
        })
        .unwrap();
    pool.run_until_stalled();
    assert_eq!(tembra_orb_native_fixture::held_additions(), 1, "held");
    tembra_orb_native_fixture::release_additions();
    assert_eq!(pool.run_until(added).unwrap().unwrap(), ((5, 7), 7));
    reference.release().unwrap();
    pool.run_until(host.close()).unwrap();
}

/// A reload during a call of an async method of the resource lets the call finish on the value it
/// holds. The retired revision closes the value once the call has ended, and a later call through
/// the handle answers `Gone/resource`, as for a compiled exporter.
#[test]
fn a_reload_during_an_async_method_lets_it_finish_and_retires_its_handle() {
    let mut pool = LocalPool::new();
    let host = Host::new(pool.spawner());
    host.install(vec![calculator::orb().unwrap()]).unwrap();
    let reference = Rc::new(host.get(ACCUMULATORS, vec!["main".into()], "host").unwrap());
    let called = reference.clone();
    let accumulator: Owned = pool
        .run_until(host.root("host", move |context| {
            async move {
                called
                    .call_portable(&context, "[constructor]accumulator", (5_i64,))
                    .await
            }
            .boxed_local()
        }))
        .unwrap();
    let dropped = Accumulators::dropped();
    tembra_orb_native_fixture::hold_additions();
    let added = host
        .spawn_root("host", move |context| {
            async move {
                let added = accumulator
                    .call_portable::<_, (i64, i64)>(&context, ADD_LATER, (2_i64,))
                    .await;
                Ok((added, accumulator))
            }
            .boxed_local()
        })
        .unwrap();
    pool.run_until_stalled();
    assert_eq!(tembra_orb_native_fixture::held_additions(), 1, "held");
    pool.run_until(host.reload(vec![calculator::orb().unwrap()], 5_000))
        .unwrap();
    pool.run_until_stalled();
    assert_eq!(Accumulators::dropped(), dropped, "the call holds its value");
    tembra_orb_native_fixture::release_additions();
    let (added, accumulator) = pool.run_until(added).unwrap().unwrap();
    assert_eq!(added.unwrap(), (5, 7));
    pool.run_until(host.settled());
    assert_eq!(
        Accumulators::dropped(),
        dropped + 1,
        "closed with the retired revision"
    );
    let (refused, accumulator) = pool
        .run_until(host.root("host", move |context| {
            async move {
                let refused = accumulator
                    .call_portable::<_, (i64, i64)>(&context, ADD_LATER, (1_i64,))
                    .await
                    .unwrap_err();
                Ok((refused, accumulator))
            }
            .boxed_local()
        }))
        .unwrap();
    assert_eq!(
        (refused.code, refused.reason.as_str()),
        (Code::Gone, "resource"),
        "{refused}"
    );
    drop(accumulator);
    assert_eq!(Accumulators::dropped(), dropped + 1, "closed once");
    reference.release().unwrap();
    pool.run_until(host.close()).unwrap();
}
