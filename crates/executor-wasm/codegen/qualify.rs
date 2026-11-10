//! Refuse WIT shapes whose resource ownership the generated dispatcher cannot deliver or roll
//! back. Later phases extend these rules deliberately together with the ownership mechanism.
use wit_parser::{Function, FunctionKind, Handle, Resolve, Type, TypeDefKind};

/// Follow aliases to the defining type.
fn peel(resolve: &Resolve, ty: Type) -> Type {
    match ty {
        Type::Id(id) => match &resolve.types[id].kind {
            TypeDefKind::Type(inner) => peel(resolve, *inner),
            _ => ty,
        },
        other => other,
    }
}

/// Whether a value of `ty` can carry a resource, stream or future handle.
pub(super) fn carries_handle(resolve: &Resolve, ty: Type) -> bool {
    let Type::Id(id) = peel(resolve, ty) else {
        return false;
    };
    let any = |types: &mut dyn Iterator<Item = Type>| {
        for ty in types {
            if carries_handle(resolve, ty) {
                return true;
            }
        }
        false
    };
    match &resolve.types[id].kind {
        TypeDefKind::Resource
        | TypeDefKind::Handle(_)
        | TypeDefKind::Future(_)
        | TypeDefKind::Stream(_)
        | TypeDefKind::Unknown => true,
        TypeDefKind::Record(record) => any(&mut record.fields.iter().map(|field| field.ty)),
        TypeDefKind::Tuple(tuple) => any(&mut tuple.types.iter().copied()),
        TypeDefKind::Variant(variant) => any(&mut variant.cases.iter().filter_map(|case| case.ty)),
        TypeDefKind::Result(result) => any(&mut result.ok.into_iter().chain(result.err)),
        TypeDefKind::Option(inner)
        | TypeDefKind::List(inner)
        | TypeDefKind::FixedLengthList(inner, _) => carries_handle(resolve, *inner),
        TypeDefKind::Map(key, value) => any(&mut [*key, *value].into_iter()),
        TypeDefKind::Flags(_) | TypeDefKind::Enum(_) => false,
        TypeDefKind::Type(_) => unreachable!("peeled"),
    }
}

/// Whether `ty` contains a future or stream whose payload carries a handle. Such handles would
/// be issued while no reply owns them, so nothing could publish or roll them back.
fn unowned_payload(resolve: &Resolve, ty: Type) -> bool {
    let Type::Id(id) = peel(resolve, ty) else {
        return false;
    };
    let any = |types: &mut dyn Iterator<Item = Type>| {
        for ty in types {
            if unowned_payload(resolve, ty) {
                return true;
            }
        }
        false
    };
    match &resolve.types[id].kind {
        TypeDefKind::Future(Some(payload)) | TypeDefKind::Stream(Some(payload)) => {
            carries_handle(resolve, *payload)
        }
        TypeDefKind::Record(record) => any(&mut record.fields.iter().map(|field| field.ty)),
        TypeDefKind::Tuple(tuple) => any(&mut tuple.types.iter().copied()),
        TypeDefKind::Variant(variant) => any(&mut variant.cases.iter().filter_map(|case| case.ty)),
        TypeDefKind::Result(result) => any(&mut result.ok.into_iter().chain(result.err)),
        TypeDefKind::Option(inner)
        | TypeDefKind::List(inner)
        | TypeDefKind::FixedLengthList(inner, _) => unowned_payload(resolve, *inner),
        TypeDefKind::Map(key, value) => any(&mut [*key, *value].into_iter()),
        _ => false,
    }
}

/// Whether a parameter is a direct `stream<u8>`, after aliases.
fn byte_stream(resolve: &Resolve, ty: Type) -> bool {
    matches!(peel(resolve, ty), Type::Id(id)
        if matches!(resolve.types[id].kind, TypeDefKind::Stream(Some(Type::U8))))
}

/// The owned/borrowed handle a parameter passes directly, after aliases.
fn direct_handle(resolve: &Resolve, ty: Type) -> Option<Handle> {
    let Type::Id(id) = peel(resolve, ty) else {
        return None;
    };
    match &resolve.types[id].kind {
        TypeDefKind::Handle(handle) => Some(*handle),
        TypeDefKind::Resource => Some(Handle::Own(id)),
        _ => None,
    }
}

