use crate::{
    callbacks::{Callback, NodeValue},
    handles::{Entry, IssuedContext},
    session::Session,
};
use futures::FutureExt;
use serde_json::{Value, json};
use std::{
    cell::RefCell,
    collections::BTreeMap,
    rc::{Rc, Weak},
};
use tembra_core::{contracts::Contract, *};

pub(crate) struct ObjectOwner {
    pub recorder: DeclarationRecorder,
    pub invocation: InvocationOwner,
    pub states: RefCell<BTreeMap<String, Readable>>,
    pub opaque: RefCell<BTreeMap<String, (WeakResource<NodeValue>, String)>>,
    pub projection: RefCell<Option<NodeValue>>,
}

pub(crate) fn contract(value: &Value) -> Result<Rc<Contract>> {
    Contract::from_json(text(value)?)
        .map(Rc::new)
        .map_err(|e| RuntimeError::invalid(e.reason, e.message))
}
pub(crate) fn text(value: &Value) -> Result<&str> {
    value
        .as_str()
        .ok_or_else(|| RuntimeError::invalid("binding", "expected text"))
}
pub(crate) fn parse(value: &Value) -> Result<Value> {
    tembra_contracts::parse_json(text(value)?)
        .map_err(|e| RuntimeError::invalid(e.reason, e.message))
}
pub(crate) fn envelope(value: Option<Value>) -> Value {
    match value {
        Some(value) => json!({"present":true,"value":value}),
        None => json!({"present":false}),
    }
}
pub(crate) fn payload(value: Value) -> Result<Option<Value>> {
    match (
        value.get("present"),
        value.as_object().map(serde_json::Map::len),
    ) {
        (Some(Value::Bool(false)), Some(1)) => Ok(None),
        (Some(Value::Bool(true)), Some(2)) if value.get("value").is_some() => {
            Ok(Some(value["value"].clone()))
        }
        _ => Err(RuntimeError::invalid(
            "payload",
            "expected presence envelope",
        )),
    }
}
pub(crate) fn snapshot(value: &Snapshot) -> Value {
    json!({"revision": value.revision(), "value": value.value()})
}
pub(crate) fn handle(value: &Value) -> Result<&str> {
    value
        .get("$handle")
        .and_then(Value::as_str)
        .ok_or_else(|| RuntimeError::invalid("foreign_handle", "expected issued handle"))
}
pub(crate) fn undefined(value: &Value) -> bool {
    value.get("$undefined") == Some(&Value::Bool(true))
}
pub(crate) fn registry(value: &Value) -> Result<Registry> {
    let value = parse(value)?;
    let fields = value
        .as_object()
        .ok_or_else(|| RuntimeError::invalid("registry", "expected registry definition"))?;
    if fields.len() != 2 {
        return Err(RuntimeError::invalid(
            "registry",
            "unknown or missing registry fields",
        ));
    }
    Registry::new(
        text(&value["name"])?,
        match value["conflict"].as_str() {
            Some("last") => ConflictPolicy::Last,
            Some("error") => ConflictPolicy::Error,
            _ => return Err(RuntimeError::invalid("registry", "invalid conflict policy")),
        },
    )
}

/// Scope lease removes transport authority after physical callback settlement.
pub(crate) struct ContextLease {
    session: Weak<Session>,
    pub wire: Value,
    id: String,
}
impl ContextLease {
    pub fn issue(
        session: &Rc<Session>,
        context: Context,
        invocation: Option<InvocationOwner>,
    ) -> Result<Self> {
        context.check(session.host.id())?;
        let signal = context.cancellation().child();
        let expired = signal.clone();
        context.on_settle(move || {
            expired.cancel(RuntimeError::aborted("expired", "context scope settled"))
        })?;
        let wire = session.issue(Entry::Context(IssuedContext {
            context,
            invocation,
            signal,
        }))?;
        let id = handle(&wire)?.into();
        Ok(Self {
            session: Rc::downgrade(session),
            wire,
            id,
        })
    }
}
impl Drop for ContextLease {
    fn drop(&mut self) {
        if let Some(session) = self.session.upgrade() {
            session.remove(&self.id);
        }
    }
}

