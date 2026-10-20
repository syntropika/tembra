use std::collections::BTreeMap;

use serde_json::{Value, json};
use tembra_contracts::{input_compatible, output_fits};

// This finite-language interpreter deliberately does not call the runtime validator,
// cleaner or implication helpers. Its grammar is a small, fully specified test oracle.
#[derive(Clone, Debug)]
enum Language {
    Any,
    Empty,
    Integer(i64, i64),
    Enum(Vec<Value>),
    Object {
        properties: BTreeMap<String, Language>,
        required: Vec<String>,
        additional: Option<bool>,
        min: usize,
        max: usize,
    },
    Array {
        items: Box<Language>,
        min: usize,
        max: usize,
    },
}

fn equal(left: &Value, right: &Value) -> bool {
    match (left, right) {
        (Value::Number(left), Value::Number(right)) => left.as_f64() == right.as_f64(),
        (Value::Array(left), Value::Array(right)) => {
            left.len() == right.len() && left.iter().zip(right).all(|(a, b)| equal(a, b))
        }
        (Value::Object(left), Value::Object(right)) => {
            left.len() == right.len()
                && left
                    .iter()
                    .all(|(key, value)| right.get(key).is_some_and(|v| equal(value, v)))
        }
        _ => left == right,
    }
}

impl Language {
    fn schema(&self) -> Value {
        match self {
            Self::Any => json!({}),
            Self::Empty => json!(false),
            Self::Integer(min, max) => json!({"type":"integer","minimum":min,"maximum":max}),
            Self::Enum(values) => json!({"enum":values}),
            Self::Array { items, min, max } => {
                json!({"type":"array","items":items.schema(),"minItems":min,"maxItems":max})
            }
            Self::Object {
                properties,
                required,
                additional,
                min,
                max,
            } => {
                let properties: BTreeMap<_, _> = properties
                    .iter()
                    .map(|(name, item)| (name, item.schema()))
                    .collect();
                let mut schema = json!({
                    "type":"object", "properties":properties, "required":required,
                    "minProperties":min, "maxProperties":max,
                });
                if let Some(additional) = additional {
                    schema["additionalProperties"] = json!(additional);
                }
                schema
            }
        }
    }

    fn accepts(&self, value: &Value) -> bool {
        match self {
            Self::Any => true,
            Self::Empty => false,
            Self::Integer(min, max) => value
                .as_f64()
                .is_some_and(|n| n.fract() == 0.0 && n >= *min as f64 && n <= *max as f64),
            Self::Enum(values) => values.iter().any(|candidate| equal(candidate, value)),
            Self::Array { items, min, max } => value.as_array().is_some_and(|values| {
                (*min..=*max).contains(&values.len()) && values.iter().all(|v| items.accepts(v))
            }),
            Self::Object {
                properties,
                required,
                additional,
                min,
                max,
            } => value.as_object().is_some_and(|values| {
                (*min..=*max).contains(&values.len())
                    && required.iter().all(|key| values.contains_key(key))
                    && values.iter().all(|(key, value)| {
                        properties
                            .get(key)
                            .map_or(*additional != Some(false), |p| p.accepts(value))
                    })
            }),
        }
    }

    fn cleaned(&self, value: &Value) -> Value {
        match (self, value) {
            (
                Self::Object {
                    properties,
                    additional,
                    ..
                },
                Value::Object(values),
            ) => Value::Object(
                values
                    .iter()
                    .filter_map(|(key, value)| match properties.get(key) {
                        Some(property) => Some((key.clone(), property.cleaned(value))),
                        None if *additional == Some(true) => Some((key.clone(), value.clone())),
                        None => None,
                    })
                    .collect(),
            ),
            (Self::Array { items, .. }, Value::Array(values)) => {
                Value::Array(values.iter().map(|value| items.cleaned(value)).collect())
            }
            _ => value.clone(),
        }
    }
}

