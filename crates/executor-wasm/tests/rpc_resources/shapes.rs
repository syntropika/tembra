//! Existing resource WIT exercises records, variants and results through both executors.
use super::*;
use tembra::portable::{Issuable, Nominal, Payload};

pub(super) const INTERFACE: &str = "tembra-test:resources/sessions@1.0.0";
const PACE: &str = "tembra-test:resources/pace@1.0.0";

/// Resolve exactly the fixed WIT already qualified by the resource executor tests.
fn contract() -> Contract {
    Contract::from_dir(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/wit/resources"),
        Some("provider"),
    )
    .expect("resource WIT")
}

/// Provide the existing imports; these shape scenarios never need a second suspension gate.
pub(super) fn pace() -> Orb {
    let contract = contract();
    let (_, signature, _) = contract
        .import_requirement(PACE, None)
        .expect("pace import");
    Orb::new("pace", 1).service(
        Service::new(format!("pace::{PACE}"), |_| Ok(()))
            .signature(Some(signature))
            .method("wait", |_, _, (n,): (u64,)| async move { Ok(n) })
            .method("note", |_, _, (_n,): (u64,)| async move { Ok(()) }),
    )
}

/// Select the same existing contract for a native Payload exporter or a compiled provider.
pub(super) fn provider(wasm: bool) -> Result<Orb> {
    let contract = contract();
    if wasm {
        let bytes = std::fs::read(
            std::env::var_os("TEMBRA_RPC_SHAPES_COMPONENT").expect("resource component"),
        )
        .expect("resource component bytes");
        let artifact = contract
            .component_artifact(&bytes, "shapes", 1, &Default::default())
            .expect("resource artifact");
        return Engine::new(Limits::default())?
            .load(&artifact, Install::default().requires(PACE, "pace", 1));
    }
    let closed = Rc::new(Cell::new(0_u64));
    let signature = contract
        .service_signatures()
        .expect("signatures")
        .remove(INTERFACE)
        .expect("sessions");
    let methods = contract
        .model()
        .exports
        .iter()
        .find_map(|entry| match &entry.item {
            tembra_contracts::Item::Interface(interface)
                if interface.contract.as_deref() == Some(INTERFACE) =>
            {
                Some(
                    interface
                        .functions
                        .iter()
                        .map(|function| function.name.clone())
                        .collect::<Vec<_>>(),
                )
            }
            _ => None,
        })
        .expect("sessions methods");
    let mut service = Service::new(format!("shapes::{INTERFACE}"), |_| Ok(()))
        .signature(Some(signature))
        .contract(INTERFACE, &contract.prepared().encode());
    for method in methods {
        let closed = closed.clone();
        service = service.payload_method(method.clone(), move |_, _, payload| {
            let result = serve(&method, payload, &closed);
            async move { result }
        });
    }
    Ok(Orb::new("shapes", 1).service(service))
}

struct Session {
    value: u64,
    closed: Rc<Cell<u64>>,
}
impl Issuable for Session {
    /// Keep the defining interface's exact identity for the native backend.
    fn nominal() -> Nominal {
        Nominal {
            interface: INTERFACE.into(),
            name: "session".into(),
        }
    }
}
impl Drop for Session {
    /// Match the compiled fixture's physical destructor counter.
    fn drop(&mut self) {
        self.closed.set(self.closed.get() + 1);
    }
}
struct Counter(Cell<u64>);
impl Issuable for Counter {
    /// A distinct nominal resource in the same interface cannot become a session.
    fn nominal() -> Nominal {
        Nominal {
            interface: INTERFACE.into(),
            name: "counter".into(),
        }
    }
}

/// Construct and consume containers with the same portable attachment representation as Wasm.
fn serve(method: &str, mut input: Payload, closed: &Rc<Cell<u64>>) -> Result<Payload> {
    let mut output = Payload::data(Value::Null);
    let fresh = |value| Session {
        value,
        closed: closed.clone(),
    };
    let value = match method {
        "open-bundle" | "open-padded" => {
            let (seed, fail, label) = if method == "open-padded" {
                let (seed, padding): (u64, u32) =
                    serde_json::from_value(input.into_data()?).expect("checked WIT input");
                (seed, false, "x".repeat(padding as usize))
            } else {
                let (seed, fail): (u64, bool) =
                    serde_json::from_value(input.into_data()?).expect("checked WIT input");
                (seed, fail, "bundle".into())
            };
            if fail {
                json!({"tag":"err","val":"refused"})
            } else {
                let bundle = json!({
                    "primary": output.issue(fresh(seed)), "optional": output.issue(fresh(seed + 1)),
                    "extras": [output.issue(fresh(seed + 2)), output.issue(fresh(seed + 3))],
                    "count": output.issue(Counter(Cell::new(0))), "seed": seed, "label": label
                });
                if method == "open-padded" {
                    bundle
                } else {
                    json!({"tag":"ok","val":bundle})
                }
            }
        }
        "choose" => {
            let (seed,): (u64,) =
                serde_json::from_value(input.into_data()?).expect("checked WIT input");
            if seed == 0 {
                json!({"tag":"empty"})
            } else {
                json!({"tag":"active","val":output.issue(fresh(seed + 1))})
            }
        }
        "[method]session.total" => {
            let receiver = input.receiver::<Session>()?;
            input.into_data()?;
            json!(receiver.with(|value| value.value)?)
        }
        "[method]counter.hits" => {
            let counter = input.receiver::<Counter>()?;
            input.into_data()?;
            json!(counter.with(|counter| {
                counter.0.set(counter.0.get() + 1);
                counter.0.get()
            })?)
        }
        "take-bundle" => {
            let mut owned: Vec<_> = input.take_arguments().into_iter().map(Some).collect();
            let values = input.value().as_array().expect("checked arguments");
            let record = values[0].as_object().expect("checked record");
            let mut take = |slot: &Value| -> Result<Session> {
                let moved = owned[slot.as_u64().expect("checked slot") as usize]
                    .take()
                    .expect("linear slot");
                input.receive(moved)
            };
            let primary = take(&record["primary"])?;
            let mut totals = vec![primary.value];
            if !record["optional"].is_null() {
                totals.push(take(&record["optional"])?.value);
            }
            for slot in record["extras"].as_array().expect("checked list") {
                totals.push(take(slot)?.value);
            }
            let count = owned[record["count"].as_u64().expect("checked slot") as usize]
                .take()
                .expect("counter slot");
            let counter = input.receive::<Counter>(count)?;
            totals.push(counter.0.get() + 1);
            totals.push(record["seed"].as_u64().expect("checked u64"));
            if owned.iter().any(Option::is_some) {
                return Err(tembra::Error::invalid("arguments", "unused owner"));
            }
            json!(totals)
        }
        "closed" => {
            input.into_data()?;
            json!(closed.get())
        }
        _ => {
            return Err(tembra::Error::new(
                tembra::Code::Unsupported,
                "test",
                "unused shape function",
            ));
        }
    };
    output.map_value(|_| Ok(value))
}