pub(crate) fn facet(
    session: &Rc<Session>,
    name: &Value,
    uses: &Value,
    setup: &Value,
    generation: Rc<crate::loader::Generation>,
) -> Result<ExecutableFacet> {
    let _generation = session.enter_generation(Some(generation.clone()));
    let name = text(name)?.to_owned();
    let uses = parse(uses)?
        .as_array()
        .ok_or_else(|| RuntimeError::invalid("facet_uses", "expected descriptor array"))?
        .iter()
        .map(|v| contract(&Value::String(v.to_string())))
        .collect::<Result<Vec<_>>>()?;
    let setup = session.callback(setup)?;
    let weak = Rc::downgrade(session);
    let expected = session.host.id();
    let setup = move |scope: &FacetScope, declaration: &mut FacetDeclaration<'_>| {
        let session = weak
            .upgrade()
            .ok_or_else(|| RuntimeError::unavailable("closed", "Host ended"))?;
        if scope.host_id() != expected {
            return Err(RuntimeError::invalid(
                "foreign_host",
                "facet belongs to another Host",
            ));
        }
        generation.retain_run(scope, &declaration.recorder())?;
        let _generation = session.enter_generation(Some(generation.clone()));
        let scope = session.issue(Entry::Scope(scope.clone()))?;
        let recorder = session.issue(Entry::Recorder(declaration.recorder()))?;
        let result = setup.sync(vec![scope, recorder.clone()]);
        session.remove(handle(&recorder)?);
        let result = result?;
        if !undefined(&result) {
            return Err(RuntimeError::invalid(
                "facet_setup",
                "setup must return undefined",
            ));
        }
        Ok(())
    };
    ExecutableFacet::foreign(name, uses, setup)
}

pub(crate) fn factory(
    session: &Rc<Session>,
    callback: Rc<Callback>,
    token: Rc<Contract>,
) -> Factory {
    let weak = Rc::downgrade(session);
    Rc::new(move |input, principal, declaration| {
        let session = weak
            .upgrade()
            .ok_or_else(|| RuntimeError::unavailable("closed", "Host ended"))?;
        let _generation = session.enter_generation(callback.generation.clone());
        let owner = Rc::new(ObjectOwner {
            recorder: declaration.recorder(),
            invocation: declaration.invocation_owner(),
            states: RefCell::default(),
            projection: RefCell::default(),
            opaque: RefCell::default(),
        });
        let raw = session.issue(Entry::Object(Rc::downgrade(&owner)))?;
        let result = callback.sync(vec![
            json!(input.to_string()),
            json!(principal.to_string()),
            raw.clone(),
        ]);
        session.remove(handle(&raw)?);
        output(&session, &token, owner, result?, None)
    })
}

pub(crate) fn output(
    session: &Rc<Session>,
    token: &Contract,
    owner: Rc<ObjectOwner>,
    output: Value,
    source: Option<Rc<Implementation>>,
) -> Result<Implementation> {
    let variant = match output.get("variant") {
        None => None,
        Some(v) => Some(text(v)?.to_owned()),
    };
    let members = token
        .members(variant.as_deref().unwrap_or_default())
        .ok_or_else(|| RuntimeError::invalid("variant", "unknown implementation variant"))?;
    let mut implementation = Implementation::new().retain_owner(owner.clone());
    if let Some(variant) = variant {
        implementation = implementation.variant(variant);
    }
    if let Some(projection) = output.get("projection") {
        *owner.projection.borrow_mut() = Some(NodeValue::new(session, projection)?);
    }
    if let Some(aliases) = output.get("aliases").and_then(Value::as_object) {
        let source = source.ok_or_else(|| {
            RuntimeError::invalid("contract_adapter", "projection requires selected source")
        })?;
        for (name, target) in aliases {
            implementation = implementation.project_method(name, source.clone(), text(target)?)?;
        }
    }
    let methods = output
        .get("methods")
        .and_then(Value::as_object)
        .ok_or_else(|| RuntimeError::invalid("implementation", "expected method record"))?;
    for (name, value) in methods {
        if !matches!(
            members.get(name),
            Some(tembra_contracts::Member::Method { .. })
        ) {
            return Err(RuntimeError::invalid("member", "undeclared method"));
        }
        let callback = session.callback(value)?;
        let member = members.get(name).expect("checked member");
        if matches!(member,tembra_contracts::Member::Method{args,result} if [args,result].into_iter().flatten().any(|s|matches!(s,tembra_contracts::ValueSchema::Native(_))))
        {
            implementation = crate::hooks::signature(session, member)?.install(
                implementation,
                name.clone(),
                callback,
                session,
                owner.clone(),
            );
            continue;
        }
        let weak = Rc::downgrade(session);
        let invocation = owner.invocation.clone();
        implementation = implementation.json_method(name, move |input, context| {
            let callback = callback.clone();
            let weak = weak.clone();
            let invocation = invocation.clone();
            async move {
                let session = weak
                    .upgrade()
                    .ok_or_else(|| RuntimeError::unavailable("closed", "Host ended"))?;
                let physical = invocation.retain(&context)?;
                let lease = ContextLease::issue(&session, context, Some(invocation))?;
                let (send, receive) = futures::channel::oneshot::channel();
                // An abandoned result observer must not release a running Node callback's receiver.
                // The driver owns the lease and conversion until physical Promise settlement.
                session.scheduler.driver.spawn(
                    async move {
                        let _physical = physical;
                        let result = async {
                            let value = callback
                                .invoke(
                                    vec![lease.wire.clone(), json!(envelope(input).to_string())],
                                    false,
                                )
                                .await?;
                            payload(parse(&value)?)
                        }
                        .await;
                        let _ = send.send(result);
                    }
                    .boxed_local(),
                );
                receive.await.map_err(|_| {
                    RuntimeError::unavailable("javascript", "callback observation ended")
                })?
            }
            .boxed_local()
        });
    }
    for (name, state) in owner.states.borrow().iter() {
        implementation = implementation.state(name, state.clone());
    }
    for (name, (resource, nominal)) in owner.opaque.borrow().iter() {
        if !matches!(members.get(name),Some(tembra_contracts::Member::Opaque{id}) if id==nominal) {
            return Err(RuntimeError::invalid(
                "opaque",
                "capability nominal differs from member",
            ));
        }
        implementation = implementation.opaque(name, resource.upgrade()?);
    }
    Ok(implementation)
}

