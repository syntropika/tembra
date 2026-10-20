use serde_json::{Value, json};
use tembra_core::contracts::canonical_json;
use tembra_core::delta::{
    CountField, DeltaBatch, DeltaError, DeltaOp, PathError, PathSegment, RevisionField, ValueKind,
    ValueSource, apply, diff,
};

fn key(value: &str) -> PathSegment {
    PathSegment::Key(value.to_owned())
}

fn replay(before: &Value, after: &Value) {
    let original = before.clone();
    let expected = canonical_json(after).unwrap();
    let operations = diff(before, after).unwrap();
    let batch = DeltaBatch::new(3, 11, operations).unwrap();
    let encoded = serde_json::to_value(&batch).unwrap();
    let decoded: DeltaBatch = serde_json::from_value(encoded).unwrap();
    let candidate = apply(before, 3, &decoded).unwrap();
    assert_eq!(canonical_json(&candidate).unwrap(), expected);
    assert_eq!(before, &original);
    assert_eq!(decoded, batch);
    assert_eq!(batch.base_revision(), 3);
    assert_eq!(batch.result_revision(), 11);
}

#[test]
fn structural_replay_covers_missing_null_unicode_and_nested_container_transitions() {
    let corpus = [
        Value::Null,
        json!(false),
        json!(1),
        json!(1.0),
        json!(-0.0),
        json!(0),
        json!(""),
        json!("a😀e\u{301}終"),
        json!("😀aé\u{0}"),
        json!([]),
        json!([null]),
        json!([1, {"name": "λ", "items": [null, true]}, "end"]),
        json!([1, {"name": "μ", "items": [false]}, "end", 7]),
        json!({}),
        json!({"missing": null}),
        json!({"0": [0, 1], "a/b~c": {"é": "e\u{301}"}}),
        json!({"__proto__": {"ok": true}, "safe": {"constructor": 1}}),
        json!({"__proto__": null, "safe": {"prototype": [1, 2]}}),
    ];
    for before in &corpus {
        for after in &corpus {
            replay(before, after);
        }
    }
}

fn generated(seed: u64, depth: usize) -> Value {
    if depth == 0 {
        return match seed % 6 {
            0 => Value::Null,
            1 => json!((seed / 6).is_multiple_of(2)),
            2 => json!(seed % 17),
            3 => json!((seed % 17) as f64 / 4.0),
            4 => json!("😀e\u{301}é\u{0}"),
            _ => json!(format!("value {seed}")),
        };
    }
    let next = |index: u64| {
        seed.wrapping_mul(6364136223846793005)
            .wrapping_add(index + 1)
    };
    match seed % 4 {
        0 => (0..seed % 5)
            .map(|index| generated(next(index), depth - 1))
            .collect(),
        1 => json!({"left": generated(next(1), depth - 1), "right": generated(next(2), depth - 1)}),
        2 => json!({"0": generated(next(3), depth - 1), "a/b~c": generated(next(4), depth - 1)}),
        _ => generated(next(5), 0),
    }
}

#[test]
fn independently_generated_trees_replay_in_both_directions_without_canonical_byte_changes() {
    for seed in 0..128 {
        let before = generated(seed, 4);
        let after = generated(seed.wrapping_mul(17).wrapping_add(9), 4);
        replay(&before, &after);
        replay(&after, &before);
        assert!(diff(&before, &before).unwrap().is_empty());
    }
}

#[test]
fn profile_numeric_equality_produces_no_operations_even_inside_reserved_data() {
    for (before, after) in [
        (json!(1), json!(1.0)),
        (json!(-0.0), json!(0)),
        (json!({"numbers": [1, -0.0]}), json!({"numbers": [1.0, 0]})),
        (json!({"constructor": -0.0}), json!({"constructor": 0})),
    ] {
        assert!(diff(&before, &after).unwrap().is_empty());
        replay(&before, &after);
    }
}

