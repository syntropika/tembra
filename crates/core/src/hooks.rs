use crate::Reason;
use crate::{
    Code, Error, Host, Id, Result,
    host::{conflict, gone},
};
use std::rc::Rc;

/// An ordinary managed operation attached to an orb-owned extension point.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Contribution {
    pub orb: Id,
    pub target: String,
    pub hook: String,
    pub operation: Id,
    pub priority: i32,
    pub ordinal: u32,
}

impl Host {
    /// Registers metadata once. Pipeline composition and all values belong to orb code.
    pub fn contribute(&self, contribution: Contribution) -> Result<()> {
        let mut inner = self.inner.borrow_mut();
        if contribution.operation == 0 {
            return Err(Error::invalid(
                Reason::Operation,
                "operation identity is required",
            ));
        }
        let orb = inner
            .orbs
            .get(&contribution.orb)
            .ok_or_else(|| gone(Reason::Orb))?;
        let own = orb.declaration.name == contribution.target;
        if !own
            && !orb
                .declaration
                .requires
                .iter()
                .any(|r| r.orb == contribution.target && r.hooks.contains(&contribution.hook))
        {
            return Err(Error::new(
                Code::Forbidden,
                Reason::HookDependency,
                "hook use must be declared in requires",
            ));
        }
        let target = inner
            .current()
            .get(&contribution.target)
            .and_then(|id| inner.orbs.get(id))
            .ok_or_else(|| gone(Reason::Orb))?;
        if !target.declaration.hooks.contains(&contribution.hook) {
            return Err(gone(Reason::Hook));
        }
        if inner
            .contributions
            .iter()
            .any(|c| c.orb == contribution.orb && c.operation == contribution.operation)
        {
            return Err(conflict(Reason::DuplicateContribution));
        }
        inner.contributions.push(contribution);
        inner.contributions.sort_by_key(|c| (c.priority, c.ordinal));
        inner.plans.clear();
        Ok(())
    }

    /// Returns one immutable, version-selected plan, shared between runs of the same version.
    pub fn hook_plan(&self, root: Id, target: &str, hook: &str) -> Result<Rc<[Id]>> {
        let mut inner = self.inner.borrow_mut();
        let version = inner.check_root(root)?.version;
        let key = (version, target.to_owned(), hook.to_owned());
        if let Some(plan) = inner.plans.get(&key) {
            return Ok(plan.clone());
        }
        let snapshot = inner
            .versions
            .get(&version)
            .ok_or_else(|| conflict(Reason::Superseded))?;
        let target_id = snapshot.get(target).ok_or_else(|| gone(Reason::Orb))?;
        if !inner.orbs[target_id].declaration.hooks.contains(hook) {
            return Err(gone(Reason::Hook));
        }
        let plan: Rc<[Id]> = inner
            .contributions
            .iter()
            .filter(|c| {
                c.target == target && c.hook == hook && snapshot.values().any(|id| *id == c.orb)
            })
            .map(|c| c.operation)
            .collect::<Vec<_>>()
            .into();
        inner.plans.insert(key, plan.clone());
        Ok(plan)
    }
}
