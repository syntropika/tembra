//! Emit the shared compact WIT type and method metadata for TypeScript.
use super::*;
use std::fmt::Write as _;

impl<'a> Generator<'a> {
    /// Register one TypeScript type descriptor and return its index.
    pub(super) fn ts(&mut self, ty: Type) -> usize {
        let descriptor = match self.peel(ty) {
            Type::Bool => "[\"bool\"]".into(),
            Type::U8 => "[\"u8\"]".into(),
            Type::U16 => "[\"u16\"]".into(),
            Type::U32 => "[\"u32\"]".into(),
            Type::U64 => "[\"u64\"]".into(),
            Type::S8 => "[\"s8\"]".into(),
            Type::S16 => "[\"s16\"]".into(),
            Type::S32 => "[\"s32\"]".into(),
            Type::S64 => "[\"s64\"]".into(),
            Type::F32 => "[\"f32\"]".into(),
            Type::F64 => "[\"f64\"]".into(),
            Type::Char => "[\"char\"]".into(),
            Type::String => "[\"string\"]".into(),
            Type::ErrorContext => panic!("error-context is not supported"),
            Type::Id(id) => match self.resolve.types[id].kind.clone() {
                TypeDefKind::List(Type::U8) => "[\"bytes\"]".into(),
                TypeDefKind::List(element) => format!("[\"list\", {}]", self.ts(element)),
                TypeDefKind::Tuple(tuple) => {
                    let items: Vec<String> = tuple
                        .types
                        .iter()
                        .map(|ty| self.ts(*ty).to_string())
                        .collect();
                    format!("[\"tuple\", [{}]]", items.join(", "))
                }
                TypeDefKind::Record(record) => {
                    let fields: Vec<String> = record
                        .fields
                        .iter()
                        .map(|field| {
                            format!("[\"{}\", {}]", lower_camel(&field.name), self.ts(field.ty))
                        })
                        .collect();
                    format!("[\"record\", [{}]]", fields.join(", "))
                }
                TypeDefKind::Flags(flags) => {
                    assert!(flags.flags.len() <= 32, "flags wider than 32 bits");
                    let names: Vec<String> = flags
                        .flags
                        .iter()
                        .map(|flag| format!("\"{}\"", lower_camel(&flag.name)))
                        .collect();
                    format!("[\"flags\", [{}]]", names.join(", "))
                }
                TypeDefKind::Enum(cases) => {
                    let names: Vec<String> = cases
                        .cases
                        .iter()
                        .map(|case| format!("\"{}\"", case.name))
                        .collect();
                    format!("[\"enum\", [{}]]", names.join(", "))
                }
                TypeDefKind::Variant(variant) => {
                    let cases: Vec<String> = variant
                        .cases
                        .iter()
                        .map(|case| {
                            let payload = case.ty.map_or(-1, |ty| self.ts(ty) as i64);
                            format!("[\"{}\", {payload}]", case.name)
                        })
                        .collect();
                    format!("[\"variant\", [{}]]", cases.join(", "))
                }
                TypeDefKind::Option(inner) => format!("[\"option\", {}]", self.ts(inner)),
                TypeDefKind::Result(result) => {
                    let ok = result.ok.map_or(-1, |ty| self.ts(ty) as i64);
                    let err = result.err.map_or(-1, |ty| self.ts(ty) as i64);
                    format!("[\"result\", {ok}, {err}]")
                }
                TypeDefKind::Handle(Handle::Own(resource)) => {
                    format!("[\"own\", {}]", self.resource(resource))
                }
                TypeDefKind::Handle(Handle::Borrow(resource)) => {
                    format!("[\"borrow\", {}]", self.resource(resource))
                }
                TypeDefKind::Resource => format!("[\"own\", {}]", self.resource(id)),
                TypeDefKind::Stream(Some(Type::U8)) => "[\"stream\", \"u8\"]".into(),
                TypeDefKind::Stream(Some(item)) => format!("[\"stream\", {}]", self.ts(item)),
                TypeDefKind::Future(payload) => {
                    format!(
                        "[\"future\", {}]",
                        payload.map_or(-1, |ty| self.ts(ty) as i64)
                    )
                }
                other => panic!("WIT type {other:?} has no Phase 1 binding rule"),
            },
        };
        if let Some(index) = self.type_index.get(&descriptor) {
            return *index;
        }
        self.types.push(descriptor.clone());
        self.type_index.insert(descriptor, self.types.len() - 1);
        self.types.len() - 1
    }

    /// Render the TypeScript type and method table shared by the guest and browser hosts.
    pub(super) fn typescript(&mut self, methods: &[Method<'_>], writable: &[usize]) -> String {
        let mut out = String::from(
            "// Generated by crates/executor-wasm/build.rs from the bundled WASI WIT. Do not edit.\n\
             // Regenerate with: TEMBRA_BLESS=1 cargo test -p tembra-executor-wasm --lib standard\n\n\
             /** One WIT type: a kind followed by indices into `TYPES` or names. */\n\
             export type WasiType = readonly [string, ...unknown[]];\n\n\
             /** One generated method; its index is the binary method number. */\n\
             export interface WasiMethod {\n  readonly interface: string;\n  readonly name: string;\n  readonly resource?: number;\n  /** Set for `[static]` functions and constructors, which have no `self` parameter. */\n  readonly kind?: \"static\" | \"constructor\";\n  readonly asynchronous: boolean;\n  readonly params: readonly number[];\n  readonly result?: number;\n}\n\n",
        );
        out += "export const TYPES: readonly WasiType[] = [\n";
        for ty in &self.types {
            let _ = writeln!(out, "  {ty},");
        }
        out += "];\n\nexport const RESOURCES: readonly string[] = [\n";
        for (name, _) in &self.resources {
            let _ = writeln!(out, "  {name:?},");
        }
        out += "];\n\nexport const METHODS: readonly WasiMethod[] = [\n";
        for method in methods {
            let resource = method.resource.as_ref().map(|name| {
                let qualified = format!("{}#{name}", method.interface);
                self.resources
                    .iter()
                    .position(|(known, _)| *known == qualified)
                    .expect("method resource")
            });
            let _ = write!(
                out,
                "  {{ interface: {:?}, name: {:?}, ",
                method.interface,
                lower_camel(method.function.item_name())
            );
            if let Some(resource) = resource {
                let _ = write!(out, "resource: {resource}, ");
            }
            if let Some(kind) = method.kind {
                let _ = write!(out, "kind: {kind:?}, ");
            }
            let params: Vec<String> = method.params.iter().map(usize::to_string).collect();
            let _ = write!(
                out,
                "asynchronous: {}, params: [{}]",
                method.asynchronous,
                params.join(", ")
            );
            if let Some(result) = method.result {
                let _ = write!(out, ", result: {result}");
            }
            out += " },\n";
        }
        out += "];\n\n/** `TYPES` indices of the futures a guest may create (FUTURE_NEW) and write once. */\n";
        let writable: Vec<String> = writable.iter().map(usize::to_string).collect();
        let _ = writeln!(
            out,
            "export const WRITABLE_FUTURES: readonly number[] = [{}];",
            writable.join(", ")
        );
        out
    }
}
