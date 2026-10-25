use super::*;
use crate::Reason;

impl Host {
    pub fn begin_task(&self, owner: Owner) -> Result<Id> {
        let mut inner = self.inner.borrow_mut();
        inner.open()?;
        inner.owner(owner)?;
        let id = inner.id()?;
        let version = inner.version;
        if let Some(facet) = inner.task_facet(owner) {
            inner.facet_tasks.insert(facet, id);
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

    pub fn finish_task(&self, task: Id) -> Result<()> {
        let mut inner = self.inner.borrow_mut();
        let task_id = task;
        let task = inner
            .tasks
            .remove(&task)
            .ok_or_else(|| gone(Reason::Task))?;
        if let Some(facet) = inner.task_facet(task.owner) {
            inner.facet_tasks.remove(&facet, task_id);
        }
        inner.owner_tasks.remove(&task.owner, task_id);
        inner.owner_tasks.remove(&Owner::Host, task_id);
        inner.settled_owner(task.owner);
        if let Owner::Object(object) = task.owner {
            let facet = inner.objects[&object].facet;
            let owner = inner.facets[&facet].owner;
            inner.settled_owner(Owner::Facet(owner));
        }
        if let Owner::Object(object) = task.owner {
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
