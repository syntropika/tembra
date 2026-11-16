//! The WIT resource `Accumulator` of `calculator::orb()` in a native host: a native caller
//! constructs one, opens others from the service and from a static function, alone and nested in
//! a list, a tuple and a result, calls their methods through its `Owned` handles, and drops them,
//! each value closing once. A compiled caller does the same in
//! `crates/executor-wasm/tests/orb_macro_native.rs`.
use futures::{FutureExt, executor::LocalPool};
use serde_json::{Value, json};
use std::rc::Rc;
use tembra::{
    Host, Reference,
    core::Code,
    portable::{Delivered, Owned},
};
use tembra_orb_native_fixture::calculator::{self, accumulators::AccumulatorsClient};

const ACCUMULATORS: &str = "calculator::example:calculator/accumulators@1.0.0";
const INTERFACE: &str = "example:calculator/accumulators@1.0.0";

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