#[test]
fn diff_is_structural_and_uses_safe_ancestor_replacement_for_reserved_keys() {
    let before = json!({"items": [{"x": 1}], "safe": {"constructor": 1}, "sibling": 4});
    let after = json!({"items": [{"x": 2}], "safe": {"constructor": 2}, "sibling": 4});
    let ops = diff(&before, &after).unwrap();
    assert!(ops.contains(&DeltaOp::Set {
        path: vec![key("items"), PathSegment::Index(0), key("x")],
        value: json!(2),
    }));
    assert!(ops.contains(&DeltaOp::Set {
        path: vec![key("safe")],
        value: json!({"constructor": 2}),
    }));
    assert_eq!(
        diff(&json!({"__proto__": 1}), &json!({"__proto__": 2})).unwrap(),
        vec![DeltaOp::Set {
            path: vec![],
            value: json!({"__proto__": 2})
        }]
    );
    replay(&before, &after);
}

#[test]
fn arrays_use_exact_splices_and_follow_indices_after_earlier_removal() {
    let before = json!(["prefix", 1, 2, "suffix"]);
    let after = json!(["prefix", 7, "suffix"]);
    assert_eq!(
        diff(&before, &after).unwrap(),
        vec![DeltaOp::Splice {
            path: vec![],
            index: 1,
            delete_count: 2,
            items: vec![json!(7)],
        }]
    );
    let batch = DeltaBatch::new(
        0,
        1,
        vec![
            DeltaOp::Remove {
                path: vec![PathSegment::Index(0)],
            },
            DeltaOp::Set {
                path: vec![PathSegment::Index(0), key("x")],
                value: json!(8),
            },
            DeltaOp::Set {
                path: vec![PathSegment::Index(1)],
                value: json!(null),
            },
            DeltaOp::Splice {
                path: vec![],
                index: 1,
                delete_count: 0,
                items: vec![json!("new")],
            },
        ],
    )
    .unwrap();
    assert_eq!(
        apply(&json!([0, {"x": 1}]), 0, &batch).unwrap(),
        json!([{"x": 8}, "new", null])
    );
}

#[test]
fn a_later_invalid_operation_discards_all_earlier_candidate_edits() {
    let base = json!({"items": [1, 2], "keep": true});
    let original = base.clone();
    let batch = DeltaBatch::new(
        2,
        5,
        vec![
            DeltaOp::Set {
                path: vec![key("new")],
                value: json!("accepted only privately"),
            },
            DeltaOp::Splice {
                path: vec![key("items")],
                index: 0,
                delete_count: 1,
                items: vec![],
            },
            DeltaOp::Remove {
                path: vec![key("missing")],
            },
        ],
    )
    .unwrap();
    let saved = batch.clone();
    assert_eq!(
        apply(&base, 2, &batch),
        Err(DeltaError::InvalidPath {
            operation: 2,
            segment: 0,
            error: PathError::MissingKey {
                key: "missing".into()
            },
        })
    );
    assert_eq!(base, original);
    assert_eq!(batch, saved);
    assert_eq!(
        apply(&base, 2, &DeltaBatch::new(2, 5, vec![]).unwrap()).unwrap(),
        base
    );
}

#[test]
fn revisions_are_safe_exact_bases_with_strictly_greater_results_and_skipped_epochs() {
    const MAX: u64 = 9_007_199_254_740_991;
    let batch = DeltaBatch::new(0, MAX, vec![]).unwrap();
    assert_eq!(apply(&Value::Null, 0, &batch).unwrap(), Value::Null);
    assert_eq!(
        apply(&Value::Null, 2, &batch),
        Err(DeltaError::BaseRevisionMismatch {
            expected: 0,
            actual: 2
        })
    );
    for (base, result) in [(4, 4), (4, 2)] {
        assert_eq!(
            DeltaBatch::new(base, result, vec![]),
            Err(DeltaError::InvalidRevisionOrder {
                base_revision: base,
                result_revision: result
            })
        );
    }
    assert_eq!(
        DeltaBatch::new(MAX + 1, MAX + 2, vec![]),
        Err(DeltaError::UnsafeRevision {
            field: RevisionField::Base,
            value: MAX + 1
        })
    );
    assert_eq!(
        DeltaBatch::new(0, MAX + 1, vec![]),
        Err(DeltaError::UnsafeRevision {
            field: RevisionField::Result,
            value: MAX + 1
        })
    );
    assert_eq!(
        apply(&Value::Null, MAX + 1, &batch),
        Err(DeltaError::UnsafeRevision {
            field: RevisionField::Current,
            value: MAX + 1
        })
    );
}

