//! Encode kernel effects and result metadata.

use super::*;

/// Encode the next program step without executing callbacks.
pub(super) fn step_value(step: Step) -> Value {
    match step {
        Step::Wait { deadline } => {
            json!({
                "type": StepType::Wait.as_str(),
                "deadline": deadline.map(|deadline|deadline.to_string()),
            })
        }
        Step::Prepared => json!({ "type": StepType::Prepared.as_str() }),
        Step::Complete => json!({ "type": StepType::Complete.as_str() }),
        Step::Failed(error) => {
            json!({
                "type": StepType::Failed.as_str(),
                "error": error_value(error),
            })
        }
        Step::Execute(effects) => {
            json!({
                "type": StepType::Execute.as_str(),
                "effects": effects.into_iter().map(effect_value).collect::<Vec<_>>(),
            })
        }
    }
}

/// Preserve the structured error fields across adapters.
pub(super) fn error_value(error: Error) -> Value {
    json!({
        "code": error.code.as_str(),
        "reason": error.reason,
        "message": error.message,
    })
}

/// Encode callback requests using stable wire names.
pub(super) fn effect_value(effect: Effect) -> Value {
    match effect {
        Effect::Stop {
            scope,
            keep_listeners,
        } => json!({
            "type": EffectType::Stop.as_str(),
            "scope": scope,
            "keepListeners": keep_listeners,
        }),
        Effect::Open { resource } => {
            json!({ "type": EffectType::Open.as_str(), "resource": resource })
        }
        Effect::Close { resource } => {
            json!({ "type": EffectType::Close.as_str(), "resource": resource })
        }
        Effect::Restore { previous, resource } => {
            json!({
                "type": EffectType::Restore.as_str(),
                "previous": previous,
                "resource": resource,
            })
        }
        Effect::Prepare { scope, previous } => {
            json!({
                "type": EffectType::Prepare.as_str(),
                "scope": scope,
                "previous": previous,
            })
        }
        Effect::Validate { scope, value } => {
            json!({
                "type": EffectType::Validate.as_str(),
                "scope": scope,
                "value": value,
            })
        }
        Effect::Initialize { scope } => {
            json!({ "type": EffectType::Initialize.as_str(), "scope": scope })
        }
        Effect::Migrate {
            scope,
            value,
            version,
        } => {
            json!({
                "type": EffectType::Migrate.as_str(),
                "scope": scope,
                "value": value,
                "version": version,
            })
        }
        Effect::Adopt { replacements } => {
            json!({
                "type": EffectType::Adopt.as_str(),
                "replacements": replacements.into_iter().map(|pair|json!({ "previous": pair.previous, "next": pair.next })).collect::<Vec<_>>(),
            })
        }
        Effect::Start { scope } => json!({ "type": EffectType::Start.as_str(), "scope": scope }),
        Effect::Drop { scope } => json!({ "type": EffectType::Drop.as_str(), "scope": scope }),
    }
}

/// Encode an engine action for the owning language adapter.
pub(super) fn action_value(action: Action) -> Value {
    match action {
        Action::Execute { program, effects } => {
            json!({
                "type": ActionType::Execute.as_str(),
                "id": program,
                "effects": effects.into_iter().map(effect_value).collect::<Vec<_>>(),
            })
        }
        Action::CancelEffects { program, effects } => {
            json!({
                "type": ActionType::CancelEffects.as_str(),
                "id": program,
                "effects": effects.into_iter().map(effect_value).collect::<Vec<_>>(),
            })
        }
        Action::ProgramPrepared(id) => {
            json!({ "type": ActionType::ProgramPrepared.as_str(), "id": id })
        }
        Action::ProgramComplete { program, error } => {
            json!({
                "type": ActionType::ProgramComplete.as_str(),
                "id": program,
                "error": error.map(error_value),
            })
        }
        Action::DisposeObject(id) => json!({ "type": ActionType::Dispose.as_str(), "id": id }),
        Action::CloseResource(id) => {
            json!({ "type": ActionType::CloseResource.as_str(), "id": id })
        }
        Action::DropValue(id) => json!({ "type": ActionType::DropValue.as_str(), "id": id }),
        Action::CancelRoot(id) => json!({ "type": ActionType::CancelRoot.as_str(), "id": id }),
        Action::CancelTask(id) => json!({ "type": ActionType::CancelTask.as_str(), "id": id }),
        Action::RetireOrb(id) => json!({ "type": ActionType::RetireOrb.as_str(), "id": id }),
        Action::StateChanged { object, revision } => {
            json!({
                "type": ActionType::State.as_str(),
                "id": object,
                "revision": revision.to_string(),
            })
        }
        #[cfg(feature = "subscriptions")]
        Action::SubscriptionFailed(id) => {
            json!({ "type": ActionType::SubscriptionFailed.as_str(), "id": id })
        }
        #[cfg(feature = "subscriptions")]
        Action::DetachSubscription(id) => {
            json!({ "type": ActionType::DetachSubscription.as_str(), "id": id })
        }
        #[cfg(feature = "subscriptions")]
        Action::Deliver {
            subscription,
            payload,
            operation,
        } => {
            json!({
                "type": ActionType::Deliver.as_str(),
                "id": subscription,
                "payload": payload,
                "operation": operation,
            })
        }
    }
}

/// Encode admitted call identity and retained implementation version.
pub(super) fn invocation_value(i: &Invocation) -> Value {
    json!({
        "id": i.id,
        "root": i.root,
        "object": i.object,
        "orb": i.orb,
        "codeVersion": i.code_version,
        "method": i.method,
    })
}

/// Encode revisions as text to preserve all bits in JavaScript.
pub(super) fn snapshot_value(s: Snapshot) -> Value {
    json!({
        "value": s.value,
        "revision": s.revision.to_string(),
        "version": s.version,
    })
}

/// Describe an orb version without exposing its implementation.
pub(super) fn orb_value(f: &InstalledOrb) -> Value {
    json!({
        "id": f.id,
        "owner": f.owner,
        "codeVersion": f.code_version,
        "name": f.declaration.name,
        "apiVersion": f.declaration.api_version,
        "services": f.declaration.services.iter().map(|s|json!({
            "name": s.name,
            "methods": s.methods,
            "lifetime": s.lifetime.as_str(),
            "linger": s.linger_ms,
        })).collect::<Vec<_>>(),
        "hooks": f.declaration.hooks,
        "requires": f.declaration.requires.iter().map(|requirement|json!({
            "orb": requirement.orb,
            "apiVersion": requirement.api_version,
            "services": requirement.methods,
            "hooks": requirement.hooks,
        })).collect::<Vec<_>>(),
    })
}
