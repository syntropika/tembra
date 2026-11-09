use serde_json::{Value, json};
use tembra_bridge::{Kernel, core::*, input, protocol::*, wire::Handle};

/// Exchange one JSON request and split its envelope.
fn execute(kernel: &Kernel, request: &Value) -> std::result::Result<Value, Value> {
    let envelope: Value = serde_json::from_str(&kernel.request(&request.to_string())).unwrap();
    match envelope.get("error") {
        Some(error) => Err(error.clone()),
        None => Ok(envelope["ok"].clone()),
    }
}

#[test]
fn named_commands_preserve_wire_errors_and_unknown_input_has_no_effect() {
    let kernel = Kernel::new();
    let before = kernel.host.diagnostics();
    let error = execute(&kernel, &json!({"op": "scope-activte"})).unwrap_err();
    assert_eq!(error["code"], Code::Unsupported.as_str());
    assert_eq!(error["reason"], Reason::Operation.as_str());
    assert_eq!(kernel.host.diagnostics(), before);
    let error = execute(
        &kernel,
        &json!({"op": KernelOp::ScopeActivate.as_str(), "scope": 0}),
    )
    .unwrap_err();
    assert_eq!(error["code"], Code::Gone.as_str());
    assert_eq!(error["reason"], Reason::Scope.as_str());
    let typed = kernel
        .run(input::ScopeActivate { scope: Handle(0) })
        .unwrap_err();
    assert_eq!(typed.code, Code::Gone);
    assert_eq!(typed.reason, Reason::Scope.as_str());
    assert_eq!(kernel.host.diagnostics(), before);
}

#[test]
fn inputs_reject_unknown_fields_wrong_types_and_unsafe_identities_before_executing() {
    let kernel = Kernel::new();
    let root = kernel.host.begin_root("alice").unwrap();
    let before = kernel.host.diagnostics();
    for request in [
        json!({"op":"end-root","root":root,"extra":true}),
        json!({"op":"end-root"}),
        json!({"op":"end-root","root":root.to_string()}),
        json!({"op":"end-root","root":1_u64 << 53}),
        json!({"op":"end-root","root":-1}),
        json!({"op":"end-root","root":1.5}),
        json!({"op":"end-root","root":[root]}),
        json!({"op":"end-root","root":null}),
        json!({"op":"tick","now":"1e3"}),
        json!({"op":"tick","now":-1}),
        json!({"op":"acquire","service":"x","key":"main","principal":"p"}),
        json!({"op":"resource","owner":{"kind":"host"},"name":"r","version":1_u64 << 32}),
        json!({"op":"scope-register","scope":1,"owner":{"kind":"user"},"orb":0}),
        json!({"op":"prepare","orbs":[{"name":"a","apiVersion":1,"services":[],"extra":1}]}),
        json!({"op":"prepare","orbs":[{"name":"a","apiVersion":1,"services":[{"name":"s","methods":[],"lifetime":"forever"}]}]}),
        json!({"op":5}),
    ] {
        let error = execute(&kernel, &request).unwrap_err();
        assert_eq!(error["code"], Code::Invalid.as_str(), "{request}");
        assert_eq!(error["reason"], Reason::Metadata.as_str(), "{request}");
        assert_eq!(kernel.host.diagnostics(), before, "{request}");
    }
    let owner = execute(
        &kernel,
        &json!({"op":"scope-register","scope":1,"owner":{"kind":"host","id":1},"orb":0}),
    )
    .unwrap_err();
    assert_eq!(owner["reason"], Reason::Owner.as_str());
    for body in ["[]", "null", "\"end-root\""] {
        let envelope: Value = serde_json::from_str(&kernel.request(body)).unwrap();
        assert_eq!(
            envelope["error"]["reason"],
            Reason::Metadata.as_str(),
            "{body}"
        );
    }
    assert_eq!(kernel.host.diagnostics(), before);
    kernel.host.end_root(root).unwrap();
}

