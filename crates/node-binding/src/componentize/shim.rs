//! JS shim that installs the runtime's `globalThis.wit` stream and future API for one world.
//!
//! Ported from componentize-qjs v0.6.0 (commit 758687536ea711ede17369f53a90ddc3e97a8dbb),
//! `crates/core/src/codegen.rs`, licensed under the Apache License, Version 2.0. Tembra
//! changes: the stream helper is Tembra's failure-preserving copy (`js/stream-from.js`) and the
//! shim first requires the runtime's native failure primitive. Futures keep upstream's generic
//! support: WASI 0.3 imports use them even where public Orb profiles refuse them.

use std::collections::{HashMap, HashSet};

use wit_dylib::metadata;
use wit_parser::{Resolve, Type, TypeDefKind, TypeId, TypeOwner};

/// Patched upstream helper; the only stream completion path a source Orb can reach.
const STREAM_FROM: &str = include_str!("js/stream-from.js");
/// Upstream helper, unchanged.
const FUTURE_FROM: &str = include_str!("js/future-from.js");

/// Generate the shim evaluated before the user module during the build-time snapshot.
pub(super) fn generate(resolve: &Resolve, metadata: &wit_dylib::Metadata) -> String {
    let mut lines = vec![
        // An unpatched runtime would turn a failed producer into a clean end of stream.
        "if (typeof globalThis.__cqjs?.failStream !== 'function') \
         throw new Error('the QuickJS runtime lacks the stream failure primitive');"
            .to_owned(),
        "const wit = globalThis.wit = {};".to_owned(),
    ];
    let (stream_aliases, future_aliases) = async_aliases(resolve, metadata);
    let payload = |id: TypeId| match &resolve.types[id].kind {
        TypeDefKind::Stream(ty) | TypeDefKind::Future(ty) => *ty,
        _ => unreachable!("wit-dylib async metadata names a non-async type"),
    };
    let streams: Vec<_> = metadata.streams.iter().map(|s| payload(s.id)).collect();
    let futures: Vec<_> = metadata.futures.iter().map(|f| payload(f.id)).collect();
    if !streams.is_empty() {
        let constructor = Constructor {
            name: "Stream",
            native: "__cqjs.makeStream",
            helper: STREAM_FROM,
        };
        constructor.emit(&mut lines, resolve, &streams, &stream_aliases);
    }
    if !futures.is_empty() {
        let constructor = Constructor {
            name: "Future",
            native: "__cqjs.makeFuture",
            helper: FUTURE_FROM,
        };
        constructor.emit(&mut lines, resolve, &futures, &future_aliases);
    }
    lines.join("\n") + "\n"
}

struct Constructor {
    name: &'static str,
    native: &'static str,
    helper: &'static str,
}

impl Constructor {
    fn emit(
        &self,
        lines: &mut Vec<String>,
        resolve: &Resolve,
        types: &[Option<Type>],
        aliases: &[Vec<TypeId>],
    ) {
        let Self {
            name,
            native,
            helper,
        } = self;
        if types.len() == 1 {
            lines.push(format!(
                "wit.{name} = function(type) {{ return {native}(type ?? 0); }};"
            ));
        } else {
            lines.push(format!(
                "wit.{name} = function(type) {{ if (type === undefined) throw new Error('{name} \
                 type required, use wit.{name}.<TYPE>'); return {native}(type); }};"
            ));
        }
        lines.push(format!("wit.{name}.types = {{}};"));
        let emit = |lines: &mut Vec<String>, constant: &str, index: usize| {
            lines.push(format!(
                "wit.{name}.{constant} = {index}; wit.{name}.types.{constant} = {index};"
            ));
        };
        let mut emitted = HashMap::<String, usize>::new();
        let mut used = HashSet::new();
        for (index, constant) in unique_const_names(resolve, types).into_iter().enumerate() {
            emit(lines, &constant, index);
            used.insert(constant.clone());
            emitted.insert(constant, index);
        }
        for (index, aliases) in aliases.iter().enumerate() {
            for alias in aliases {
                let local = typedef_const_name(resolve, *alias, Style::Local);
                let candidate = match emitted.get(&local) {
                    Some(existing) if *existing == index => continue,
                    Some(_) => typedef_const_name(resolve, *alias, Style::Qualified),
                    None => local,
                };
                if emitted.get(&candidate) == Some(&index) {
                    continue;
                }
                let constant = unique_name(candidate, &mut used);
                emit(lines, &constant, index);
                emitted.insert(constant, index);
            }
        }
        lines.extend(helper.lines().map(str::to_owned));
    }
}

