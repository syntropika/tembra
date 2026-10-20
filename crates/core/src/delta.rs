//! Checked structural deltas over portable JSON values.
//!
//! Paths distinguish object keys from array indices. Strings are replaced whole,
//! so replay never depends on a UTF-8, UTF-16, or code-point offset convention.
//! This is Tembra's encoding, without Chord wire compression or CRDT semantics.

use std::collections::BTreeSet;
use std::fmt;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tembra_contracts::{ContractError, validate};

const MAX_SAFE_INTEGER: u64 = 9_007_199_254_740_991;
const MAX_PATH_LENGTH: usize = 128;
const RESERVED_KEYS: [&str; 3] = ["__proto__", "constructor", "prototype"];

/// An object property or an array index, without numeric-string coercion.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    content = "value",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum PathSegment {
    Key(String),
    Index(usize),
}

/// An untrusted operation description, checked by [`DeltaBatch::new`].
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub enum DeltaOp {
    /// Set the root at an empty path; otherwise insert an object key or set an
    /// existing array index. Setting exactly the array length appends one item.
    Set {
        path: Vec<PathSegment>,
        value: Value,
    },
    /// Remove an existing object key or array element. The path cannot be empty.
    Remove { path: Vec<PathSegment> },
    /// Replace an exact array range. No clamping or sparse extension is allowed.
    Splice {
        path: Vec<PathSegment>,
        index: usize,
        delete_count: usize,
        items: Vec<Value>,
    },
}

/// Identifies a revision involved in a refused batch or replay.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RevisionField {
    Base,
    Result,
    Current,
}

/// Identifies the borrowed value or operation payload that is not portable.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ValueSource {
    Before,
    After,
    Current,
    Operation {
        operation: usize,
        item: Option<usize>,
    },
    Candidate,
}

/// A JSON kind used in wrong-container diagnostics.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ValueKind {
    Null,
    Boolean,
    Number,
    String,
    Array,
    Object,
}

impl ValueKind {
    fn of(value: &Value) -> Self {
        match value {
            Value::Null => Self::Null,
            Value::Bool(_) => Self::Boolean,
            Value::Number(_) => Self::Number,
            Value::String(_) => Self::String,
            Value::Array(_) => Self::Array,
            Value::Object(_) => Self::Object,
        }
    }
}

/// Explains which array count exceeds the portable integer domain.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CountField {
    PathIndex { segment: usize },
    SpliceIndex,
    SpliceDeleteCount,
}

/// A failed path traversal, with the exact key, kind, or bounds involved.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PathError {
    ReservedKey {
        key: String,
    },
    MissingKey {
        key: String,
    },
    WrongContainer {
        expected: ValueKind,
        actual: ValueKind,
    },
    IndexOutOfBounds {
        index: usize,
        length: usize,
        allows_end: bool,
    },
}

/// A refused delta. Failure never modifies a borrowed value or batch.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DeltaError {
    UnsafeRevision {
        field: RevisionField,
        value: u64,
    },
    InvalidRevisionOrder {
        base_revision: u64,
        result_revision: u64,
    },
    BaseRevisionMismatch {
        expected: u64,
        actual: u64,
    },
    NonPortable {
        source: ValueSource,
        error: ContractError,
    },
    PathTooDeep {
        operation: usize,
        length: usize,
        maximum: usize,
    },
    UnsafeCount {
        operation: usize,
        field: CountField,
        value: usize,
    },
    InvalidPath {
        operation: usize,
        segment: usize,
        error: PathError,
    },
    RemoveRoot {
        operation: usize,
    },
    InvalidSplice {
        operation: usize,
        index: usize,
        delete_count: usize,
        length: usize,
    },
}

