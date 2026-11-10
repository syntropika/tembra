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

/// Accept only resource flows with a complete owner: direct borrows (lent to an async task until
/// it settles or aborts), direct owned inputs and `stream<u8>` inputs of synchronous calls
/// (transferred only after the whole packet validated, before return), and results journaled
/// with their reply.
pub(super) fn qualify(resolve: &Resolve, function: &Function) -> Result<(), String> {
    let asynchronous = matches!(
        function.kind,
        FunctionKind::AsyncFreestanding | FunctionKind::AsyncMethod(_)
    );
    for param in &function.params {
        if byte_stream(resolve, param.ty) {
            if asynchronous {
                return Err(format!(
                    "stream parameter `{}` of an async func has no transfer owner if its task is \
                     aborted",
                    param.name
                ));
            }
            continue;
        }
        match direct_handle(resolve, param.ty) {
            Some(Handle::Borrow(_)) => {}
            Some(Handle::Own(_)) if asynchronous => {
                return Err(format!(
                    "owned parameter `{}` of an async func would be dropped without upstream \
                     cleanup if its task is aborted",
                    param.name
                ));
            }
            Some(Handle::Own(_)) => {}
            None if carries_handle(resolve, param.ty) => {
                return Err(format!(
                    "parameter `{}` nests a resource, stream or future handle; only direct \
                     handle parameters are lent or transferred",
                    param.name
                ));
            }
            None => {}
        }
    }
    if let Some(result) = function.result
        && unowned_payload(resolve, result)
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
        ] {
            assert_eq!(check(name), Ok(()), "{name}");
        }
    }

    #[test]
    fn async_owned_inputs_and_nested_handles_are_refused() {
        for name in ["owned", "owned-alias"] {
            assert!(
                check(name).unwrap_err().contains("owned parameter `x`"),
                "{name}"
            );
        }
        assert!(
            check("async-bytes")
                .unwrap_err()
                .contains("stream parameter `data`")
        );
        for name in ["nested", "optional", "listed", "handle-input"] {
            assert!(
                check(name).unwrap_err().contains("nests a resource"),
                "{name}"
            );
        }
    }

    #[test]
    fn handle_bearing_future_and_stream_payloads_are_refused() {
        for name in ["handle-future", "handle-stream"] {
            assert!(
                check(name)
                    .unwrap_err()
                    .contains("payload carrying handles"),
                "{name}"
            );
        }
    }
}
