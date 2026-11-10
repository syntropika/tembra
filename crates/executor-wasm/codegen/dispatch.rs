//! Generate direct upstream Host calls while preserving WIT calling conventions.
use super::*;
use std::fmt::Write as _;

impl<'a> Generator<'a> {
    /// Find a named interface of the bundled WIT.
    fn interface(&self, name: &str) -> InterfaceId {
        self.resolve
            .interfaces
            .iter()
            .find(|(id, _)| self.interface_name(*id) == name)
            .map(|(id, _)| id)
            .unwrap_or_else(|| panic!("bundled WIT has no interface {name}"))
    }

    /// Every bound function: the explicit set, then complete interfaces in declaration order.
    fn functions(&self) -> Vec<(String, &'a Function, InterfaceId)> {
        let resolve: &'a Resolve = self.resolve;
        let mut functions = Vec::new();
        for interface in COMPLETE {
            let id = self.interface(interface);
            for function in resolve.interfaces[id].functions.values() {
                functions.push((interface.to_string(), function, id));
            }
        }
        functions
    }

    /// Generate the Rust dispatcher and collect the TypeScript method table.
    pub(super) fn generate(&mut self) -> (String, Vec<Method<'a>>, Vec<usize>) {
        let mut methods = Vec::new();
        let mut arms = String::new();
        let mut names_list = Vec::new();
        for (index, (interface, function, interface_id)) in self.functions().into_iter().enumerate()
        {
            let name = function.name.as_str();
            if let Err(error) = qualify::qualify(self.resolve, function) {
                panic!("{interface}#{name} is not qualified for generated dispatch: {error}");
            }
            let module = self.module(interface_id);
            let package = &self.resolve.packages[self.resolve.interfaces[interface_id]
                .package
                .expect("package")];
            let (_, data, getter, blocking) = VIEWS
                .iter()
                .find(|(package_name, _, _, _)| *package_name == package.name.name)
                .expect("host view for package");
            let asynchronous = matches!(
                function.kind,
                FunctionKind::AsyncFreestanding
                    | FunctionKind::AsyncMethod(_)
                    | FunctionKind::AsyncStatic(_)
            );
            // Async functions are always store functions; this marks WIT-synchronous ones.
            let store = STORE.contains(&format!("{interface}#{name}").as_str()) && !asynchronous;
            let resource = match function.kind {
                FunctionKind::Method(id)
                | FunctionKind::AsyncMethod(id)
                | FunctionKind::Static(id)
                | FunctionKind::AsyncStatic(id)
                | FunctionKind::Constructor(id) => {
                    Some(self.resolve.types[id].name.clone().expect("resource name"))
                }
                FunctionKind::Freestanding | FunctionKind::AsyncFreestanding => None,
                _ => panic!("{interface}#{name}: no binding rule for this function kind"),
            };
            let trait_name = match (&resource, asynchronous || store) {
                (None, false) => format!("{module}::Host"),
                (None, true) => format!("<{data} as {module}::HostWithStore<State>>"),
                (Some(resource), false) => format!("{module}::Host{}", camel(resource)),
                (Some(resource), true) => {
                    format!(
                        "<{data} as {module}::Host{}WithStore<State>>",
                        camel(resource)
                    )
                }
            };
            // Upstream bindgen names a constructor `new`.
            let method = match function.kind {
                FunctionKind::Constructor(_) => "new".to_owned(),
                _ => snake(&function.item_name().replace('-', "_")),
            };
            // Every owned handle, input stream and writable future is claimed (validated,
            // deduplicated) during decode and committed only after the whole packet validated
            // and every refusal (budget, admission, table capacity) passed.
            let mut decode = "let mut seen = Vec::new();".to_owned();
            let mut names = Vec::new();
            let mut lends = Vec::new();
            let mut commits = "h.commit(seen);".to_owned();
            let mut owned = Vec::new();
            let mut transfers = Vec::new();
            for param in &function.params {
                let local = format!("p_{}", snake(&param.name));
                // A borrow captured by a Store task is lent until the task settles or aborts,
                // so the owner cannot drop (and upstream cannot reuse) the resource meanwhile.
                match self.borrowed(param.ty).filter(|_| asynchronous) {
                    Some(kind) => {
                        let lend = format!("l_{}", snake(&param.name));
                        let _ =
                            write!(decode, "let ({local}, {lend}) = h.lend({kind}, r.u64()?)?;");
                        lends.push(lend);
                    }
                    None => {
                        let _ = write!(decode, "let {local} = {};", self.decode(param.ty));
                    }
                }
                match self.owned_param(param.ty).filter(|_| asynchronous) {
                    // An async task takes its owned argument on its first poll; aborted before
                    // that, the guard closes it through the close queue.
                    Some(kind) => {
                        let _ = write!(
                            commits,
                            "let {local} = Owned::new({kind}, {local}, &h.closing);"
                        );
                        owned.push(local.clone());
                    }
                    None => {
                        if let Some(transfer) = self.transfer(param.ty, &local) {
                            transfers.push(transfer);
                        }
                    }
                }
                if let Some((_, _, _, framing)) = CAPPED
                    .iter()
                    .find(|(i, f, p, _)| *i == interface.as_str() && *f == name && *p == param.name)
                {
                    let _ = write!(
                        decode,
                        "let {local} = {local}.min(state.packet_bytes.saturating_sub({framing}) as u64);"
                    );
                }
                names.push(local);
            }
            let leases: Vec<String> = lends.iter().map(|l| format!("{l}.lease()")).collect();
            let leases = format!("vec![{}]", leases.join(", "));
            let takes: String = owned
                .iter()
                .map(|o| format!("let {o} = {o}.take();"))
                .collect();
            let lends = if lends.is_empty() {
                "()".to_owned()
            } else {
                format!("({},)", lends.join(", "))
            };
            let args = names.join(", ");
            let params: Vec<usize> = function.params.iter().map(|p| self.ts(p.ty)).collect();
            let result = function.result.map(|ty| self.ts(ty));
            let arm = if store {
                self.store_arm(
                    function,
                    &trait_name,
                    (data, getter, *blocking),
                    &args,
                    (&commits, &transfers.concat()),
                )
            } else if asynchronous {
                let encode = self.encode_return(function.result);
                let admit = if *blocking {
                    "let permit = admit_file()?;"
                } else {
                    "let permit = ();"
                };
                let owned = function.result.map_or(0, |ty| self.owned(ty));
                let files = format!("let files = h.files({owned})?;");
                assert!(
                    transfers.is_empty(),
                    "{interface}#{name}: async stream or future parameters have no transfer owner"
                );
                format!(
                    "{files} {admit} {commits} let leases = {leases}; Ok(Call::Spawn(Box::new(move |store: StoreContextMut<'_, State>, reply: Reply| \
                     store.spawn(async move |access: &Accessor<State>| -> wasmtime::Result<()> {{ \
                     let _permit = permit; let _lends = {lends}; \
                     let view = access.with_getter::<{data}>({getter}); \
                     {takes} let result = {trait_name}::{method}(&view, {args}).await; \
                     let bytes = access.with(|mut store| -> wasmtime::Result<Vec<u8>> {{ let h = &mut store.data_mut().standard.handles; h.prepaid.extend(files); let mut out = Writer::default(); let w = &mut out; {encode} Ok(out.0) }}); \
                     settle(access, reply, bytes); Ok(()) }})), leases))"
                )
            } else {
                let encode = self.encode_return(function.result);
                assert!(
                    transfers.is_empty(),
                    "{interface}#{name}: stream or future parameters need a store function"
                );
                format!(
                    "{{ {commits} let result = {trait_name}::{method}(&mut {getter}(state), {args}); \
                     let h = &mut state.standard.handles; let mut out = Writer::default(); let w = &mut out; {encode} Ok(Call::Ready(out.0)) }}"
                )
            };
            let _ = write!(
                arms,
                "        // {interface} {name}\n        {index} => {{ let h = &mut state.standard.handles; {decode} r.end()?; {arm} }}\n"
            );
            names_list.push(format!("{name:?}"));
            methods.push(Method {
                interface: interface.clone(),
                function,
                resource,
                kind: match function.kind {
                    FunctionKind::Static(_) | FunctionKind::AsyncStatic(_) => Some("static"),
                    FunctionKind::Constructor(_) => Some("constructor"),
                    _ => None,
                },
                asynchronous,
                params,
                result,
            });
        }
        let mut rust = String::from(
            "// Generated by build.rs from the bundled WASI WIT. Do not edit.\n\
             use wasmtime_wasi::{p3, cli::WasiCli, clocks::WasiClocks, filesystem::WasiFilesystem};\n\
             use wasmtime_wasi_http::WasiHttp;\n\
             use wasmtime::{AsContextMut, component::Accessor};\n",
        );
        let _ = writeln!(
            rust,
            "#[cfg(test)] const METHOD_NAMES: &[&str] = &[{}];",
            names_list.join(", ")
        );
        let interfaces: Vec<String> = methods
            .iter()
            .map(|m| format!("{:?}", m.interface))
            .collect();
        let asynchronous: Vec<String> =
            methods.iter().map(|m| m.asynchronous.to_string()).collect();
        let byte_stream = self
            .type_index
            .get("[\"stream\", \"u8\"]")
            .map_or(u32::MAX, |index| *index as u32);
        let _ = write!(
            rust,
            "/// Interface of each generated method; dispatch requires the Orb to import it.\n\
             pub(super) const INTERFACES: &[&str] = &[{}];\n\
             /// WIT `async func` methods run as Store tasks; the others reply synchronously.\n\
             pub(super) const ASYNCHRONOUS: &[bool] = &[{}];\n\
             /// `TYPES` index of `stream<u8>`, the only qualified guest-writable stream type.\n\
             pub(super) const BYTE_STREAM: u32 = {byte_stream};\n\
\n\
             /// Decode, validate and call one generated method.\n\
             #[allow(unused_variables, unused_mut, clippy::all)]\n\
             fn dispatch(method: u32, r: &mut Reader<'_>, mut store: StoreContextMut<'_, State>) -> wasmtime::Result<Call> {{ let state = store.data_mut();\n\
             match method {{\n{arms}        _ => Err(invalid(\"unknown standard method\")),\n    }}\n}}\n\n",
            interfaces.join(", "),
            asynchronous.join(", "),
        );
        let (futures, writable) = self.writable_futures();
        rust += &futures;
        rust += &self.helpers;
        let budgeted: Vec<String> = (0..self.resources.len())
            .map(|kind| self.budgeted_kind(kind).to_string())
            .collect();
        let _ = writeln!(
            rust,
            "/// Per resource kind: whether its handles hold a unit of the open-file budget.\n\
             pub(super) const BUDGETED: &[bool] = &[{}];",
            budgeted.join(", ")
        );
        let mut drops = String::new();
        let mut store_drops = String::new();
        let mut closing = Vec::new();
        for (kind, (qualified, path)) in self.resources.iter().enumerate() {
            let module = path.rsplit_once("::").expect("type path").0;
            let resource = path.rsplit_once("::").expect("type path").1;
            let (_, data, getter, _) = VIEWS
                .iter()
                .find(|(package, _, _, _)| module.contains(&format!("p3::bindings::{package}::")))
                .expect("view");
            if STORE_DROPS.contains(&qualified.as_str()) {
                // Queued; closed where a Store is held (`close_queued`).
                let _ = writeln!(
                    drops,
                    "        {kind} => {{ state.standard.handles.close(Close::Resource(kind, rep)); Ok(()) }},"
                );
                let _ = writeln!(
                    store_drops,
                    "        {kind} => <{data} as {module}::Host{resource}WithStore<State>>::drop(wasmtime::component::Access::<State, {data}>::new(store, {getter}), Resource::new_own(rep)),"
                );
                closing.push("true");
            } else {
                let _ = writeln!(
                    drops,
                    "        {kind} => {module}::Host{resource}::drop(&mut {getter}(state), Resource::new_own(rep)),"
                );
                closing.push("false");
            }
        }
        let _ = writeln!(
            rust,
            "/// Per resource kind: whether upstream drops it through a store function.\n\
             pub(super) const STORE_DROPPED: &[bool] = &[{}];",
            closing.join(", ")
        );
        let _ = write!(
            rust,
            "/// Close one queued store-drop resource through its upstream `WithStore` drop.\n\
             pub(super) fn drop_queued(kind: u32, rep: u32, store: StoreContextMut<'_, State>) -> wasmtime::Result<()> {{\n\
             match kind {{\n{store_drops}        _ => Err(invalid(\"unknown store-drop resource kind\")),\n    }}\n}}\n"
        );
        let _ = write!(
            rust,
            "/// Drop one owned resource through its upstream host implementation.\n\
             pub(super) fn drop_resource(kind: u32, rep: u32, state: &mut State) -> wasmtime::Result<()> {{\n\
             match kind {{\n{drops}        _ => Err(invalid(\"unknown resource kind\")),\n    }}\n}}\n"
        );
        (rust, methods, writable)
    }