impl fmt::Display for DeltaError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnsafeRevision { field, value } => {
                write!(
                    formatter,
                    "{field:?} revision {value} exceeds the safe integer range"
                )
            }
            Self::InvalidRevisionOrder {
                base_revision,
                result_revision,
            } => write!(
                formatter,
                "result revision {result_revision} must exceed base revision {base_revision}"
            ),
            Self::BaseRevisionMismatch { expected, actual } => {
                write!(
                    formatter,
                    "delta requires revision {expected}, received {actual}"
                )
            }
            Self::NonPortable { source, error } => {
                write!(formatter, "nonportable {source:?}: {error}")
            }
            Self::PathTooDeep {
                operation,
                length,
                maximum,
            } => write!(
                formatter,
                "operation {operation} path length {length} exceeds {maximum}"
            ),
            Self::UnsafeCount {
                operation,
                field,
                value,
            } => write!(
                formatter,
                "operation {operation} {field:?} count {value} exceeds the safe integer range"
            ),
            Self::InvalidPath {
                operation,
                segment,
                error,
            } => write!(
                formatter,
                "operation {operation} path segment {segment}: {error:?}"
            ),
            Self::RemoveRoot { operation } => {
                write!(formatter, "operation {operation} cannot remove the root")
            }
            Self::InvalidSplice {
                operation,
                index,
                delete_count,
                length,
            } => write!(
                formatter,
                "operation {operation} splice {index}+{delete_count} exceeds array length {length}"
            ),
        }
    }
}

impl std::error::Error for DeltaError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::NonPortable { error, .. } => Some(error),
            _ => None,
        }
    }
}

/// Checked metadata and operation syntax for replay against one exact revision.
///
/// Deserialization runs the same checks as [`Self::new`]. Container membership and
/// range validity depend on the actual base and are checked by [`apply`].
/// External JSON text must first pass [`tembra_contracts::parse_json`], preserving
/// duplicate-key and numeric-lexeme checks that an already decoded value cannot supply.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(try_from = "BatchData")]
pub struct DeltaBatch {
    base_revision: u64,
    result_revision: u64,
    ops: Vec<DeltaOp>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct BatchData {
    base_revision: u64,
    result_revision: u64,
    ops: Vec<DeltaOp>,
}

impl TryFrom<BatchData> for DeltaBatch {
    type Error = DeltaError;

    fn try_from(data: BatchData) -> Result<Self, Self::Error> {
        Self::new(data.base_revision, data.result_revision, data.ops)
    }
}

impl DeltaBatch {
    /// Check revisions, paths, counts, and portable operation payloads.
    ///
    /// Base zero is valid. The result must be strictly greater, but may skip
    /// epochs. Empty batches are allowed; state publication decides whether a
    /// no-op deserves an event.
    ///
    /// # Errors
    /// Rejects unsafe or nonincreasing revisions, overlong/reserved paths, unsafe
    /// array counts, root removal, or nonportable payloads. Existing containers
    /// and the final candidate's combined nesting are checked only by [`apply`].
    pub fn new(
        base_revision: u64,
        result_revision: u64,
        ops: Vec<DeltaOp>,
    ) -> Result<Self, DeltaError> {
        check_revision(RevisionField::Base, base_revision)?;
        check_revision(RevisionField::Result, result_revision)?;
        if result_revision <= base_revision {
            return Err(DeltaError::InvalidRevisionOrder {
                base_revision,
                result_revision,
            });
        }
        for (operation, op) in ops.iter().enumerate() {
            check_operation(operation, op)?;
        }
        Ok(Self {
            base_revision,
            result_revision,
            ops,
        })
    }

    pub fn base_revision(&self) -> u64 {
        self.base_revision
    }

    pub fn result_revision(&self) -> u64 {
        self.result_revision
    }

