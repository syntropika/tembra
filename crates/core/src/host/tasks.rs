use super::*;
use crate::Reason;

impl Host {
    /// Retain an owner's implementation version for physical task work.
    pub fn begin_task(&self, owner: Owner) -> Result<Id> {
        let mut inner = self.inner.borrow_mut();
        inner.open()?;
        inner.owner(owner)?;
        let id = inner.id()?;
        let version = inner.version;
        if let Some(orb) = inner.task_orb(owner) {
            inner.orb_tasks.insert(orb, id);
        }
        inner.owner_tasks.insert(owner, id);
        inner.owner_tasks.insert(Owner::Host, id);
        inner.tasks.insert(
            id,
            Task {
                owner,
                cancelled: false,
                version,
            },
        );
        if let Owner::Object(object) = owner {
            inner.objects.get_mut(&object).expect("checked owner").tasks += 1;
        }
        Ok(id)
    }

    /// Issue an operation root under a live task's retained version.
    pub fn task_root(&self, task: Id, principal: impl Into<String>) -> Result<Id> {
        let mut inner = self.inner.borrow_mut();
        inner.open()?;
        let record = inner.tasks.get(&task).ok_or_else(|| gone(Reason::Task))?;
        if record.cancelled {
            return Err(Error::new(
                Code::Cancelled,
                Reason::Task,
                "task is cancelled",
            ));
        }
        let version = inner.version;
        inner.root(version, principal.into(), false)
    }

    /// Request task cancellation without releasing its ownership.
    pub fn stop_task(&self, task: Id) -> Result<()> {
        let mut inner = self.inner.borrow_mut();
        let task_record = inner
            .tasks
            .get_mut(&task)
            .ok_or_else(|| gone(Reason::Task))?;
        if !task_record.cancelled {
            task_record.cancelled = true;
            inner.actions.push_back(Action::CancelTask(task));
        }
        Ok(())
    }

    /// Release physical task ownership and wake dependent cleanup.
    pub fn finish_task(&self, task: Id) -> Result<()> {
        let mut inner = self.inner.borrow_mut();
        let task_id = task;
        let task = inner
            .tasks
            .remove(&task)
            .ok_or_else(|| gone(Reason::Task))?;
        if let Some(orb) = inner.task_orb(task.owner) {
            inner.orb_tasks.remove(&orb, task_id);
        }
        inner.owner_tasks.remove(&task.owner, task_id);
        inner.owner_tasks.remove(&Owner::Host, task_id);
        inner.settled_owner(task.owner);
        if let Owner::Object(object) = task.owner {
            let orb = inner.objects[&object].orb;
            let owner = inner.orbs[&orb].owner;
            inner.settled_owner(Owner::Orb(owner));
            inner
                .objects
                .get_mut(&object)
                .expect("task retains object")
                .tasks -= 1;
            inner.maybe_dispose(object);
        }
        Ok(())
    }
}
