//! Admit retained roots while blocking incompatible new work.

use super::*;

impl Host {
    /// Stops first access by old roots while allowing already admitted roots to reenter.
    /// Returns true only after their complete roots and owned tasks physically settle. Ending or
    /// cancelling a root does not release its lease while a callback is still running.
    pub fn gate(&self, ticket: Id, objects: &[Id]) -> Result<bool> {
        let mut inner = self.inner.borrow_mut();
        let Inner {
            pending,
            objects: records,
            object_roots,
            ..
        } = &mut *inner;
        let pending = pending
            .as_mut()
            .filter(|p| p.reload.id == ticket)
            .ok_or_else(|| conflict(Reason::ReloadTicket))?;
        if objects.iter().any(|id| !pending.objects.contains(id)) {
            return Err(Error::invalid(Reason::GateObject, "object is not affected"));
        }
        for id in objects {
            records
                .get_mut(id)
                .ok_or_else(|| gone(Reason::Object))?
                .gate
                .get_or_insert_with(|| object_roots.members(id).collect());
        }
        pending.gated.extend(objects);
        Ok(objects.iter().all(|id| !inner.gate_object_busy(*id)))
    }

    /// Drains an exclusive orb-owned resource, including hook invocations without an object.
    pub fn gate_orbs(&self, ticket: Id, owners: &[Id]) -> Result<bool> {
        let mut inner = self.inner.borrow_mut();
        let Inner {
            pending, orb_roots, ..
        } = &mut *inner;
        let pending = pending
            .as_mut()
            .filter(|pending| pending.reload.id == ticket)
            .ok_or_else(|| conflict(Reason::ReloadTicket))?;
        if owners.iter().any(|owner| !pending.owners.contains(owner)) {
            return Err(Error::invalid(Reason::GateOrb, "orb is not affected"));
        }
        for owner in owners {
            pending
                .gated_orbs
                .entry(*owner)
                .or_insert_with(|| orb_roots.members(owner).collect());
        }
        Ok(owners.iter().all(|owner| !inner.orb_busy(*owner)))
    }
}