    pub fn ops(&self) -> &[DeltaOp] {
        &self.ops
    }
}

fn check_revision(field: RevisionField, value: u64) -> Result<(), DeltaError> {
    if value > MAX_SAFE_INTEGER {
        return Err(DeltaError::UnsafeRevision { field, value });
    }
    Ok(())
}

fn portable(value: &Value, source: ValueSource) -> Result<(), DeltaError> {
    validate(&Value::Bool(true), value).map_err(|error| DeltaError::NonPortable { source, error })
}

fn count(operation: usize, field: CountField, value: usize) -> Result<(), DeltaError> {
    if value as u128 > u128::from(MAX_SAFE_INTEGER) {
        return Err(DeltaError::UnsafeCount {
            operation,
            field,
            value,
        });
    }
    Ok(())
}

fn path_error(operation: usize, segment: usize, error: PathError) -> DeltaError {
    DeltaError::InvalidPath {
        operation,
        segment,
        error,
    }
}

fn check_operation(operation: usize, op: &DeltaOp) -> Result<(), DeltaError> {
    let path = match op {
        DeltaOp::Set { path, .. } | DeltaOp::Remove { path } | DeltaOp::Splice { path, .. } => path,
    };
    if path.len() > MAX_PATH_LENGTH {
        return Err(DeltaError::PathTooDeep {
            operation,
            length: path.len(),
            maximum: MAX_PATH_LENGTH,
        });
    }
    for (segment, part) in path.iter().enumerate() {
        match part {
            PathSegment::Key(key) if RESERVED_KEYS.contains(&key.as_str()) => {
                return Err(path_error(
                    operation,
                    segment,
                    PathError::ReservedKey { key: key.clone() },
                ));
            }
            PathSegment::Index(value) => {
                count(operation, CountField::PathIndex { segment }, *value)?;
            }
            _ => {}
        }
    }
    match op {
        DeltaOp::Set { value, .. } => {
            portable(
                value,
                ValueSource::Operation {
                    operation,
                    item: None,
                },
            )?;
        }
        DeltaOp::Remove { path } if path.is_empty() => {
            return Err(DeltaError::RemoveRoot { operation });
        }
        DeltaOp::Splice {
            index,
            delete_count,
            items,
            ..
        } => {
            count(operation, CountField::SpliceIndex, *index)?;
            count(operation, CountField::SpliceDeleteCount, *delete_count)?;
            for (item, value) in items.iter().enumerate() {
                portable(
                    value,
                    ValueSource::Operation {
                        operation,
                        item: Some(item),
                    },
                )?;
            }
        }
        _ => {}
    }
    Ok(())
}

/// Replay a whole batch against its exact base revision, returning an owned candidate.
///
/// # Guarantees
/// Neither `value` nor `batch` changes on success or failure, even when a later
/// operation fails after earlier operations succeeded on the private candidate.
/// Success returns a portable value; it does not publish state or advance any
/// caller-owned revision. Reserved keys are legal data in replacements, but not
/// path segments, keeping the encoding safe for JavaScript consumers.
///
/// # Errors
/// Rejects an unsafe/current-mismatched revision, nonportable base or candidate,
/// missing path/key, wrong container, out-of-bounds index or splice. All array
/// ranges are exact: no clamping, holes, or automatic parent creation occurs.
///
/// ```
/// use serde_json::json;
/// use tembra_core::delta::{apply, DeltaBatch, DeltaError, DeltaOp, PathSegment};
/// let base = json!({"items":[1,2]});
/// let original = base.clone();
/// let batch = DeltaBatch::new(0, 4, vec![
///     DeltaOp::Set { path: vec![PathSegment::Key("accepted".into())], value: json!(true) },
///     DeltaOp::Remove { path: vec![PathSegment::Key("missing".into())] },
/// ])?;
/// assert!(matches!(apply(&base, 0, &batch), Err(DeltaError::InvalidPath { operation: 1, .. })));
/// assert_eq!(base, original);
/// let empty = DeltaBatch::new(0, 4, vec![])?;
/// assert_eq!(apply(&base, 0, &empty)?, original);
/// # Ok::<(), DeltaError>(())
/// ```
pub fn apply(
    value: &Value,
    current_revision: u64,
    batch: &DeltaBatch,
) -> Result<Value, DeltaError> {
    check_revision(RevisionField::Current, current_revision)?;
    if current_revision != batch.base_revision {
        return Err(DeltaError::BaseRevisionMismatch {
            expected: batch.base_revision,
            actual: current_revision,
        });
    }
    portable(value, ValueSource::Current)?;
    let mut candidate = value.clone();
    for (operation, op) in batch.ops.iter().enumerate() {
        apply_operation(&mut candidate, operation, op)?;
    }
    portable(&candidate, ValueSource::Candidate)?;
    Ok(candidate)
}

fn descend<'a>(
    mut value: &'a mut Value,
    path: &[PathSegment],
    operation: usize,
) -> Result<&'a mut Value, DeltaError> {
    for (segment, part) in path.iter().enumerate() {
        let actual = ValueKind::of(value);
        value = match part {
            PathSegment::Key(key) => {
                let Value::Object(object) = value else {
                    return Err(path_error(
                        operation,
                        segment,
                        PathError::WrongContainer {
                            expected: ValueKind::Object,
                            actual,
                        },
                    ));
                };
                object.get_mut(key).ok_or_else(|| {
                    path_error(
                        operation,
                        segment,
                        PathError::MissingKey { key: key.clone() },
                    )
                })?
            }
            PathSegment::Index(index) => {
                let Value::Array(array) = value else {
                    return Err(path_error(
                        operation,
                        segment,
                        PathError::WrongContainer {
                            expected: ValueKind::Array,
                            actual,
                        },
                    ));
                };
                let length = array.len();
                array.get_mut(*index).ok_or_else(|| {
                    path_error(
                        operation,
                        segment,
                        PathError::IndexOutOfBounds {
                            index: *index,
                            length,
                            allows_end: false,
                        },
                    )
                })?
            }
        };
    }
    Ok(value)
}

