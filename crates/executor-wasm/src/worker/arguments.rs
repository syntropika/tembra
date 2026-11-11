//! Linear argument carriers beside the private packet. Numbers in packets alone grant nothing.
use std::sync::{Arc, Mutex};
use tokio::sync::oneshot;

/// Owned arguments already consumed by the sender's canonical ABI. Until the host redeems
/// their slots, this guard queues their release on every abandoned or refused path.
pub(crate) struct Moves {
    slots: Vec<u32>,
    dropped: Arc<Mutex<Vec<u32>>>,
}

impl Moves {
    pub(crate) fn new(slots: Vec<u32>, dropped: Arc<Mutex<Vec<u32>>>) -> Self {
        Self { slots, dropped }
    }

    /// The host now owns every slot, including cleanup if a later slot proves invalid.
    pub(crate) fn redeem(mut self) -> Vec<u32> {
        std::mem::take(&mut self.slots)
    }

    pub(crate) fn push(&mut self, slot: u32) -> u32 {
        let index = u32::try_from(self.slots.len()).expect("argument count fits u32");
        self.slots.push(slot);
        index
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.slots.is_empty()
    }
}

impl Drop for Moves {
    fn drop(&mut self) {
        if self.slots.is_empty() {
            return;
        }
        if let Ok(mut dropped) = self.dropped.lock() {
            dropped.append(&mut self.slots);
        }
    }
}

/// The destination of one verified owner, chosen by the host rather than the guest packet.
pub(crate) enum Resource {
    /// Return an owner to the Store and exact environment that originally issued it.
    Issued { environment: u64, instance: u32 },
    /// A foreign owner installed in this revision's existing imported-resource table.
    Imported { slot: u32 },
}

/// One incoming ownership transaction. Its acknowledgement is sent only once the actual
/// canonical call has accepted the handles; before that the host rolls back installed slots.
pub(crate) struct Arguments {
    pub(crate) resources: Vec<Resource>,
    pub(crate) accepted: oneshot::Sender<()>,
}
