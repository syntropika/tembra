//! Compare bounded lifecycle batches with their original command sequence on fresh kernels.
use serde_json::{Value, json};
use tembra_bridge::{
    Kernel,
    core::*,
    input,
    wire::{Handle, Wide},
};

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

/// Exchange one JSON command and return its complete envelope.
fn exchange(kernel: &Kernel, op: &str, fields: Value) -> Value {
    let mut request = fields;
    request["op"] = json!(op);
    serde_json::from_str(&kernel.request(&request.to_string())).unwrap()
}

fn individual_admit(kernel: &Kernel, request: &Value) -> Value {
    let checked = exchange(
        kernel,
        "check-context",
        json!({"root":request["root"],"call":request["parent"]}),
    );
    if checked.get("error").is_some() {
        return checked;
    }
    exchange(
        kernel,
        "begin-call",
        json!({"reference":request["reference"],"root":request["root"],"method":request["method"],"context":request["context"]}),
    )
}

fn diagnostics(kernel: &Kernel) -> Value {
    exchange(kernel, "diagnostics", json!({}))
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
                    let released =
                        exchange(kernel, "release", json!({"reference":request["reference"]}));
                    assert!(released.get("ok").is_some(), "{released}");
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
        let mut actual = exchange(&batched, "admit-call", request.clone());
        if case == "invalid-parent" {
            // The batch names its own field; the individual check names `call`.
            assert_eq!(
                actual["error"]["message"],
                "parent: expected a safe unsigned integer identity, found a number"
            );
            actual["error"]["message"] = expected["error"]["message"].clone();
        }
        assert_eq!(actual, expected, "{case}");
        if case == "valid" {
            assert!(actual["ok"]["id"].is_u64(), "{actual}");
        } else {
            assert!(actual["error"]["code"].is_string(), "{case}: {actual}");
        }
        assert_eq!(diagnostics(&batched), diagnostics(&individual), "{case}");
    }
}

#[test]
fn admission_applies_metadata_limits_before_executing_any_command() {
    let (kernel, request) = setup();
    for (text, reason) in [
        ("{".to_string(), Reason::Metadata),
        (" ".repeat(1024 * 1024 + 1), Reason::MetadataSize),
    ] {
        let envelope: Value = serde_json::from_str(&kernel.request(&text)).unwrap();
        assert_eq!(envelope["error"]["reason"], reason.as_str());
        assert_eq!(kernel.host.diagnostics().calls, 0);
    }
    // Structured inputs have no byte length; the decode budget bounds them instead.
    let mut oversized = request;
    oversized["method"] = json!("x".repeat(1024 * 1024 + 1));
    let error = kernel.dispatch("admit-call", oversized).unwrap_err();
    assert_eq!(error.reason, Reason::MetadataSize.as_str());
    assert_eq!(kernel.host.diagnostics().calls, 0);
}

#[test]
fn typed_admission_needs_no_serialization() {
    let (kernel, request) = setup();
    let invocation = kernel
        .run(input::AdmitCall {
            reference: Handle(request["reference"].as_u64().unwrap()),
            root: Handle(request["root"].as_u64().unwrap()),
            parent: None,
            method: "read".into(),
            context: None,
        })
        .unwrap();
    assert_eq!(invocation.method, "read");
    assert_eq!(kernel.host.diagnostics().calls, 1);
}

#[test]
fn finish_preserves_action_order_deadlines_and_wide_clocks() {
    for now in [42, (1_u64 << 53) + 2] {
        let (individual, request) = setup();
        let (batched, _) = setup();
        let (typed, _) = setup();
        let call = individual_admit(&individual, &request)["ok"]["id"]
            .as_u64()
            .unwrap();
        for kernel in [&batched, &typed] {
            let admitted = exchange(kernel, "admit-call", request.clone());
            assert_eq!(admitted["ok"]["id"], call);
        }
        for kernel in [&individual, &batched, &typed] {
            exchange(kernel, "release", json!({"reference":request["reference"]}));
        }
        exchange(&individual, "tick", json!({"now":now}));
        exchange(&individual, "finish-call", json!({"call":call}));
        let expected = json!({"ok":{
            "actions":exchange(&individual, "drain", json!({}))["ok"],
            "deadline":exchange(&individual, "next-deadline", json!({}))["ok"],
        }});
        assert_eq!(
            exchange(
                &batched,
                "finish-call-batch",
                json!({"call":call,"now":now})
            ),
            expected
        );
        let settled = typed
            .run(input::FinishCallBatch {
                call: Handle(call),
                now: Wide(now),
            })
            .unwrap();
        assert_eq!(json!({"ok":settled}), expected);
        assert_eq!(diagnostics(&batched), diagnostics(&individual));
        assert_eq!(diagnostics(&typed), diagnostics(&individual));
    }
}