fn apply_operation(value: &mut Value, operation: usize, op: &DeltaOp) -> Result<(), DeltaError> {
    match op {
        DeltaOp::Set {
            path,
            value: replacement,
        } => {
            let Some((last, parents)) = path.split_last() else {
                *value = replacement.clone();
                return Ok(());
            };
            let parent = descend(value, parents, operation)?;
            let actual = ValueKind::of(parent);
            match (last, parent) {
                (PathSegment::Key(key), Value::Object(object)) => {
                    object.insert(key.clone(), replacement.clone());
                }
                (PathSegment::Index(index), Value::Array(array)) if *index <= array.len() => {
                    if *index == array.len() {
                        array.push(replacement.clone());
                    } else {
                        array[*index] = replacement.clone();
                    }
                }
                (PathSegment::Index(index), Value::Array(array)) => {
                    return Err(path_error(
                        operation,
                        parents.len(),
                        PathError::IndexOutOfBounds {
                            index: *index,
                            length: array.len(),
                            allows_end: true,
                        },
                    ));
                }
                (part, _) => return Err(wrong_container(operation, parents.len(), part, actual)),
            }
        }
        DeltaOp::Remove { path } => {
            let Some((last, parents)) = path.split_last() else {
                return Err(DeltaError::RemoveRoot { operation });
            };
            let parent = descend(value, parents, operation)?;
            let actual = ValueKind::of(parent);
            match (last, parent) {
                (PathSegment::Key(key), Value::Object(object)) => {
                    if object.remove(key).is_none() {
                        return Err(path_error(
                            operation,
                            parents.len(),
                            PathError::MissingKey { key: key.clone() },
                        ));
                    }
                }
                (PathSegment::Index(index), Value::Array(array)) if *index < array.len() => {
                    array.remove(*index);
                }
                (PathSegment::Index(index), Value::Array(array)) => {
                    return Err(path_error(
                        operation,
                        parents.len(),
                        PathError::IndexOutOfBounds {
                            index: *index,
                            length: array.len(),
                            allows_end: false,
                        },
                    ));
                }
                (part, _) => return Err(wrong_container(operation, parents.len(), part, actual)),
            }
        }
        DeltaOp::Splice {
            path,
            index,
            delete_count,
            items,
        } => {
            let target = descend(value, path, operation)?;
            let actual = ValueKind::of(target);
            let Value::Array(array) = target else {
                return Err(path_error(
                    operation,
                    path.len(),
                    PathError::WrongContainer {
                        expected: ValueKind::Array,
                        actual,
                    },
                ));
            };
            if *index > array.len() || *delete_count > array.len() - index {
                return Err(DeltaError::InvalidSplice {
                    operation,
                    index: *index,
                    delete_count: *delete_count,
                    length: array.len(),
                });
            }
            array.splice(*index..index + delete_count, items.iter().cloned());
        }
    }
    Ok(())
}

