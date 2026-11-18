//! `sketch::orb()` in a native host: a state holding an internally tagged enum and a flattened
//! struct, each with an `f64`, which `serde` reads through its buffer, reads back natively as in a
//! component.
use futures::{FutureExt, executor::LocalPool};
use serde_json::{Value, json};
use std::rc::Rc;
use tembra::Host;
use tembra_orb_native_fixture::sketch;

const PAD: &str = "calculator::example:sketch/pad@1.0.0";

/// The state keeps the circle and its scale, the method reads both back, and so does the host.
#[test]
fn a_state_read_through_the_buffer_of_serde_reads_back_its_floats() {
    let mut pool = LocalPool::new();
    let host = Host::new(pool.spawner());
    host.install(vec![sketch::orb().unwrap()]).unwrap();
    let pad = Rc::new(host.get(PAD, vec!["main".into()], "host").unwrap());
    pool.run_until(pad.ready()).unwrap();
    let called = pad.clone();
    let answer = pool
        .run_until(host.root("host", move |context| {
            async move {
                called
                    .call_portable::<_, Value>(&context, "circle", json!([1.5, 0.25]))
                    .await
            }
            .boxed_local()
        }))
        .unwrap();
    assert_eq!(answer, json!([1.5, 0.25]));
    assert_eq!(
        *pad.state::<Value>().unwrap().value().unwrap(),
        json!({ "shape": { "kind": "Circle", "radius": 1.5 }, "scale": 0.25 })
    );
}