    /// Generated guest-writable future support: the admitted `TYPES` indices, their typed
    /// promise constructors (with the abandonment value) and their one-shot write decoders.
    fn writable_futures(&mut self) -> (String, Vec<usize>) {
        let mut futures = BTreeMap::new();
        for (_, function, _) in self.functions() {
            for param in &function.params {
                let ty = match self.peel(param.ty) {
                    Type::Id(id) => match self.resolve.types[id].kind {
                        TypeDefKind::Option(inner) => inner,
                        _ => param.ty,
                    },
                    other => other,
                };
                if qualify::writable_future(self.resolve, ty) {
                    let Type::Id(id) = self.peel(ty) else {
                        unreachable!()
                    };
                    let TypeDefKind::Future(Some(payload)) = self.resolve.types[id].kind else {
                        unreachable!()
                    };
                    futures.insert(self.ts(ty), payload);
                }
            }
        }
        let mut create = String::new();
        let mut write = String::new();
        for (index, payload) in &futures {
            let rust = self.rust_type(*payload);
            let Type::Id(id) = self.peel(*payload) else {
                unreachable!()
            };
            let TypeDefKind::Result(result) = &self.resolve.types[id].kind else {
                unreachable!()
            };
            let Some(Type::Id(error)) = result.err.map(|ty| self.peel(ty)) else {
                unreachable!()
            };
            let error = self.rust_path(error);
            let decode = self.decode(*payload);
            let _ = writeln!(
                create,
                "        {index} => Some(Promise::<{rust}>::create(closing, || Err({error}::InternalError(Some(String::from(ABANDONED_FUTURE)))))),"
            );
            let _ = writeln!(
                write,
                "        {index} => {{ let promise = Promise::<{rust}>::of(promise)?; let mut seen = Vec::new(); let value: {rust} = {decode}; r.end()?; promise.lock().expect(\"promise\").writable()?; let owned = h.commit(seen); promise.lock().expect(\"promise\").write(value, owned); Ok(()) }}"
            );
        }
        let indices: Vec<String> = futures.keys().map(usize::to_string).collect();
        let source = format!(
            "/// `TYPES` indices of the futures a guest may create and write (FUTURE_NEW).\n\
             #[allow(dead_code)] pub(super) const WRITABLE_FUTURES: &[u32] = &[{}];\n\
             /// Create the writer state of one writable future type.\n\
             #[allow(unused_variables, clippy::all)]\n\
             fn future_new(ty: u32, closing: &Shared<Vec<Close>>) -> Option<Arc<dyn Promised>> {{\n\
             match ty {{\n{create}        _ => None,\n    }}\n}}\n\
             /// Decode and accept the one value of a writable future (FUTURE_WRITE).\n\
             #[allow(unused_variables, unused_mut, clippy::all)]\n\
             fn write_future(ty: u32, promise: Arc<dyn Promised>, r: &mut Reader<'_>, h: &mut Handles) -> wasmtime::Result<()> {{\n\
             match ty {{\n{write}        _ => Err(invalid(\"future type is not writable by the guest\")),\n    }}\n}}\n",
            indices.join(", ")
        );
        (source, futures.into_keys().collect())
    }