fn wrong_container(
    operation: usize,
    segment: usize,
    part: &PathSegment,
    actual: ValueKind,
) -> DeltaError {
    let expected = match part {
        PathSegment::Key(_) => ValueKind::Object,
        PathSegment::Index(_) => ValueKind::Array,
    };
    path_error(
        operation,
        segment,
        PathError::WrongContainer { expected, actual },
    )
}

/// Produce deterministic structural operations whose replay equals `after` in
/// the portable value profile. Numbers compare as binary64: `1 == 1.0` and
/// `-0.0 == 0`, so representational differences produce no operations.
///
/// Equal-length arrays are compared recursively; different lengths use one exact
/// splice after common prefix/suffix removal. Strings use complete replacements.
/// A changed reserved data key replaces its closest safe ancestor instead of
/// emitting an unsafe path. Operations are not promised minimal or canonical.
///
/// # Errors
/// Rejects either nonportable input before producing operations. Neither borrowed
/// input is changed. Revisions are attached separately with [`DeltaBatch::new`].
pub fn diff(before: &Value, after: &Value) -> Result<Vec<DeltaOp>, DeltaError> {
    portable(before, ValueSource::Before)?;
    portable(after, ValueSource::After)?;
    let mut ops = Vec::new();
    diff_value(before, after, &mut Vec::new(), &mut ops);
    Ok(ops)
}

fn equal(left: &Value, right: &Value) -> bool {
    match (left, right) {
        (Value::Number(left), Value::Number(right)) => left.as_f64() == right.as_f64(),
        (Value::Array(left), Value::Array(right)) => {
            left.len() == right.len()
                && left
                    .iter()
                    .zip(right)
                    .all(|(left, right)| equal(left, right))
        }
        (Value::Object(left), Value::Object(right)) => {
            left.len() == right.len()
                && left
                    .iter()
                    .all(|(key, left)| right.get(key).is_some_and(|right| equal(left, right)))
        }
        _ => left == right,
    }
}

fn emit_set(path: &[PathSegment], value: &Value, ops: &mut Vec<DeltaOp>) {
    ops.push(DeltaOp::Set {
        path: path.to_vec(),
        value: value.clone(),
    });
}

fn diff_value(before: &Value, after: &Value, path: &mut Vec<PathSegment>, ops: &mut Vec<DeltaOp>) {
    if equal(before, after) {
        return;
    }
    match (before, after) {
        (Value::Object(before), Value::Object(after)) => {
            if RESERVED_KEYS
                .iter()
                .any(|key| match (before.get(*key), after.get(*key)) {
                    (Some(before), Some(after)) => !equal(before, after),
                    (None, None) => false,
                    _ => true,
                })
            {
                emit_set(path, &Value::Object(after.clone()), ops);
                return;
            }
            let keys: BTreeSet<_> = before.keys().chain(after.keys()).collect();
            for key in keys {
                path.push(PathSegment::Key(key.clone()));
                match (before.get(key), after.get(key)) {
                    (Some(before), Some(after)) => diff_value(before, after, path, ops),
                    (_, Some(after)) => emit_set(path, after, ops),
                    (Some(_), None) => ops.push(DeltaOp::Remove { path: path.clone() }),
                    (None, None) => {}
                }
                path.pop();
            }
        }
        (Value::Array(before), Value::Array(after)) if before.len() == after.len() => {
            for (index, (before, after)) in before.iter().zip(after).enumerate() {
                path.push(PathSegment::Index(index));
                diff_value(before, after, path, ops);
                path.pop();
            }
        }
        (Value::Array(before), Value::Array(after)) => {
            let prefix = before
                .iter()
                .zip(after)
                .take_while(|(a, b)| equal(a, b))
                .count();
            let suffix = before[prefix..]
                .iter()
                .rev()
                .zip(after[prefix..].iter().rev())
                .take_while(|(a, b)| equal(a, b))
                .count();
            ops.push(DeltaOp::Splice {
                path: path.clone(),
                index: prefix,
                delete_count: before.len() - prefix - suffix,
                items: after[prefix..after.len() - suffix].to_vec(),
            });
        }
        _ => emit_set(path, after, ops),
    }
}