#[test]
fn structured_input_is_bounded_without_a_byte_length() {
    let kernel = Kernel::new();
    let before = kernel.host.diagnostics();
    let scopes = vec![1_u64; 1024 * 1024 + 1];
    let error = kernel
        .dispatch("scopes-cleanup", json!({ "scopes": scopes }))
        .unwrap_err();
    assert_eq!(error.code, Code::Invalid);
    assert_eq!(error.reason, Reason::MetadataSize.as_str());
    let error = kernel
        .dispatch("root", json!({ "principal": "p".repeat(1024 * 1024) }))
        .unwrap_err();
    assert_eq!(error.reason, Reason::MetadataSize.as_str());
    assert_eq!(kernel.host.diagnostics(), before);
}

#[test]
fn context_values_keep_kind_checks_and_owner_issued_opaque_sources() {
    let kernel = Kernel::new();
    let root = kernel.host.begin_root("alice").unwrap();
    let key = |kind: &str| json!({"name":"trace","version":1,"kind":kind});
    let derive = |kind: &str, extra: Value| {
        let mut request = json!({"op":"context-derive","root":root,"key":key(kind)});
        for (name, value) in extra.as_object().unwrap() {
            request[name] = value.clone();
        }
        execute(&kernel, &request)
    };
    for (kind, extra, reason) in [
        ("integer", json!({"value":"1"}), Reason::Context),
        (
            "integer",
            json!({"value":9_007_199_254_740_992_u64}),
            Reason::Context,
        ),
        ("integer", json!({"value":1.5}), Reason::Context),
        ("boolean", json!({"value":1}), Reason::Context),
        ("text", json!({}), Reason::Metadata),
        ("text", json!({"value":"a","source":0}), Reason::Context),
        ("opaque", json!({"value":true}), Reason::Context),
        ("opaque", json!({"source":0}), Reason::Context),
        ("unknown", json!({"value":"a"}), Reason::Metadata),
    ] {
        let error = derive(kind, extra.clone()).unwrap_err();
        assert_eq!(error["reason"], reason.as_str(), "{kind} {extra}");
    }
    let frame = derive("integer", json!({"value":-9_007_199_254_740_991_i64})).unwrap();
    let value = execute(
        &kernel,
        &json!({"op":"context-value","root":root,"context":frame,"key":key("integer")}),
    )
    .unwrap();
    assert_eq!(
        value,
        json!({"kind":"integer","value":-9_007_199_254_740_991_i64})
    );
    kernel.host.end_root(root).unwrap();
}

#[test]
fn enum_names_preserve_closed_parsing_and_extensible_failure_reasons() {
    assert_eq!(NativeOp::parse("invoke"), Some(NativeOp::Invoke));
    assert_eq!(EffectType::parse("restore"), Some(EffectType::Restore));
    assert_eq!(KernelOp::parse("admit-call"), Some(KernelOp::AdmitCall));
    assert_eq!(KernelOp::parse("execute"), None);
    assert_eq!(Code::parse("cancelled"), Some(Code::Cancelled));
    assert_eq!(Code::parse("custom"), None);
    let error = Error::invalid("quota", "quota exceeded");
    assert_eq!(error.reason, "quota");
    assert_eq!(error.message, "quota exceeded");
}

#[test]
fn reload_deadlines_use_the_supplied_clock_without_a_separate_tick() {
    for op in [KernelOp::ScopesReloadRegistered, KernelOp::ScopesReload] {
        let kernel = Kernel::new();
        let reload = kernel
            .host
            .prepare(vec![Orb::new("clock", 1)], vec![])
            .unwrap();
        let mut request = json!({
            "op":op.as_str(),"ticket":reload.id,"deadline":50,"now":10_000,
        });
        if op == KernelOp::ScopesReload {
            request["replacements"] = json!([]);
        }
        request["deadline"] = json!("invalid");
        assert_eq!(
            execute(&kernel, &request).unwrap_err()["reason"],
            Reason::Metadata.as_str()
        );
        kernel.host.tick(0).unwrap();
        request["deadline"] = json!(50);
        request["now"] = json!("invalid");
        assert_eq!(
            execute(&kernel, &request).unwrap_err()["reason"],
            Reason::Metadata.as_str()
        );
        kernel.host.tick(0).unwrap();
        request["now"] = json!(10_000);
        execute(&kernel, &request).unwrap();
        assert_eq!(kernel.host.next_deadline(), Some(10_050));
        request["now"] = json!(9_999);
        assert_eq!(
            execute(&kernel, &request).unwrap_err()["reason"],
            Reason::Clock.as_str()
        );
        kernel.host.tick(10_049).unwrap();
        assert_eq!(kernel.host.next_deadline(), Some(10_050));
    }
}

