use serde_json::json;
use tembra_bridge::{Kernel, core::*, protocol::KernelOp};

#[cfg(not(feature = "hooks"))]
#[test]
fn terminal_observation_is_explicitly_unavailable_without_hooks() {
    let kernel = Kernel::new();
    let before = kernel.host.diagnostics();
    let error = kernel
        .execute(&json!({"op": KernelOp::BeginHookObservation.as_str()}))
        .unwrap_err();
    assert_eq!(error.code, Code::Unsupported);
    assert_eq!(error.reason, Reason::Operation.as_str());
    assert_eq!(kernel.host.diagnostics(), before);
}

#[cfg(feature = "hooks")]
mod enabled {
    use super::*;

    fn install(kernel: &Kernel, name: &str) -> InstalledOrb {
        let mut declaration = Orb::new(name, 1);
        declaration.hooks.insert("terminal".into());
        let reload = kernel.host.prepare(vec![declaration], vec![]).unwrap();
        let orb = reload.orbs[0].clone();
        kernel.host.commit(reload.id, vec![]).unwrap();
        kernel
            .host
            .register_scope(Scope {
                id: kernel.host.allocate_handle().unwrap(),
                owner: Owner::Orb(orb.owner),
                orb: orb.id,
                parents: vec![],
                dependencies: vec![],
                resources: vec![],
                state_version: None,
                migrate: false,
                active: true,
                closing: false,
            })
            .unwrap();
        orb
    }

    #[test]
    fn decode_is_atomic_and_the_kernel_owns_identity_and_single_admission() {
        let kernel = Kernel::new();
        let orb = install(&kernel, "owner");
        let other = install(&kernel, "other");
        let root = kernel.host.begin_root("alice").unwrap();
        let source = kernel
            .host
            .begin_operation(orb.id, root, 400, None)
            .unwrap();
        kernel.host.cancel_root(root).unwrap();
        let request = json!({
            "op": KernelOp::BeginHookObservation.as_str(),
            "source": source.id,
            "orb": orb.id,
            "operation": 401,
            "hook": "terminal",
        });
        for field in ["source", "orb", "operation", "object", "hook"] {
            let mut invalid = request.clone();
            invalid[field] = json!([]);
            let before = kernel.host.diagnostics();
            assert_eq!(
                kernel.execute(&invalid).unwrap_err().reason,
                Reason::Metadata.as_str()
            );
            assert_eq!(kernel.host.diagnostics(), before);
        }
        let mut wrong_owner = request.clone();
        wrong_owner["orb"] = json!(other.id);
        assert_eq!(
            kernel.execute(&wrong_owner).unwrap_err().reason,
            "operation_owner"
        );
        let before = kernel.host.diagnostics();
        let admitted = kernel.execute(&request).unwrap();
        let invocation = &admitted["invocation"];
        let call = invocation["id"].as_u64().unwrap() as Id;
        let observed_root = invocation["root"].as_u64().unwrap() as Id;
        assert_eq!(invocation["orb"], orb.id);
        assert_eq!(invocation["codeVersion"], source.code_version);
        assert_eq!(kernel.host.root_principal(observed_root).unwrap(), "alice");
        assert_eq!(admitted["plan"], json!([]));
        assert_eq!(kernel.host.diagnostics().roots, before.roots + 1);
        assert_eq!(kernel.host.diagnostics().calls, before.calls + 1);
        let after = kernel.host.diagnostics();
        let mut duplicate = request.clone();
        duplicate["operation"] = json!(402);
        assert_eq!(
            kernel.execute(&duplicate).unwrap_err().reason,
            "observation_admitted"
        );
        assert_eq!(kernel.host.diagnostics(), after);
        let mut recursive = request;
        recursive["source"] = json!(call);
        assert_eq!(
            kernel.execute(&recursive).unwrap_err().reason,
            "observation_source"
        );
        assert_eq!(kernel.host.diagnostics(), after);
        kernel.host.finish_call(call).unwrap();
        kernel.host.end_root(observed_root).unwrap();
        kernel.host.finish_call(source.id).unwrap();
        kernel.host.end_root(root).unwrap();
        assert_eq!(kernel.host.diagnostics().roots, 0);
        assert_eq!(kernel.host.diagnostics().calls, 0);
    }
}