fn languages() -> Vec<Language> {
    let atoms = [
        Language::Any,
        Language::Empty,
        Language::Integer(-1, 1),
        Language::Integer(0, 1),
        Language::Integer(0, 0),
        Language::Enum(vec![json!(0), json!(1), Value::Null]),
        Language::Enum(vec![json!(false), Value::Null]),
    ];
    let mut result = atoms.to_vec();
    for atom in &atoms {
        for (min, max) in [(0, 2), (1, 2), (0, 1)] {
            result.push(Language::Array {
                items: Box::new(atom.clone()),
                min,
                max,
            });
        }
    }
    for additional in [None, Some(false), Some(true)] {
        for required in [vec![], vec!["a".into()], vec!["a".into(), "b".into()]] {
            for (min, max) in [(0, 3), (1, 3), (2, 2)] {
                for leaf in [&atoms[0], &atoms[3]] {
                    result.push(Language::Object {
                        properties: BTreeMap::from([
                            ("a".into(), leaf.clone()),
                            ("b".into(), Language::Integer(-1, 1)),
                        ]),
                        required: required.clone(),
                        additional,
                        min,
                        max,
                    });
                }
            }
        }
    }
    result
}

fn witnesses() -> Vec<Value> {
    let leaves = [json!(-1), json!(0), json!(1), Value::Null];
    let mut result = vec![
        json!(-2),
        json!(2),
        json!(0.5),
        json!(false),
        json!(true),
        json!("x"),
    ];
    result.extend(leaves.clone());
    result.push(json!([]));
    for first in &leaves {
        result.push(json!([first]));
        for second in &leaves {
            result.push(json!([first, second]));
        }
    }
    // Complete optional key/value product: absence differs from explicit null.
    for a in 0..=leaves.len() {
        for b in 0..=leaves.len() {
            for extra in 0..=leaves.len() {
                let mut object = serde_json::Map::new();
                for (key, index) in [("a", a), ("b", b), ("extra", extra)] {
                    if let Some(value) = leaves.get(index) {
                        object.insert(key.into(), value.clone());
                    }
                }
                result.push(Value::Object(object));
            }
        }
    }
    result.extend([
        json!({"a":[0,1],"b":0}),
        json!({"a":{"nested":null},"b":1,"extra":[]}),
        json!([{"a":0}, {"a":null,"extra":1}]),
    ]);
    result
}

#[test]
fn every_accepted_finite_output_proof_preserves_the_independent_source_language() {
    let languages = languages();
    let values = witnesses();
    let accepted: Vec<Vec<_>> = languages
        .iter()
        .map(|language| values.iter().map(|v| language.accepts(v)).collect())
        .collect();
    let mut proofs = 0;
    for (source_id, source) in languages.iter().enumerate() {
        for (target_id, target) in languages.iter().enumerate() {
            if output_fits(&source.schema(), &target.schema()).is_ok() {
                proofs += 1;
                for (witness, value) in values.iter().enumerate() {
                    assert!(
                        !accepted[source_id][witness] || accepted[target_id][witness],
                        "output proof {source_id}->{target_id}, witness={value}"
                    );
                }
            }
        }
    }
    assert!(proofs > languages.len(), "must exercise nonidentity proofs");
}

#[test]
fn every_accepted_finite_input_proof_survives_both_independent_cleaning_recipes() {
    let languages = languages();
    let values = witnesses();
    let sent: Vec<Vec<_>> = languages
        .iter()
        .map(|source| {
            values
                .iter()
                .map(|raw| source.cleaned(raw))
                .filter(|v| source.accepts(v))
                .collect()
        })
        .collect();
    let mut proofs = 0;
    for (source_id, source) in languages.iter().enumerate() {
        for (target_id, target) in languages.iter().enumerate() {
            if input_compatible(&source.schema(), &target.schema()).is_ok() {
                proofs += 1;
                for sent in &sent[source_id] {
                    let received = target.cleaned(sent);
                    assert!(
                        target.accepts(&received),
                        "input proof {source_id}->{target_id}, sent={sent}, received={received}"
                    );
                }
            }
        }
    }
    assert!(proofs > languages.len(), "must exercise nonidentity proofs");
}