    /// Invoke WIT-synchronous Store methods immediately, before their borrowed arguments drop.
    /// Input byte streams transfer only after the whole packet validated and capacity for the
    /// result handles is reserved; outputs bind to bounded host pipes.
    pub(super) fn store_arm(
        &mut self,
        function: &Function,
        trait_name: &str,
        (data, getter, blocking): (&str, &str, bool),
        args: &str,
        (commits, transfers): (&str, &str),
    ) -> String {
        let result = self.peel(function.result.expect("store methods return handles"));
        let Type::Id(id) = result else {
            panic!("store result")
        };
        let (elements, tupled) = match self.resolve.types[id].kind.clone() {
            TypeDefKind::Tuple(tuple) => (tuple.types, true),
            TypeDefKind::Future(_) | TypeDefKind::Stream(_) => (vec![result], false),
            other => panic!("store result {other:?} has no binding rule"),
        };
        let mut reserve = String::new();
        let mut bind = String::new();
        let mut names = Vec::new();
        let mut streams = 0;
        for (index, ty) in elements.iter().enumerate() {
            let Type::Id(inner) = self.peel(*ty) else {
                panic!("handle")
            };
            if matches!(self.resolve.types[inner].kind, TypeDefKind::Stream(_)) {
                streams += 1;
            }
            match self.resolve.types[inner].kind.clone() {
                TypeDefKind::Handle(Handle::Own(resource)) => {
                    let kind = self.resource(resource);
                    let _ = write!(reserve, "w.u64(h.own({kind}, t{index}));");
                }
                TypeDefKind::Stream(Some(Type::U8)) => {
                    let _ = write!(
                        reserve,
                        "let (s{index}, pipe{index}) = h.stream(files.pop())?; w.u64(s{index});"
                    );
                    let _ = write!(bind, "t{index}.pipe(store.as_context_mut(), pipe{index})?;");
                }
                TypeDefKind::Stream(Some(item)) => {
                    let encode = self.encode(item, "value");
                    let _ = write!(
                        reserve,
                        "let (s{index}, pipe{index}) = h.items()?; w.u64(s{index});"
                    );
                    let _ = write!(
                        bind,
                        "t{index}.pipe(store.as_context_mut(), ItemConsumer::new(pipe{index}, files.pop(), |w: &mut Writer, value| {{ {encode} Ok(()) }}))?;"
                    );
                }
                TypeDefKind::Future(Some(payload)) => {
                    let encode = self.encode(payload, "value");
                    let _ = write!(
                        reserve,
                        "let (f{index}, slot{index}) = h.future()?; w.u64(f{index});"
                    );
                    let _ = write!(
                        bind,
                        "t{index}.pipe(store.as_context_mut(), Slot::new(slot{index}, |w: &mut Writer, h: &mut Handles, value| {{ {encode} Ok(()) }}))?;"
                    );
                }
                other => panic!("store result {other:?} has no binding rule"),
            }
            names.push(format!("t{index}"));
        }
        let pattern = if tupled {
            format!("({})", names.join(", "))
        } else {
            names.join(", ")
        };
        // Owned claims commit, then input streams and futures transfer their readable ends.
        let transfer = format!("{commits} {transfers}");
        // Capacity is reserved before upstream creates host stream/future state, so a full
        // table refuses the call instead of dropping freshly created transmit ends.
        format!(
            "h.reserve({})?; let mut files = h.files({})?; {transfer} let {pattern} = {trait_name}::{}(wasmtime::component::Access::<State, {data}>::new(store.as_context_mut(), {getter}), {args})?; \
             let h = &mut store.data_mut().standard.handles; let mut out = Writer::default(); let w = &mut out; {reserve} \
             {bind} Ok(Call::Ready(out.0))",
            elements.len(),
            // Only filesystem streams hold open files; stdio streams draw no budget.
            if blocking {
                streams + transfers.matches("files.pop()").count()
            } else {
                0
            },
            match function.kind {
                FunctionKind::Constructor(_) => "new".to_owned(),
                _ => snake(&function.item_name().replace('-', "_")),
            }
        )
    }
}