pub(crate) fn serve_options(
    session: &Rc<Session>,
    provider: &Rc<Contract>,
    flags: &Value,
    adapters: &Value,
) -> Result<ServeOptions> {
    let flags = parse(flags)?;
    let fields = flags
        .as_object()
        .ok_or_else(|| RuntimeError::invalid("serve_options", "expected serve flags"))?;
    if fields
        .keys()
        .any(|key| key != "lifetime" && key != "linger")
    {
        return Err(RuntimeError::invalid("serve_options", "unknown option"));
    }
    let mut options = ServeOptions {
        lifetime: match flags.get("lifetime").map(text).transpose()? {
            None | Some("ref") => Lifetime::Referenced,
            Some("host") => Lifetime::Host,
            _ => return Err(RuntimeError::invalid("serve_options", "unknown lifetime")),
        },
        ..ServeOptions::default()
    };
    if let Some(linger) = flags.get("linger") {
        let ms = linger
            .as_f64()
            .filter(|n| n.is_finite() && *n >= 0.0 && *n <= 9_007_199_254_740_991.0)
            .ok_or_else(|| RuntimeError::invalid("linger", "invalid milliseconds"))?;
        options.linger = std::time::Duration::from_secs_f64(ms / 1000.0);
    }
    if let Some(adapters) = adapters.as_array() {
        for adapter in adapters {
            let offered = contract(&adapter["provider"])?;
            let consumer = contract(&adapter["consumer"])?;
            if offered.fingerprint() != provider.fingerprint()
                || consumer.name() != provider.name()
                || consumer.kind() != provider.kind()
                || consumer.placement() != provider.placement()
                || consumer.version() == provider.version()
                || options.accepts.contains_key(&consumer.version())
            {
                return Err(RuntimeError::invalid(
                    "contract_adapter",
                    "incompatible or duplicate adapter",
                ));
            }
            let callback = session.callback(&adapter["adapt"])?;
            let weak = Rc::downgrade(session);
            let version = consumer.version();
            options.accepts.insert(
                version,
                Rc::new(move |source| {
                    let session = weak
                        .upgrade()
                        .ok_or_else(|| RuntimeError::unavailable("closed", "Host ended"))?;
                    let _generation = session.enter_generation(callback.generation.clone());
                    let selected = source.retained_owner::<ObjectOwner>().ok_or_else(|| {
                        RuntimeError::invalid(
                            "foreign_implementation",
                            "expected Node implementation",
                        )
                    })?;
                    let current = selected
                        .projection
                        .borrow()
                        .as_ref()
                        .ok_or_else(|| {
                            RuntimeError::invalid("contract_adapter", "missing projection")
                        })?
                        .wire();
                    let owner = Rc::new(ObjectOwner {
                        recorder: selected.recorder.clone(),
                        invocation: selected.invocation.clone(),
                        states: RefCell::default(),
                        projection: RefCell::default(),
                        opaque: RefCell::default(),
                    });
                    let raw = session.issue(Entry::Object(Rc::downgrade(&owner)))?;
                    let result = callback.sync(vec![current, raw.clone()]);
                    session.remove(handle(&raw)?);
                    output(&session, &consumer, owner, result?, Some(source))
                }),
            );
        }
    }
    Ok(options)
}
