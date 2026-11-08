use super::*;
use crate::model::{MAX_ID, Resource};

/// Install one orb revision and return its stable owner and revision identity.
fn install(host: &Host, isolated: bool) -> (Id, Id) {
    let reload = host.prepare(vec![declaration(isolated)], vec![]).unwrap();
    host.commit(reload.id, vec![]).unwrap();
    (reload.orbs[0].owner, reload.orbs[0].id)
}

/// Build the same logical orb as either a native or an executor-backed revision.
fn declaration(isolated: bool) -> Orb {
    let mut orb = Orb::new("example", 1);
    orb.isolated = isolated;
    orb
}

/// Declare the shared test resource under an orb owner.
fn connection(host: &Host, owner: Id, exclusive: bool) -> Resource {
    host.resource(Owner::Orb(owner), "connection".into(), 1, exclusive, vec![])
        .unwrap()
}

#[test]
fn host_and_native_resources_use_the_shared_realm_and_are_reused_on_reload() {
    let host = Host::new();
    let shared = host
        .resource(Owner::Host, "shared".into(), 1, false, vec![])
        .unwrap();
    assert_eq!(shared.realm, 0);
    let (owner, _) = install(&host, false);
    let first = connection(&host, owner, false);
    assert_eq!(first.realm, 0);
    host.resource_opened(first.id, host.allocate_handle().unwrap())
        .unwrap();
    let reload = host.prepare(vec![declaration(false)], vec![]).unwrap();
    assert_eq!(reload.orbs[0].owner, owner);
    let candidate = connection(&host, owner, false);
    assert_eq!(candidate.id, first.id);
    assert!(candidate.value.is_some());
    host.commit(reload.id, vec![]).unwrap();
    assert_eq!(connection(&host, owner, false).id, first.id);
}

#[test]
fn isolated_revisions_never_reuse_values_from_another_revision() {
    let host = Host::new();
    let (owner, revision) = install(&host, true);
    let first = connection(&host, owner, false);
    assert_eq!(first.realm, revision);
    host.resource_opened(first.id, host.allocate_handle().unwrap())
        .unwrap();
    // An aborted candidate leaves the published revision, whose redeclaration reuses its own value.
    let reload = host.prepare(vec![declaration(true)], vec![]).unwrap();
    host.abort(reload.id).unwrap();
    assert_eq!(connection(&host, owner, false).id, first.id);
    // Every reload is a fresh executor revision with its own realm.
    let reload = host.prepare(vec![declaration(true)], vec![]).unwrap();
    let candidate = connection(&host, owner, false);
    assert_ne!(candidate.id, first.id);
    assert_eq!(candidate.realm, reload.orbs[0].id);
    assert_ne!(candidate.realm, first.realm);
    assert_eq!(candidate.value, None);
    host.commit(reload.id, vec![]).unwrap();
    assert_eq!(connection(&host, owner, false).id, candidate.id);
}

#[test]
fn switching_between_native_and_isolated_revisions_changes_the_realm() {
    let host = Host::new();
    let (owner, _) = install(&host, false);
    let native = connection(&host, owner, false);
    let reload = host.prepare(vec![declaration(true)], vec![]).unwrap();
    let isolated = connection(&host, owner, false);
    assert_ne!(isolated.id, native.id);
    assert_eq!(isolated.realm, reload.orbs[0].id);
    host.commit(reload.id, vec![]).unwrap();
    let reload = host.prepare(vec![declaration(false)], vec![]).unwrap();
    let back = connection(&host, owner, false);
    assert_eq!(back.realm, 0);
    assert_eq!(back.id, native.id);
    host.abort(reload.id).unwrap();
}

#[test]
fn unknown_owners_fail_closed_instead_of_selecting_a_default_realm() {
    let host = Host::new();
    let error = host
        .resource(Owner::Orb(MAX_ID), "connection".into(), 1, false, vec![])
        .unwrap_err();
    assert_eq!(error.reason, Reason::Orb.as_str());
    let error = host
        .resource(Owner::Object(MAX_ID), "connection".into(), 1, false, vec![])
        .unwrap_err();
    assert_eq!(error.reason, Reason::Object.as_str());
}

#[test]
fn aborted_candidates_restore_resources_in_their_original_realm() {
    let host = Host::new();
    let (owner, revision) = install(&host, true);
    let old = connection(&host, owner, true);
    assert_eq!(old.realm, revision);
    let reload = host.prepare(vec![declaration(true)], vec![]).unwrap();
    // The candidate closes the previous exclusive resource before declaring its own.
    assert!(host.close_resource(old.id).unwrap());
    host.resource_closed(old.id).unwrap();
    let candidate = connection(&host, owner, true);
    assert_eq!(candidate.realm, reload.orbs[0].id);
    assert!(host.close_resource(candidate.id).unwrap());
    host.resource_closed(candidate.id).unwrap();
    // Restoration runs while the candidate is still pending, where public derivation
    // would select the candidate realm.
    let restored = host.restore_resource(&old, vec![]).unwrap();
    assert_ne!(restored.id, old.id);
    assert_eq!(restored.realm, old.realm);
    assert_eq!(restored.code_version, old.code_version);
    host.abort(reload.id).unwrap();
    let record = host.inner.borrow().resources[&restored.id].clone();
    assert_eq!(record.realm, revision);
    assert_eq!(record.code_version, old.code_version);
    // After abort the published revision redeclares into the restored record.
    assert_eq!(connection(&host, owner, true).id, restored.id);
}

#[test]
fn restoration_never_reuses_a_live_candidate_record_from_another_realm() {
    let host = Host::new();
    let (owner, revision) = install(&host, true);
    let old = connection(&host, owner, false);
    let reload = host.prepare(vec![declaration(true)], vec![]).unwrap();
    let candidate = connection(&host, owner, false);
    assert_ne!(candidate.id, old.id);
    assert!(host.close_resource(old.id).unwrap());
    host.resource_closed(old.id).unwrap();
    let restored = host.restore_resource(&old, vec![]).unwrap();
    assert_ne!(restored.id, candidate.id);
    assert_eq!(restored.realm, revision);
    host.abort(reload.id).unwrap();
}
