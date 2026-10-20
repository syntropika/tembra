//! Synchronous views over the exact core-selected foreign implementation.
use super::{implementation::Aliases, implementation::SelectedImplementation, *};
use std::collections::BTreeMap;

pub(super) fn declarations<'s>(
    engine: &Rc<Engine>,
    scope: &mut Scope<'s, '_>,
    setup: &Rc<Owner>,
    provider: &Contract,
    raw: v8::Local<'s, v8::Value>,
) -> Result<BTreeMap<u64, ContractAdapter>> {
    if raw.is_undefined() {
        return Ok(BTreeMap::new());
    }
    let raw = helper(engine, scope, "adapterEntries", &[raw])?;
    let entries = v8::Local::<v8::Array>::try_from(raw)
        .map_err(|_| invalid("contract_adapter", "adapters must be a dense data array"))?;
    if entries.length() as usize > engine.options.max_callbacks {
        return Err(limit("callbacks"));
    }
    let mut adapters = BTreeMap::new();
    for index in 0..entries.length() {
        let entry = object(arg(scope, entries, index)?)?;
        let offered = token(scope, entry, "provider")?;
        let consumer = token(scope, entry, "consumer")?;
        if offered.fingerprint() != provider.fingerprint() {
            return Err(invalid(
                "contract_adapter",
                "adapter belongs to another provider",
            ));
        }
        if consumer.name() != provider.name()
            || consumer.kind() != provider.kind()
            || consumer.placement() != provider.placement()
        {
            return Err(invalid(
                "contract_adapter",
                "adapters cannot change name, kind or placement",
            ));
        }
        if consumer.version() == provider.version() || adapters.contains_key(&consumer.version()) {
            return Err(invalid(
                "contract_adapter",
                "adapter versions must be distinct and unique",
            ));
        }
        let callback = callback_for(setup, get(scope, entry, "adapt")?, scope)?;
        let version = consumer.version();
        let adapter: ContractAdapter = Rc::new(move |source| apply(&callback, &consumer, source));
        adapters.insert(version, adapter);
    }
    Ok(adapters)
}

fn token<'s>(
    scope: &mut Scope<'s, '_>,
    entry: v8::Local<'s, v8::Object>,
    name: &str,
) -> Result<Rc<Contract>> {
    let value = get(scope, entry, name)?;
    let text = string(value, scope)?;
    Contract::from_json(&text)
        .map(Rc::new)
        .map_err(|error| invalid(error.reason, error.message))
}

fn apply(
    callback: &Rc<Callback>,
    consumer: &Contract,
    source: Rc<Implementation>,
) -> Result<Implementation> {
    let anchor = source
        .retained_owner::<SelectedImplementation>()
        .ok_or_else(|| {
            invalid(
                "foreign_implementation",
                "selected implementation has no native JavaScript projection",
            )
        })?;
    if anchor.owner.module.host != callback.module.host {
        return Err(invalid(
            "foreign_host",
            "adapter source belongs to another Host",
        ));
    }
    if anchor.owner.module.engine.id != callback.module.engine.id {
        return Err(invalid(
            "generation",
            "adapter source belongs to another engine",
        ));
    }
    let owner = Owner::with_setup(
        &callback.module,
        anchor.owner.invocation.clone(),
        callback.slot.setup.clone(),
    );
    let aliases = Aliases { source, anchor };
    callback.module.engine.with_scope(|scope| {
        v8::tc_scope!(let scope, scope);
        let projection = v8::Local::new(scope, &aliases.anchor.projection);
        let function = v8::Local::new(scope, &callback.function);
        let receiver = v8::undefined(scope).into();
        let result = function
            .call(scope, receiver, &[projection.into()])
            .ok_or_else(|| {
                let caught = scope.exception();
                exception(scope, caught, "contract adapter threw")
            })?;
        super::implementation::parse(
            scope,
            &callback.module,
            &owner,
            consumer,
            result,
            Some(&aliases),
        )
    })
}
