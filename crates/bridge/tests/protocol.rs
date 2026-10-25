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
