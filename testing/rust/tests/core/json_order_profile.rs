use serde_json::{Value, json};
use tembra_core::{
    contracts,
    delta::{self, DeltaBatch, DeltaOp, PathSegment},
};

#[test]
fn delta_operation_order_and_canonical_identity_do_not_depend_on_json_map_backend() {
    let before: Value = serde_json::from_str(r#"{"z":1,"a":1,"m":1}"#).unwrap();
    let after: Value = serde_json::from_str(r#"{"z":2,"a":2,"m":2}"#).unwrap();
    let ops = delta::diff(&before, &after).unwrap();
    let keys: Vec<_> = ops
        .iter()
        .map(|op| match op {
            DeltaOp::Set { path, .. } => match path.as_slice() {
                [PathSegment::Key(key)] => key.as_str(),
                _ => panic!("expected one property path"),
            },
            _ => panic!("expected one replacement per property"),
        })
        .collect();
    assert_eq!(keys, ["a", "m", "z"]);
    let batch = DeltaBatch::new(0, 1, ops).unwrap();
    assert_eq!(delta::apply(&before, 0, &batch).unwrap(), after);
    let schema =
        json!({"type":"object","properties":{"a":{"type":"integer"},"z":{"type":"integer"}}});
    let input: Value = serde_json::from_str(r#"{"z":1,"drop":true,"a":2}"#).unwrap();
    let cleaned = contracts::clean_arguments(&schema, &input).unwrap();
    assert_eq!(cleaned, json!({"a":2,"z":1}));
    assert_eq!(
        contracts::canonical_json(&cleaned).unwrap(),
        r#"{"a":2,"z":1}"#
    );
    // Raw serialization order is deliberately observed rather than assumed canonical.
    println!(
        "raw cleaned object: {}",
        serde_json::to_string(&cleaned).unwrap()
    );
    println!(
        "raw source keys: {:?}",
        input.as_object().unwrap().keys().collect::<Vec<_>>()
    );
}
