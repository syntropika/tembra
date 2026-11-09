#![cfg(feature = "hooks")]
use tembra_core::{Host, Orb, Requirement};

#[test]
fn hook_contracts_are_checked_at_install_and_provider_replacement() {
    let host = Host::new();
    let provider = |signature: &str| {
        let mut orb = Orb::new("tools", 1);
        orb.hooks.insert("guard".into());
        orb.hook_signatures.insert("guard".into(), signature.into());
        orb
    };
    let mut extension = Orb::new("extension", 1);
    extension.requires.push(Requirement {
        orb: "tools".into(),
        api_version: 1,
        methods: Default::default(),
        signatures: Default::default(),
        hooks: ["guard".into()].into(),
        hook_signatures: [("guard".into(), "wit-a".into())].into(),
    });
    let install = host
        .prepare(vec![provider("wit-a"), extension], vec![])
        .unwrap();
    host.commit(install.id, vec![]).unwrap();
    assert!(host.prepare(vec![provider("wit-b")], vec![]).is_err());
    let compatible = host.prepare(vec![provider("wit-a")], vec![]).unwrap();
    host.abort(compatible.id).unwrap();
}

#[test]
fn a_hook_signature_cannot_name_an_undeclared_hook() {
    let host = Host::new();
    let mut orb = Orb::new("tools", 1);
    orb.hook_signatures.insert("missing".into(), "wit-a".into());
    assert!(host.prepare(vec![orb], vec![]).is_err());
    assert!(host.installed_orbs().is_empty());
}
