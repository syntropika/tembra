use std::{
    cell::{Cell, RefCell},
    collections::VecDeque,
    rc::{Rc, Weak},
};

use super::{StateEvent, port::ReadPort};

pub(crate) struct PendingPublication {
    pub(super) port: Weak<ReadPort>,
    pub(super) event: StateEvent,
    pub(super) listeners: Vec<u64>,
}

/// One host's synchronous FIFO. Only complete, already-adopted batches enter it.
pub(crate) struct PublicationQueue {
    observations: Option<crate::Observations>,
    pending: RefCell<VecDeque<Vec<Publication>>>,
    held: Cell<usize>,
    draining: Cell<bool>,
}

impl PublicationQueue {
    #[cfg(test)]
    pub fn new() -> Rc<Self> {
        Self::observed(None)
    }

    pub(crate) fn observed(observations: Option<crate::Observations>) -> Rc<Self> {
        Rc::new(Self {
            observations,
            pending: RefCell::default(),
            held: Cell::new(0),
            draining: Cell::new(false),
        })
    }

    pub(crate) fn observations(&self) -> &Option<crate::Observations> {
        &self.observations
    }

    pub fn append_batch(&self, batch: Vec<PendingPublication>) {
        if batch.is_empty() {
            return;
        }
        self.pending
            .borrow_mut()
            .push_back(batch.into_iter().map(Publication::State).collect());
        self.drain();
    }

    pub(crate) fn append_registry_batch(
        &self,
        batch: Vec<crate::registry::PendingRegistryPublication>,
    ) {
        if batch.is_empty() {
            return;
        }
        self.pending
            .borrow_mut()
            .push_back(batch.into_iter().map(Publication::Registry).collect());
        self.drain();
    }

    pub fn hold(self: &Rc<Self>) -> PublicationHold {
        self.held.set(self.held.get() + 1);
        PublicationHold {
            queue: self.clone(),
        }
    }

    fn drain(&self) {
        if self.held.get() != 0 || self.draining.replace(true) {
            return;
        }
        let _guard = DrainGuard(&self.draining);
        loop {
            let batch = self.pending.borrow_mut().pop_front();
            let Some(batch) = batch else { break };
            for publication in batch {
                let operation = match &publication {
                    Publication::State(_) => crate::observability::Operation::StatePublication,
                    Publication::Registry(_) => {
                        crate::observability::Operation::RegistryPublication
                    }
                };
                let mut observation = crate::observability::start(
                    &self.observations,
                    operation,
                    "publication",
                    None,
                    None,
                    None,
                );
                match publication {
                    Publication::State(publication) => {
                        if let Some(port) = publication.port.upgrade() {
                            port.deliver(publication.event, publication.listeners);
                        }
                    }
                    Publication::Registry(publication) => publication.deliver(),
                }
                if let Some(observation) = &mut observation {
                    observation.finish(crate::observability::Outcome::Ok);
                }
            }
            if self.held.get() != 0 {
                break;
            }
        }
    }
}

pub(crate) struct PublicationHold {
    queue: Rc<PublicationQueue>,
}

impl Drop for PublicationHold {
    fn drop(&mut self) {
        self.queue.held.set(self.queue.held.get() - 1);
        // Avoid a second panic during unwinding; a later append resumes queued delivery.
        if !std::thread::panicking() {
            self.queue.drain();
        }
    }
}

struct DrainGuard<'a>(&'a Cell<bool>);
impl Drop for DrainGuard<'_> {
    fn drop(&mut self) {
        self.0.set(false);
    }
}

enum Publication {
    State(PendingPublication),
    Registry(crate::registry::PendingRegistryPublication),
}