/// Whether `ty` is a future the guest may write: `future<result<_, E>>` whose error variant has
/// an `internal-error(option<string>)` case, the value an abandoned writer resolves to.
pub(super) fn writable_future(resolve: &Resolve, ty: Type) -> bool {
    let Type::Id(id) = peel(resolve, ty) else {
        return false;
    };
    let TypeDefKind::Future(Some(payload)) = &resolve.types[id].kind else {
        return false;
    };
    let Type::Id(payload) = peel(resolve, *payload) else {
        return false;
    };
    let TypeDefKind::Result(result) = &resolve.types[payload].kind else {
        return false;
    };
    let Some(Type::Id(error)) = result.err.map(|ty| peel(resolve, ty)) else {
        return false;
    };
    matches!(&resolve.types[error].kind, TypeDefKind::Variant(variant)
        if variant.cases.iter().any(|case| case.name == "internal-error"
            && case.ty.is_some_and(|ty| matches!(peel(resolve, ty), Type::Id(inner)
                if matches!(resolve.types[inner].kind, TypeDefKind::Option(Type::String))))))
}

/// A parameter whose ownership transfers after the packet validated: a direct owned resource,
/// a `stream<u8>`, a writable future, or `option<..>` of one of these.
fn transferable(resolve: &Resolve, ty: Type, optional: bool) -> bool {
    if byte_stream(resolve, ty) || writable_future(resolve, ty) {
        return true;
    }
    if let Some(Handle::Own(_)) = direct_handle(resolve, ty) {
        return true;
    }
    if optional
        && let Type::Id(id) = peel(resolve, ty)
        && let TypeDefKind::Option(inner) = resolve.types[id].kind
    {
        return transferable(resolve, inner, false);
    }
    false
}

/// Whether every handle a future/stream payload carries is an owned resource (journaled with
/// the FUTURE_READ or STREAM_READ that publishes it); streams, futures and borrows inside
/// payloads have no owner.
fn owned_only(resolve: &Resolve, ty: Type) -> bool {
    let Type::Id(id) = peel(resolve, ty) else {
        return true;
    };
    match &resolve.types[id].kind {
        TypeDefKind::Handle(Handle::Own(_)) | TypeDefKind::Resource => true,
        TypeDefKind::Handle(Handle::Borrow(_))
        | TypeDefKind::Future(_)
        | TypeDefKind::Stream(_)
        | TypeDefKind::Unknown => false,
        TypeDefKind::Record(record) => record.fields.iter().all(|f| owned_only(resolve, f.ty)),
        TypeDefKind::Tuple(tuple) => tuple.types.iter().all(|ty| owned_only(resolve, *ty)),
        TypeDefKind::Variant(variant) => variant
            .cases
            .iter()
            .filter_map(|case| case.ty)
            .all(|ty| owned_only(resolve, ty)),
        TypeDefKind::Result(result) => result
            .ok
            .into_iter()
            .chain(result.err)
            .all(|ty| owned_only(resolve, ty)),
        TypeDefKind::Option(inner)
        | TypeDefKind::List(inner)
        | TypeDefKind::FixedLengthList(inner, _) => owned_only(resolve, *inner),
        TypeDefKind::Map(key, value) => owned_only(resolve, *key) && owned_only(resolve, *value),
        TypeDefKind::Flags(_) | TypeDefKind::Enum(_) | TypeDefKind::Type(_) => true,
    }
}

/// Whether a result exposes a future/stream payload whose handles no read could publish.
fn unpublishable(resolve: &Resolve, ty: Type) -> bool {
    let Type::Id(id) = peel(resolve, ty) else {
        return false;
    };
    match &resolve.types[id].kind {
        TypeDefKind::Future(Some(payload)) | TypeDefKind::Stream(Some(payload)) => {
            !owned_only(resolve, *payload)
        }
        TypeDefKind::Tuple(tuple) => tuple.types.iter().any(|ty| unpublishable(resolve, *ty)),
        TypeDefKind::Result(result) => result
            .ok
            .into_iter()
            .chain(result.err)
            .any(|ty| unpublishable(resolve, ty)),
        TypeDefKind::Option(inner) => unpublishable(resolve, *inner),
        _ => unowned_payload(resolve, Type::Id(id)),
    }
}

