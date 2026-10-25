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
fn candidate_lookup_is_ticket_scoped_and_provider_metadata_is_per_service() {
    let kernel = Kernel::new();
    let execute = |request: serde_json::Value| kernel.execute(&request).unwrap();
    let initial = execute(
        json!({"op":KernelOp::Prepare.as_str(),"facets":[{"name":"a","apiVersion":1,"services":[{"name":"item","methods":["read"]}]}]}),
    );
    execute(json!({"op":KernelOp::Commit.as_str(),"ticket":initial["id"]}));
    for key in ["first", "second"] {
        let acquired = execute(
            json!({"op":KernelOp::Acquire.as_str(),"service":"item","key":[key],"principal":"host"}),
        );
        execute(json!({"op":KernelOp::Ready.as_str(),"object":acquired["object"]}));
    }
    let candidate = execute(
        json!({"op":KernelOp::Prepare.as_str(),"remove":["a"],"facets":[{"name":"b","apiVersion":1,"services":[{"name":"item","methods":["read"]}]}]}),
    );
    assert_eq!(candidate["objects"].as_array().unwrap().len(), 2);
    assert_eq!(candidate["providers"].as_object().unwrap().len(), 1);
    let destination =
        execute(json!({"op":KernelOp::FacetNamed.as_str(),"name":"b","ticket":candidate["id"]}));
    assert_eq!(candidate["providers"]["item"], destination["id"]);
    assert!(
        kernel
            .execute(&json!({"op":KernelOp::FacetNamed.as_str(),"name":"b"}))
            .is_err()
    );
    execute(json!({"op":KernelOp::Abort.as_str(),"ticket":candidate["id"]}));
    assert!(
        kernel
            .execute(
                &json!({"op":KernelOp::FacetNamed.as_str(),"name":"b","ticket":candidate["id"]})
            )
            .is_err()
    );
}
