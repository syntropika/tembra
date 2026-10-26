use serde_json::json;
use tembra_bridge::{Kernel, core::*, protocol::*};

#[test]
fn named_commands_preserve_wire_errors_and_unknown_input_has_no_effect() {
    let kernel = Kernel::new();
    let before = kernel.host.diagnostics();
    let result: serde_json::Value =
        serde_json::from_str(&kernel.request(&json!({"op": "scope-activte"}).to_string())).unwrap();
    assert_eq!(result["error"]["code"], Code::Unsupported.as_str());
    assert_eq!(result["error"]["reason"], Reason::Operation.as_str());
    assert_eq!(kernel.host.diagnostics(), before);
    let error = kernel
        .execute(&json!({"op": KernelOp::ScopeActivate.as_str(), "scope": 0}))
        .unwrap_err();
    assert_eq!(error.code, Code::Gone);
    assert_eq!(error.reason, Reason::Scope.as_str());
    assert_eq!(kernel.host.diagnostics(), before);
}

#[test]
fn enum_names_preserve_closed_parsing_and_extensible_failure_reasons() {
    assert_eq!(NativeOp::parse("invoke"), Some(NativeOp::Invoke));
    assert_eq!(EffectType::parse("restore"), Some(EffectType::Restore));
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
            "replacements":[]
        });
        request["deadline"] = json!("invalid");
        assert_eq!(
            kernel.execute(&request).unwrap_err().reason,
            Reason::Metadata.as_str()
        );
        kernel.host.tick(0).unwrap();
        request["deadline"] = json!(50);
        request["now"] = json!("invalid");
        assert_eq!(
            kernel.execute(&request).unwrap_err().reason,
            Reason::Metadata.as_str()
        );
        kernel.host.tick(0).unwrap();
        request["now"] = json!(10_000);
        kernel.execute(&request).unwrap();
        assert_eq!(kernel.host.next_deadline(), Some(10_050));
        request["now"] = json!(9_999);
        assert_eq!(
            kernel.execute(&request).unwrap_err().reason,
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
        kernel
            .execute(&json!({
                "op":op.as_str(),"ticket":reload.id,"deadline":50,"replacements":[]
            }))
            .unwrap();
        assert_eq!(kernel.host.next_deadline(), Some(10_050));
    }
}

#[test]
fn candidate_lookup_is_ticket_scoped_and_provider_metadata_is_per_service() {
    let kernel = Kernel::new();
    let execute = |request: serde_json::Value| kernel.execute(&request).unwrap();
    let initial = execute(
        json!({"op":KernelOp::Prepare.as_str(),"orbs":[{"name":"a","apiVersion":1,"services":[{"name":"item","methods":["read"]}]}]}),
    );
    execute(json!({"op":KernelOp::Commit.as_str(),"ticket":initial["id"]}));
    for key in ["first", "second"] {
        let acquired = execute(
            json!({"op":KernelOp::Acquire.as_str(),"service":"item","key":[key],"principal":"host"}),
        );
        execute(json!({"op":KernelOp::Ready.as_str(),"object":acquired["object"]}));
    }
    let candidate = execute(
        json!({"op":KernelOp::Prepare.as_str(),"remove":["a"],"orbs":[{"name":"b","apiVersion":1,"services":[{"name":"item","methods":["read"]}]}]}),
    );
    assert_eq!(candidate["objects"].as_array().unwrap().len(), 2);
    assert_eq!(candidate["providers"].as_object().unwrap().len(), 1);
    let destination =
        execute(json!({"op":KernelOp::OrbNamed.as_str(),"name":"b","ticket":candidate["id"]}));
    assert_eq!(candidate["providers"]["item"], destination["id"]);
    assert!(
        kernel
            .execute(&json!({"op":KernelOp::OrbNamed.as_str(),"name":"b"}))
            .is_err()
    );
    execute(json!({"op":KernelOp::Abort.as_str(),"ticket":candidate["id"]}));
    assert!(
        kernel
            .execute(&json!({"op":KernelOp::OrbNamed.as_str(),"name":"b","ticket":candidate["id"]}))
            .is_err()
    );
}
