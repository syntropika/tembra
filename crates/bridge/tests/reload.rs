use serde_json::json;
use std::collections::BTreeSet;
use tembra_bridge::{Kernel, core::*};

#[test]
fn large_reload_uses_registered_scopes_instead_of_an_oversized_pair_request() {
    let kernel = Kernel::new();
    let host = &kernel.host;
    let mut facet = Facet::new("large", 1);
    facet.services.push(Service::new("large", ["read"]));
    let initial = host.prepare(vec![facet.clone()], vec![]).unwrap();
    host.commit(initial.id, vec![]).unwrap();
    let register = |object, facet| {
        let id = host.allocate_handle().unwrap();
        host.register_scope(Scope {
            id,
            owner: Owner::Object(object),
            facet,
            parents: vec![],
            dependencies: vec![],
            resources: vec![],
            state_version: None,
            migrate: false,
            active: false,
            closing: false,
        })
        .unwrap();
    };
    for index in 0..40_000 {
        let claim = host
            .acquire(
                Address {
                    service: "large".into(),
                    key: vec![index.to_string()],
                    principal: "host".into(),
                },
                1,
                BTreeSet::new(),
            )
            .unwrap();
        host.ready(claim.object).unwrap();
        register(claim.object, claim.facet);
    }
    let reload = host.prepare(vec![facet], vec![]).unwrap();
    for object in &reload.objects {
        register(*object, reload.facets[0].id);
    }
    let request =
        json!({ "op": "scopes-reload-registered", "ticket": reload.id, "deadline": 1000 })
            .to_string();
    assert!(request.len() < 128);
    let answer: serde_json::Value = serde_json::from_str(&kernel.request(&request)).unwrap();
    let program = answer["ok"]
        .as_u64()
        .expect("bounded metadata admitted the large reload") as Id;
    host.run_program(program).unwrap();
    let mut prepared = false;
    loop {
        let actions = host.drain();
        if actions.is_empty() {
            break;
        }
        for action in actions {
            match action {
                Action::Execute { program, effects } => {
                    let request = json!({ "op": "program-complete-step", "program": program, "results": effects.iter().map(|_| json!({"value":null})).collect::<Vec<_>>() }).to_string();
                    let response: serde_json::Value =
                        serde_json::from_str(&kernel.request(&request)).unwrap();
                    assert!(response.get("ok").is_some(), "{response}");
                }
                Action::ProgramPrepared(_) => {
                    prepared = true;
                    host.commit_program(program).unwrap();
                }
                Action::ProgramComplete { error, .. } => assert!(error.is_none(), "{error:?}"),
                _ => {}
            }
        }
    }
    assert!(prepared);
    assert_eq!(host.version(), reload.code_version);
    assert_eq!(host.diagnostics().objects, 40_000);
}