/// Accept only resource flows with a complete owner:
/// - direct borrows (async: lent until the task settles or aborts);
/// - synchronous transferable parameters (direct owned, `stream<u8>`, writable future, or an
///   `option` of one), transferred only after the whole packet validated;
/// - async direct owned parameters, transferred on the task's first poll (an aborted task
///   leaves them to retire, which drops them through upstream);
/// - results whose handles are journaled with their reply, including owned handles inside
///   future payloads and stream items (journaled with the FUTURE_READ or STREAM_READ reply that
///   delivers them).
pub(super) fn qualify(resolve: &Resolve, function: &Function) -> Result<(), String> {
    let asynchronous = matches!(
        function.kind,
        FunctionKind::AsyncFreestanding | FunctionKind::AsyncMethod(_)
    );
    for param in &function.params {
        if let Some(Handle::Borrow(_)) = direct_handle(resolve, param.ty) {
            continue;
        }
        let direct_own = matches!(direct_handle(resolve, param.ty), Some(Handle::Own(_)));
        if asynchronous && !direct_own && transferable(resolve, param.ty, true) {
            return Err(format!(
                "stream or future parameter `{}` of an async func has no transfer owner if its \
                 task is aborted",
                param.name
            ));
        }
        if transferable(resolve, param.ty, true) {
            continue;
        }
        if carries_handle(resolve, param.ty) {
            return Err(format!(
                "parameter `{}` nests a resource, stream or future handle; only direct or \
                 optional transferable handles are lent or transferred",
                param.name
            ));
        }
    }
    if let Some(result) = function.result
        && unpublishable(resolve, result)
    {
        return Err(
            "result has a future or stream payload carrying handles, which no reply publishes"
                .into(),
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const WIT: &str = r#"package test:qualify;
interface t {
  resource r;
  type own-alias = r;
  type borrow-alias = borrow<r>;
  type deep-alias = borrow-alias;
  record holder { item: borrow<r> }
  type holder-alias = holder;
  direct: async func(x: borrow<r>) -> u32;
  aliased: async func(x: deep-alias) -> u32;
  opens: async func(x: borrow<r>) -> result<r, u32>;
  sync-owned: func(x: r);
  sync-alias-owned: func(x: own-alias);
  owned: async func(x: r);
  owned-alias: async func(x: own-alias);
  nested: async func(x: holder-alias);
  optional: async func(x: option<borrow<r>>);
  listed: func(x: list<r>);
  bytes: func() -> tuple<stream<u8>, future<result<_, u32>>>;
  handle-future: func() -> tuple<stream<u8>, future<result<own-alias, u32>>>;
  handle-stream: func() -> stream<r>;
  listen: func() -> result<stream<r>, u32>;
  future-stream: func() -> stream<future<u32>>;
  stream-stream: func() -> stream<stream<u8>>;
  sync-bytes: func(data: stream<u8>, offset: u64) -> future<result<_, u32>>;
  async-bytes: async func(data: stream<u8>);
  handle-input: func(data: stream<r>);
  record holder-entry { name: string, size: u64 }
  entries: func() -> tuple<stream<holder-entry>, future<result<_, u32>>>;
}"#;

    fn check(name: &str) -> Result<(), String> {
        let mut resolve = Resolve::default();
        resolve.push_str("qualify.wit", WIT).unwrap();
        let (_, interface) = resolve
            .interfaces
            .iter()
            .find(|(_, interface)| interface.name.as_deref() == Some("t"))
            .unwrap();
        qualify(&resolve, &interface.functions[name])
    }

    #[test]
    fn direct_borrows_and_owned_results_are_qualified() {
        for name in [
            "direct",
            "aliased",
            "opens",
            "sync-owned",
            "sync-alias-owned",
            "bytes",
            "sync-bytes",
            "entries",
            "owned",
            "owned-alias",
            "handle-future",
            "handle-stream",
            "listen",
        ] {
            assert_eq!(check(name), Ok(()), "{name}");
        }
    }

    #[test]
    fn async_owned_inputs_and_nested_handles_are_refused() {
        assert!(
            check("async-bytes")
                .unwrap_err()
                .contains("stream or future parameter `data`")
        );
        for name in ["nested", "optional", "listed", "handle-input"] {
            assert!(
                check(name).unwrap_err().contains("nests a resource"),
                "{name}"
            );
        }
    }

    #[test]
    fn unowned_stream_payloads_are_refused() {
        // Owned handles in a stream item are published with the STREAM_READ reply; streams and
        // futures nested inside items have no owner.
        for name in ["future-stream", "stream-stream"] {
            assert!(
                check(name)
                    .unwrap_err()
                    .contains("payload carrying handles"),
                "{name}"
            );
        }
    }
}
