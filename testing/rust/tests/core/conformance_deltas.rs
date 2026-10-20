use serde_json::{Value, json};
use tembra_core::delta::{DeltaBatch, DeltaError, DeltaOp, PathSegment, apply};

fn key(value: &str) -> PathSegment {
    PathSegment::Key(value.into())
}

#[test]
fn independent_edit_scripts_match_and_every_late_invalid_suffix_is_atomic() {
    let mut scenarios = 0;
    for length in 0..=5 {
        for removed in 0..=length {
            for inserted in 0..=3 {
                let base = json!({
                    "items": (0..length).map(|n| json!({"n":n})).collect::<Vec<_>>(),
                    "flags": {"kept":null,"discard":true},
                    "text":"😀e\u{301}",
                });
                let mut expected = base.clone();
                expected["flags"].as_object_mut().unwrap().remove("discard");
                expected["flags"]["new"] = json!(false);
                let replacement: Vec<_> = (0..inserted).map(|n| json!({"n":100+n})).collect();
                expected["items"]
                    .as_array_mut()
                    .unwrap()
                    .splice(0..removed, replacement.clone());
                let mut prefix = vec![
                    DeltaOp::Remove {
                        path: vec![key("flags"), key("discard")],
                    },
                    DeltaOp::Set {
                        path: vec![key("flags"), key("new")],
                        value: json!(false),
                    },
                    DeltaOp::Splice {
                        path: vec![key("items")],
                        index: 0,
                        delete_count: removed,
                        items: replacement,
                    },
                ];
                if let Some(first) = expected["items"].as_array_mut().unwrap().first_mut() {
                    first["n"] = json!(-1);
                    prefix.push(DeltaOp::Set {
                        path: vec![key("items"), PathSegment::Index(0), key("n")],
                        value: json!(-1),
                    });
                }
                let before = base.clone();
                let valid = DeltaBatch::new(7, 19, prefix.clone()).unwrap();
                assert_eq!(apply(&base, 7, &valid).unwrap(), expected);
                let count = expected["items"].as_array().unwrap().len();
                let invalid = [
                    DeltaOp::Remove {
                        path: vec![key("flags"), key("missing")],
                    },
                    DeltaOp::Set {
                        path: vec![key("text"), key("nested")],
                        value: Value::Null,
                    },
                    DeltaOp::Splice {
                        path: vec![key("items")],
                        index: count + 1,
                        delete_count: 0,
                        items: vec![],
                    },
                    DeltaOp::Splice {
                        path: vec![key("items")],
                        index: count,
                        delete_count: 1,
                        items: vec![],
                    },
                    DeltaOp::Remove {
                        path: vec![key("items"), PathSegment::Index(count)],
                    },
                ];
                for (kind, suffix) in invalid.into_iter().enumerate() {
                    let mut script = prefix.clone();
                    script.push(suffix);
                    let batch = DeltaBatch::new(7, 19, script).unwrap();
                    let encoded = serde_json::to_value(&batch).unwrap();
                    let decoded: DeltaBatch = serde_json::from_value(encoded).unwrap();
                    let error = apply(&base, 7, &decoded).unwrap_err();
                    let operation = match error {
                        DeltaError::InvalidPath { operation, .. }
                        | DeltaError::InvalidSplice { operation, .. } => operation,
                        other => panic!("unexpected late failure: {other:?}"),
                    };
                    assert_eq!(
                        operation,
                        prefix.len(),
                        "length={length}, remove={removed}, insert={inserted}, kind={kind}"
                    );
                    assert_eq!(base, before);
                    assert_eq!(decoded, batch);
                    assert_eq!(decoded.base_revision(), 7);
                    assert_eq!(decoded.result_revision(), 19);
                    // The same untouched authority can still accept the valid prefix.
                    assert_eq!(apply(&base, 7, &valid).unwrap(), expected);
                    scenarios += 1;
                }
            }
        }
    }
    assert_eq!(scenarios, 420);
}
