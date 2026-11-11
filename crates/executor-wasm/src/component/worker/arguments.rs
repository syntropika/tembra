//! Preflight and consume the typed owned arguments of one canonical call.
use super::*;

pub(super) struct Prepared {
    pub(super) values: Vec<Val>,
    pub(super) accepted: Option<oneshot::Sender<()>>,
}

/// Refusals before the first handle is consumed leave a healthy Store usable. After that
/// point a lowering failure is a physical Store failure and must quarantine the revision.
pub(super) fn lower(
    store: &mut Store<State>,
    ty: &ComponentFunc,
    args: &[Value],
    lent: usize,
    ownership: Option<crate::worker::arguments::Arguments>,
) -> wasmtime::Result<std::result::Result<Prepared, String>> {
    let mut lowered = Vec::with_capacity(args.len());
    let (resources, accepted) = match ownership {
        Some(arguments) => (arguments.resources, Some(arguments.accepted)),
        None => (Vec::new(), None),
    };
    // Validate the complete data tree, exact attachment use, type and issuer before consuming
    // any canonical handle. Partial lowering is otherwise a Store-wide ownership failure.
    let mut used = BTreeSet::new();
    let mut physical = BTreeSet::new();
    for ((_, ty), value) in ty.params().skip(lent).zip(args) {
        let checked = values::lower_with(ty, value, &mut |expected, value| {
            let index =
                slot(value).ok_or_else(|| crate::failure("owned argument names no attachment"))?;
            if !used.insert(index) {
                return Err(crate::failure("an owned argument was used twice"));
            }
            let resource = resources
                .get(index as usize)
                .ok_or_else(|| crate::failure("owned argument names no attachment"))?;
            match resource {
                crate::worker::arguments::Resource::Issued {
                    environment,
                    instance,
                } => {
                    if !physical.insert((Some(*environment), *instance))
                        || store
                            .data()
                            .instances
                            .receiver(*environment, *instance, &expected)
                            .is_none()
                    {
                        return Err(crate::failure(
                            "the owned argument instance is gone or has another type",
                        ));
                    }
                }
                crate::worker::arguments::Resource::Imported { slot } => {
                    if !physical.insert((None, *slot))
                        || !store
                            .data()
                            .payloads
                            .iter()
                            .any(|payload| ResourceType::host_dynamic(*payload) == expected)
                    {
                        return Err(crate::failure(
                            "the owned argument is not a linked imported type",
                        ));
                    }
                }
            }
            Ok(Val::Bool(false))
        });
        if let Err(error) = checked {
            return Ok(Err(error.to_string()));
        }
    }
    if used.len() != resources.len() {
        return Ok(Err("unused owned argument attachment".into()));
    }
    for ((_, ty), value) in ty.params().skip(lent).zip(args) {
        let value = values::lower_with(ty, value, &mut |expected, value| {
            let index =
                slot(value).ok_or_else(|| crate::failure("owned argument names no attachment"))?;
            match &resources[index as usize] {
                crate::worker::arguments::Resource::Issued {
                    environment,
                    instance,
                } => {
                    let resource = store
                        .data_mut()
                        .instances
                        .remove(*environment, *instance)
                        .ok_or_else(|| crate::failure("the owned argument instance disappeared"))?;
                    Ok(Val::Resource(resource))
                }
                crate::worker::arguments::Resource::Imported { slot } => {
                    let payload = store
                        .data()
                        .payloads
                        .iter()
                        .copied()
                        .find(|payload| ResourceType::host_dynamic(*payload) == expected)
                        .ok_or_else(|| crate::failure("the owned argument type disappeared"))?;
                    crate::component::resources::owned_value(&mut *store, *slot, payload)
                        .map_err(crate::failure)
                }
            }
        })
        .map_err(|error| wasmtime::Error::msg(error.to_string()))?;
        lowered.push(value);
    }
    Ok(Ok(Prepared {
        values: lowered,
        accepted,
    }))
}