#[test]
fn reloads_can_still_use_an_explicitly_ticked_clock() {
    for op in [KernelOp::ScopesReloadRegistered, KernelOp::ScopesReload] {
        let kernel = Kernel::new();
        let reload = kernel
            .host
            .prepare(vec![Orb::new("clock", 1)], vec![])
            .unwrap();
        kernel.host.tick(10_000).unwrap();
        let mut request = json!({"op":op.as_str(),"ticket":reload.id,"deadline":"50"});
        if op == KernelOp::ScopesReload {
            request["replacements"] = json!([]);
        }
        execute(&kernel, &request).unwrap();
        assert_eq!(kernel.host.next_deadline(), Some(10_050));
    }
}

#[test]
fn candidate_lookup_is_ticket_scoped_and_provider_metadata_is_per_service() {
    let kernel = Kernel::new();
    let run = |request: Value| execute(&kernel, &request).unwrap();
    let initial = run(
        json!({"op":KernelOp::Prepare.as_str(),"orbs":[{"name":"a","apiVersion":1,"services":[{"name":"item","methods":["read"]}]}]}),
    );
    run(json!({"op":KernelOp::Commit.as_str(),"ticket":initial["id"]}));
    for key in ["first", "second"] {
        let acquired = run(
            json!({"op":KernelOp::Acquire.as_str(),"service":"item","key":[key],"principal":"host"}),
        );
        run(json!({"op":KernelOp::Ready.as_str(),"object":acquired["object"]}));
    }
    let candidate = run(
        json!({"op":KernelOp::Prepare.as_str(),"remove":["a"],"orbs":[{"name":"b","apiVersion":1,"services":[{"name":"item","methods":["read"]}]}]}),
    );
    assert_eq!(candidate["objects"].as_array().unwrap().len(), 2);
    assert_eq!(candidate["providers"].as_object().unwrap().len(), 1);
    let destination =
        run(json!({"op":KernelOp::OrbNamed.as_str(),"name":"b","ticket":candidate["id"]}));
    assert_eq!(candidate["providers"]["item"], destination["id"]);
    assert_eq!(destination["services"][0]["lifetime"], "referenced");
    assert_eq!(destination["services"][0]["signature"], Value::Null);
    assert!(
        execute(
            &kernel,
            &json!({"op":KernelOp::OrbNamed.as_str(),"name":"b"})
        )
        .is_err()
    );
    run(json!({"op":KernelOp::Abort.as_str(),"ticket":candidate["id"]}));
    assert!(
        execute(
            &kernel,
            &json!({"op":KernelOp::OrbNamed.as_str(),"name":"b","ticket":candidate["id"]})
        )
        .is_err()
    );
}

#[test]
fn wide_revisions_require_exact_numbers_or_decimal_text() {
    use tembra_bridge::{
        decode::{Decode, Input},
        wire::Wide,
    };
    assert_eq!(Wide::decode(Input::Float(MAX_ID as f64)).unwrap().0, MAX_ID);
    assert!(Wide::decode(Input::Float((MAX_ID + 1) as f64)).is_err());
    assert_eq!(
        Wide::decode(Input::Text(u64::MAX.to_string())).unwrap().0,
        u64::MAX
    );
    assert_eq!(Wide::decode(Input::UInt(u64::MAX)).unwrap().0, u64::MAX);
}
