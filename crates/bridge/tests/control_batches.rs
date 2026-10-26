//! Compare composed exchanges with their original command sequence on fresh kernels.
use serde_json::{Value, json};
use tembra_bridge::{Kernel, core::*};

fn setup() -> (Kernel, Value) {
    let kernel = Kernel::new();
    let mut orb = Orb::new("batch", 1);
    orb.services.push(Service::new("batch", ["read"]));
    let prepared = kernel.host.prepare(vec![orb], vec![]).unwrap();
    kernel.host.commit(prepared.id, vec![]).unwrap();
    let claim = kernel
        .host
        .acquire(
            Address {
                service: "batch".into(),
                key: vec!["main".into()],
                principal: "alice".into(),
            },
            1,
            Default::default(),
        )
        .unwrap();
    kernel.host.ready(claim.object).unwrap();
    let root = kernel.host.begin_root("alice").unwrap();
    (
        kernel,
        json!({"reference":claim.reference,"root":root,"method":"read","context":0}),
    )
}

fn individual_admit(kernel: &Kernel, request: &Value) -> Result<Value> {
    kernel
        .execute(&json!({"op":"check-context","root":request["root"],"call":request["parent"]}))?;
    kernel.execute(&json!({"op":"begin-call","reference":request["reference"],"root":request["root"],"method":request["method"],"context":request["context"]}))
}

#[test]
fn admission_preserves_validation_errors_and_kernel_state() {
    for case in [
        "valid",
        "unknown-method",
        "released",
        "cancelled",
        "other-root",
        "invalid-context",
        "wide-id",
        "invalid-parent",
    ] {
        let (individual, mut request) = setup();
        let (batched, _) = setup();
        for kernel in [&individual, &batched] {
            match case {
                "released" => {
                    kernel
                        .execute(&json!({"op":"release","reference":request["reference"]}))
                        .unwrap();
                }
                "cancelled" => {
                    kernel
                        .host
                        .cancel_root(request["root"].as_u64().unwrap())
                        .unwrap();
                }
                "other-root" => {
                    let root = kernel.host.begin_root("alice").unwrap();
                    let parent = kernel
                        .host
                        .begin_call(request["reference"].as_u64().unwrap(), root, "read")
                        .unwrap();
                    request["parent"] = json!(parent.id);
                }
                _ => {}
            }
        }
        match case {
            "unknown-method" => request["method"] = json!("unknown"),
            "invalid-context" => request["context"] = json!(999_999),
            "wide-id" => request["reference"] = json!(1_u64 << 53),
            "invalid-parent" => request["parent"] = json!(0.5),
            _ => {}
        }
        let expected = individual_admit(&individual, &request);
        let actual = batched.admit_call_batch(&request.to_string());
        match (expected, actual) {
            (Ok(expected), Ok(actual)) => assert_eq!(actual, expected, "{case}"),
            (Err(expected), Err(actual)) => {
                assert_eq!(actual.code, expected.code, "{case}");
                assert_eq!(actual.reason, expected.reason, "{case}");
                assert_eq!(actual.message, expected.message, "{case}");
            }
            results => panic!("admission diverged for {case}: {results:?}"),
        }
        assert_eq!(
            batched.execute(&json!({"op":"diagnostics"})).unwrap(),
            individual.execute(&json!({"op":"diagnostics"})).unwrap(),
            "{case}"
        );
    }
}

#[test]
fn admission_applies_metadata_limits_before_executing_any_command() {
    let (kernel, _) = setup();
    for request in ["{".to_string(), " ".repeat(1024 * 1024 + 1)] {
        let expected: Value = serde_json::from_str(&kernel.request(&request)).unwrap();
        let actual = kernel.admit_call_batch(&request).unwrap_err();
        assert_eq!(json!(actual.reason), expected["error"]["reason"]);
        assert_eq!(kernel.host.diagnostics().calls, 0);
    }
}

#[test]
fn finish_preserves_action_order_deadlines_and_wide_clocks() {
    for now in [42, (1_u64 << 53) + 2] {
        let (individual, request) = setup();
        let (batched, _) = setup();
        let call = individual_admit(&individual, &request).unwrap()["id"]
            .as_u64()
            .unwrap();
        batched.admit_call_batch(&request.to_string()).unwrap();
        for kernel in [&individual, &batched] {
            kernel
                .execute(&json!({"op":"release","reference":request["reference"]}))
                .unwrap();
        }
        individual.execute(&json!({"op":"tick","now":now})).unwrap();
        individual
            .execute(&json!({"op":"finish-call","call":call}))
            .unwrap();
        let expected = json!({
            "actions":individual.execute(&json!({"op":"drain"})).unwrap(),
            "deadline":individual.execute(&json!({"op":"next-deadline"})).unwrap(),
        });
        assert_eq!(batched.finish_call_batch(call, now).unwrap(), expected);
        assert_eq!(
            batched.execute(&json!({"op":"diagnostics"})).unwrap(),
            individual.execute(&json!({"op":"diagnostics"})).unwrap()
        );
    }
}