#[test]
fn typed_paths_refuse_wrong_containers_missing_parents_and_sparse_writes() {
    for (base, path, expected) in [
        (
            json!({"0": 1}),
            vec![PathSegment::Index(0)],
            ValueKind::Array,
        ),
        (json!([1]), vec![key("0")], ValueKind::Object),
        (
            json!({"x": null}),
            vec![key("x"), key("child")],
            ValueKind::Object,
        ),
    ] {
        let batch = DeltaBatch::new(
            0,
            1,
            vec![DeltaOp::Set {
                path,
                value: json!(2),
            }],
        )
        .unwrap();
        assert!(
            matches!(apply(&base, 0, &batch), Err(DeltaError::InvalidPath {
            error: PathError::WrongContainer { expected: kind, .. }, ..
        }) if kind == expected)
        );
    }
    for op in [
        DeltaOp::Set {
            path: vec![key("missing"), key("child")],
            value: json!(2),
        },
        DeltaOp::Remove {
            path: vec![key("missing")],
        },
        DeltaOp::Splice {
            path: vec![key("missing")],
            index: 0,
            delete_count: 0,
            items: vec![],
        },
    ] {
        let batch = DeltaBatch::new(0, 1, vec![op]).unwrap();
        assert!(matches!(
            apply(&json!({}), 0, &batch),
            Err(DeltaError::InvalidPath {
                error: PathError::MissingKey { .. },
                ..
            })
        ));
    }
    for op in [
        DeltaOp::Set {
            path: vec![PathSegment::Index(2)],
            value: json!(7),
        },
        DeltaOp::Remove {
            path: vec![PathSegment::Index(1)],
        },
    ] {
        let batch = DeltaBatch::new(0, 1, vec![op]).unwrap();
        assert!(matches!(
            apply(&json!([1]), 0, &batch),
            Err(DeltaError::InvalidPath {
                error: PathError::IndexOutOfBounds { .. },
                ..
            })
        ));
    }
}

#[test]
fn splice_bounds_are_not_clamped_and_large_counts_do_not_overflow() {
    for (index, delete_count) in [(3, 0), (1, 2), (2, 1)] {
        let batch = DeltaBatch::new(
            0,
            1,
            vec![DeltaOp::Splice {
                path: vec![],
                index,
                delete_count,
                items: vec![],
            }],
        )
        .unwrap();
        assert_eq!(
            apply(&json!([1, 2]), 0, &batch),
            Err(DeltaError::InvalidSplice {
                operation: 0,
                index,
                delete_count,
                length: 2
            })
        );
    }
    let huge = DeltaBatch::new(
        0,
        1,
        vec![DeltaOp::Splice {
            path: vec![],
            index: usize::MAX,
            delete_count: usize::MAX,
            items: vec![],
        }],
    );
    if usize::BITS > 53 {
        assert!(matches!(
            huge,
            Err(DeltaError::UnsafeCount {
                field: CountField::SpliceIndex,
                ..
            })
        ));
    } else {
        assert!(matches!(
            apply(&json!([]), 0, &huge.unwrap()),
            Err(DeltaError::InvalidSplice { .. })
        ));
    }
    let non_array = DeltaBatch::new(
        0,
        1,
        vec![DeltaOp::Splice {
            path: vec![],
            index: 0,
            delete_count: 0,
            items: vec![],
        }],
    )
    .unwrap();
    assert!(matches!(
        apply(&json!("😀"), 0, &non_array),
        Err(DeltaError::InvalidPath {
            error: PathError::WrongContainer {
                actual: ValueKind::String,
                ..
            },
            ..
        })
    ));
}