/// Named stream and future types and the aliases resolving to them, per metadata index.
fn async_aliases(
    resolve: &Resolve,
    metadata: &wit_dylib::Metadata,
) -> (Vec<Vec<TypeId>>, Vec<Vec<TypeId>>) {
    let mut streams = vec![Vec::new(); metadata.streams.len()];
    let mut futures = vec![Vec::new(); metadata.futures.len()];
    for (index, stream) in metadata.streams.iter().enumerate() {
        if resolve.types[stream.id].name.is_some() {
            streams[index].push(stream.id);
        }
    }
    for (index, future) in metadata.futures.iter().enumerate() {
        if resolve.types[future.id].name.is_some() {
            futures[index].push(future.id);
        }
    }
    for alias in &metadata.aliases {
        let mut ty = alias.ty;
        let mut visited = HashSet::new();
        loop {
            match ty {
                metadata::Type::Stream(index) => break streams[index].push(alias.id),
                metadata::Type::Future(index) => break futures[index].push(alias.id),
                metadata::Type::Alias(next) if visited.insert(next) => {
                    ty = metadata.aliases[next].ty;
                }
                _ => break,
            }
        }
    }
    (streams, futures)
}

#[derive(Clone, Copy)]
enum Style {
    Local,
    Qualified,
}

fn type_const_name(resolve: &Resolve, ty: Option<&Type>, style: Style) -> String {
    match ty {
        None => "UNIT".into(),
        Some(Type::Bool) => "BOOL".into(),
        Some(Type::U8) => "U8".into(),
        Some(Type::S8) => "S8".into(),
        Some(Type::U16) => "U16".into(),
        Some(Type::S16) => "S16".into(),
        Some(Type::U32) => "U32".into(),
        Some(Type::S32) => "S32".into(),
        Some(Type::U64) => "U64".into(),
        Some(Type::S64) => "S64".into(),
        Some(Type::F32) => "F32".into(),
        Some(Type::F64) => "F64".into(),
        Some(Type::Char) => "CHAR".into(),
        Some(Type::String) => "STRING".into(),
        Some(Type::ErrorContext) => "ERROR_CONTEXT".into(),
        Some(Type::Id(id)) => typedef_const_name(resolve, *id, style),
    }
}

fn typedef_const_name(resolve: &Resolve, id: TypeId, style: Style) -> String {
    let typedef = &resolve.types[id];
    if let Some(name) = &typedef.name {
        let prefix = match (style, typedef.owner) {
            (Style::Local, _) | (_, TypeOwner::None) => None,
            (Style::Qualified, TypeOwner::Interface(interface)) => resolve.id_of(interface),
            (Style::Qualified, TypeOwner::World(world)) => Some(resolve.worlds[world].name.clone()),
        };
        return match prefix {
            Some(prefix) => const_ident(&format!("{prefix}-{name}")),
            None => const_ident(name),
        };
    }
    let name = |ty: &Type| type_const_name(resolve, Some(ty), style);
    match &typedef.kind {
        TypeDefKind::Option(inner) => format!("OPTION_{}", name(inner)),
        TypeDefKind::Tuple(tuple) => {
            let inner: Vec<_> = tuple.types.iter().map(name).collect();
            format!("TUPLE_{}", inner.join("_"))
        }
        TypeDefKind::Result(result) => {
            let ok = result.ok.as_ref().map_or_else(|| "VOID".into(), name);
            let err = result.err.as_ref().map_or_else(|| "VOID".into(), name);
            format!("RESULT_{ok}_{err}")
        }
        TypeDefKind::List(inner) => format!("LIST_{}", name(inner)),
        TypeDefKind::Map(key, value) => format!("MAP_{}_{}", name(key), name(value)),
        TypeDefKind::Stream(inner) => format!(
            "STREAM_{}",
            inner.as_ref().map_or_else(|| "UNIT".into(), name)
        ),
        TypeDefKind::Future(inner) => format!(
            "FUTURE_{}",
            inner.as_ref().map_or_else(|| "UNIT".into(), name)
        ),
        TypeDefKind::Type(inner) => name(inner),
        _ => "OTHER".into(),
    }
}

fn unique_const_names(resolve: &Resolve, types: &[Option<Type>]) -> Vec<String> {
    let base: Vec<_> = types
        .iter()
        .map(|ty| type_const_name(resolve, ty.as_ref(), Style::Local))
        .collect();
    let mut counts = HashMap::<&str, usize>::new();
    for name in &base {
        *counts.entry(name).or_default() += 1;
    }
    let mut used = HashSet::new();
    base.iter()
        .zip(types)
        .map(|(name, ty)| {
            let candidate = if counts[name.as_str()] > 1 {
                type_const_name(resolve, ty.as_ref(), Style::Qualified)
            } else {
                name.clone()
            };
            unique_name(candidate, &mut used)
        })
        .collect()
}

fn unique_name(candidate: String, used: &mut HashSet<String>) -> String {
    if used.insert(candidate.clone()) {
        return candidate;
    }
    (2..)
        .map(|suffix| format!("{candidate}_{suffix}"))
        .find(|name| used.insert(name.clone()))
        .expect("an unused suffix exists")
}

fn const_ident(text: &str) -> String {
    let mut out: String = text
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_uppercase()
            } else {
                '_'
            }
        })
        .collect();
    if out.starts_with(|c: char| c.is_ascii_digit()) {
        out.insert(0, '_');
    }
    out
}
