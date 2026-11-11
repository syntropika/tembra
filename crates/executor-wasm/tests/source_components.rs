//! Prepared source Orbs: SDK-authored TypeScript built into real components against
//! `tembra:orb`, executed by the same component worker as compiled Orbs.
#[path = "support/source.rs"]
mod source;
use serde_json::json;
use source::{Installed, engines, fixture};
use std::sync::{Arc, Mutex};
use tembra_executor_wasm::{Contract, Install};

const COUNTER: &str = "counter::tembra-test:counter/counter@1.0.0";

/// Lifecycle factories, typed state requests, exact u64 values, the caller's principal and
/// console output attributed by the host to the Orb.
#[test]
#[ignore = "requires TEMBRA_WASM_FIXTURES"]
fn a_source_component_serves_state_and_context_through_its_lifecycle() {
    for engine in engines() {
        let records = Arc::new(Mutex::new(Vec::new()));
        let observed = records.clone();
        let engine = engine.on_log(move |record| {
            observed.lock().unwrap().push((
                record.orb.clone(),
                record.level.clone(),
                record.message.clone(),
            ));
        });
        let mut installed = Installed::new(&engine, vec![(fixture("counter"), Install::default())]);
        let counter = installed.reference(COUNTER, "alice");
        let big = u64::MAX - 1;
        assert_eq!(
            installed
                .call(&counter, "alice", "add", json!([big]))
                .unwrap(),
            json!(big)
        );
        assert_eq!(
            installed
                .call(&counter, "alice", "add", json!([1]))
                .unwrap(),
            json!(u64::MAX)
        );
        assert_eq!(
            installed
                .call(&counter, "alice", "whoami", json!([]))
                .unwrap(),
            json!("alice")
        );
        counter.release().unwrap();
        assert!(installed.host.errors().is_empty());
        installed.close(&engine);
        let records = records.lock().unwrap();
        let expected =
            |message: &str| ("counter".to_owned(), "info".to_owned(), message.to_owned());
        assert_eq!(
            *records,
            vec![expected(&format!("add {big}")), expected("add 1")],
            "console output is attributed to the Orb, in call order"
        );
    }
}

/// Remove the Tembra metadata sections appended to a prepared component.
fn bare(bytes: &[u8]) -> Vec<u8> {
    let mut output = bytes[..8].to_vec();
    let mut offset = 8;
    let leb = |offset: &mut usize| {
        let (mut value, mut shift) = (0_usize, 0);
        loop {
            let byte = bytes[*offset];
            *offset += 1;
            value |= usize::from(byte & 0x7f) << shift;
            if byte & 0x80 == 0 {
                return value;
            }
            shift += 7;
        }
    };
    while offset < bytes.len() {
        let start = offset;
        let id = bytes[offset];
        offset += 1;
        let size = leb(&mut offset);
        let body = offset;
        offset += size;
        if id == 0 {
            let mut name = body;
            let length = leb(&mut name);
            if bytes[name..name + length].starts_with(b"tembra:") {
                continue;
            }
        }
        output.extend_from_slice(&bytes[start..offset]);
    }
    output
}

/// The component's own description must match its artifact identity before any factory runs.
#[test]
#[ignore = "requires TEMBRA_WASM_FIXTURES"]
fn a_description_that_differs_from_the_artifact_is_refused_before_factories() {
    let contract = Contract::from_dir(
        concat!(env!("CARGO_MANIFEST_DIR"), "/tests/source/counter"),
        Some("counter-orb"),
    )
    .unwrap();
    let renamed = contract
        .source_component_artifact(&bare(&fixture("counter")), "renamed", 1)
        .unwrap();
    for engine in engines() {
        let Err(error) = engine.load(&renamed, Install::default()) else {
            panic!("mismatched description accepted")
        };
        assert!(error.message.contains("identity differs"), "{error}");
        assert_eq!(engine.instances(), 0);
    }
}