#[test]
fn serde_rejects_unknown_fields_and_cannot_bypass_checked_batch_construction() {
    let valid = json!({"base_revision":0,"result_revision":2,"ops":[{"op":"set","path":[{"kind":"key","value":"a"}],"value":null}]});
    let batch: DeltaBatch = serde_json::from_value(valid.clone()).unwrap();
    assert_eq!(apply(&json!({}), 0, &batch).unwrap(), json!({"a": null}));
    for corrupt in [
        json!({"base_revision":0,"result_revision":2,"ops":[],"extra":true}),
        json!({"base_revision":2,"result_revision":2,"ops":[]}),
        json!({"base_revision":0,"result_revision":9007199254740992_u64,"ops":[]}),
        json!({"base_revision":0,"result_revision":2,"ops":[{"op":"set","path":[],"value":0,"extra":true}]}),
        json!({"base_revision":0,"result_revision":2,"ops":[{"op":"remove","path":[]}]}),
        json!({"base_revision":0,"result_revision":2,"ops":[{"op":"remove","path":[{"kind":"key","value":"prototype"}]}]}),
        json!({"base_revision":0,"result_revision":2,"ops":[{"op":"set","path":[{"kind":"index","value":-1}],"value":0}]}),
        json!({"base_revision":0,"result_revision":2,"ops":[{"op":"set","path":[{"kind":"key","value":"x","extra":true}],"value":0}]}),
        json!({"base_revision":0,"result_revision":2,"ops":[{"op":"unknown","path":[]}]}),
    ] {
        assert!(
            serde_json::from_value::<DeltaBatch>(corrupt.clone()).is_err(),
            "{corrupt}"
        );
    }
}

fn nested_arrays(containers: usize) -> Value {
    (0..containers).fold(Value::Null, |value, _| Value::Array(vec![value]))
}

#[test]
fn portable_values_and_combined_candidate_depth_are_checked_before_escape() {
    let unsafe_number = json!(9_007_199_254_740_992_u64);
    assert!(matches!(
        diff(&unsafe_number, &Value::Null),
        Err(DeltaError::NonPortable {
            source: ValueSource::Before,
            ..
        })
    ));
    assert!(matches!(
        diff(&Value::Null, &unsafe_number),
        Err(DeltaError::NonPortable {
            source: ValueSource::After,
            ..
        })
    ));
    assert!(matches!(
        DeltaBatch::new(
            0,
            1,
            vec![DeltaOp::Set {
                path: vec![],
                value: unsafe_number.clone()
            }]
        ),
        Err(DeltaError::NonPortable {
            source: ValueSource::Operation {
                operation: 0,
                item: None
            },
            ..
        })
    ));
    assert!(matches!(
        DeltaBatch::new(
            0,
            1,
            vec![DeltaOp::Splice {
                path: vec![],
                index: 0,
                delete_count: 0,
                items: vec![unsafe_number.clone()]
            }]
        ),
        Err(DeltaError::NonPortable {
            source: ValueSource::Operation { item: Some(0), .. },
            ..
        })
    ));
    let empty = DeltaBatch::new(0, 1, vec![]).unwrap();
    assert!(matches!(
        apply(&unsafe_number, 0, &empty),
        Err(DeltaError::NonPortable {
            source: ValueSource::Current,
            ..
        })
    ));
    assert!(diff(&Value::Null, &nested_arrays(129)).is_err());
    let base = json!({"child": 0});
    let original = base.clone();
    let batch = DeltaBatch::new(
        0,
        1,
        vec![DeltaOp::Set {
            path: vec![key("child")],
            value: nested_arrays(128),
        }],
    )
    .unwrap();
    assert!(matches!(
        apply(&base, 0, &batch),
        Err(DeltaError::NonPortable {
            source: ValueSource::Candidate,
            ..
        })
    ));
    assert_eq!(base, original);
    replay(&nested_arrays(128), &nested_arrays(128));
}

#[test]
fn constructor_rejects_reserved_paths_overlong_paths_and_root_removal() {
    for reserved in ["__proto__", "constructor", "prototype"] {
        assert!(matches!(
            DeltaBatch::new(
                0,
                1,
                vec![DeltaOp::Set {
                    path: vec![key("safe"), key(reserved)],
                    value: Value::Null
                }]
            ),
            Err(DeltaError::InvalidPath {
                segment: 1,
                error: PathError::ReservedKey { .. },
                ..
            })
        ));
    }
    assert_eq!(
        DeltaBatch::new(0, 1, vec![DeltaOp::Remove { path: vec![] }]),
        Err(DeltaError::RemoveRoot { operation: 0 })
    );
    assert_eq!(
        DeltaBatch::new(
            0,
            1,
            vec![DeltaOp::Remove {
                path: vec![key("x"); 129]
            }]
        ),
        Err(DeltaError::PathTooDeep {
            operation: 0,
            length: 129,
            maximum: 128
        })
    );
}
