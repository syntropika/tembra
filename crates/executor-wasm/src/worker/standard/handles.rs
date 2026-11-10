//! Issue non-reusing revision-owned handles before exposing native resources.
use super::{
    HANDLE_LIMIT, Shared, invalid,
    streams::{Feed, Inbox, InputBudget, Pending, Pipe, PipeConsumer},
};
use crate::abi::IDENTITY_LIMIT;
use std::{
    any::Any,
    collections::HashMap,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
    },
};
use wasmtime::component::Resource;

pub(super) enum Entry {
    /// An upstream resource; `lent` counts pending Store tasks that captured a borrow of it.
    Resource {
        kind: u32,
        rep: u32,
        lent: Arc<AtomicUsize>,
        /// This resource's share of the engine's open-file budget, held for its drop.
        _file: Option<FilePermit>,
    },
    /// An output stream (bytes or encoded items) read by the guest.
    Stream(Shared<Pipe>),
    Future(Shared<Pending>),
    /// A guest-writable input byte stream; its readable end transfers once to upstream.
    Writer(Shared<Inbox>),
    /// A stream or future the guest no longer names whose upstream producer or reader may still
    /// hold host transmit state. It keeps counting toward the limit until only this table holds
    /// it, so synchronous create/drop loops cannot grow host state before the next Store turn.
    Detached(Arc<dyn Any + Send + Sync>),
}

impl Entry {
    /// A detached entry whose producer and readers have all released their state.
    fn finished(&self) -> bool {
        matches!(self, Entry::Detached(state) if Arc::strong_count(state) == 1)
    }
}

/// One pending task's borrow of a resource entry; released exactly once.
pub(in crate::worker) struct Lease {
    lent: Arc<AtomicUsize>,
    released: AtomicBool,
}

impl Lease {
    /// End this borrow when its task settles, is dropped, or is aborted by its callback owner.
    /// An aborted Store task is never polled again, so the upstream borrow cannot be used.
    pub(super) fn release(&self) {
        if !self.released.swap(true, Ordering::AcqRel) {
            self.lent.fetch_sub(1, Ordering::AcqRel);
        }
    }
}

impl Drop for Lease {
    fn drop(&mut self) {
        self.release();
    }
}

/// Task-held guard of a [`Lease`]; dropping the task's future ends the borrow.
pub(super) struct Lend(Arc<Lease>);

impl Lend {
    /// Share the lease with the callback owner so revocation releases it synchronously.
    pub(super) fn lease(&self) -> Arc<Lease> {
        Arc::clone(&self.0)
    }
}

impl Drop for Lend {
    fn drop(&mut self) {
        self.0.release();
    }
}

/// Take the next nonzero identity below the JavaScript-safe limit; exhaustion never wraps.
pub(super) fn next_identity(counter: &AtomicU64) -> wasmtime::Result<u64> {
    counter
        .try_update(Ordering::Relaxed, Ordering::Relaxed, |id| {
            id.checked_add(1).filter(|next| *next <= IDENTITY_LIMIT)
        })
        .map(|id| id + 1)
        .map_err(|_| invalid("standard handle identity space exhausted"))
}

/// One unit of the open-file budget: a unit of this revision's private share and a unit of the
/// process-wide pool, released together when the descriptor or stream object drops.
pub(super) struct FilePermit {
    _revision: tokio::sync::OwnedSemaphorePermit,
    _process: tokio::sync::OwnedSemaphorePermit,
}

/// Process-wide non-reusing identities prevent foreign and retired handles from aliasing.
/// The revision table alone owns the corresponding upstream resources.
pub(super) struct Handles {
    pub(super) entries: HashMap<u64, Entry>,
    /// The process-wide open-file pool shared with every other source Orb.
    pub(super) files: Arc<tokio::sync::Semaphore>,
    /// This revision's private share of `files`, so one Orb cannot exhaust the pool for others.
    pub(super) share: Arc<tokio::sync::Semaphore>,
    /// Accepted input bytes across this revision's writers.
    pub(super) input: Shared<InputBudget>,
    /// Permits an async call reserved at dispatch, before I/O, for the resources its reply owns.
    pub(super) prepaid: Vec<FilePermit>,
    /// Handles issued by the reply currently being encoded; published or rolled back as a unit.
    pub(super) issued: Vec<u64>,
    /// Owned upstream resources that could not be issued; released on rollback.
    pub(super) orphans: Vec<(u32, u32)>,
    /// First refusal while encoding the current reply.
    pub(super) refusal: Option<String>,
}

impl Handles {
    /// Start an empty table for one revision.
    #[cfg(test)]
    pub(super) fn new() -> Self {
        Self::with_files(crate::engine::FileBudget::new(crate::engine::OPEN_FILES))
    }

    /// Start an empty table drawing from a shared open-file budget.
    pub(super) fn with_files(budget: crate::engine::FileBudget) -> Self {
        Self {
            share: Arc::new(tokio::sync::Semaphore::new(budget.share)),
            files: budget.process,
            prepaid: Vec::new(),
            input: Shared::default(),
            entries: HashMap::new(),
            issued: Vec::new(),
            orphans: Vec::new(),
            refusal: None,
        }
    }

    /// Draw from another pool, e.g. one shared by two test revisions.
    #[cfg(test)]
    pub(super) fn use_budget(&mut self, budget: &crate::engine::FileBudget) {
        self.files = Arc::clone(&budget.process);
        self.share = Arc::new(tokio::sync::Semaphore::new(budget.share));
    }

    /// Refuse unless `count` more entries fit, after purging finished detached state.
    pub(super) fn reserve(&mut self, count: usize) -> wasmtime::Result<()> {
        if self.entries.len() + count > HANDLE_LIMIT {
            self.entries.retain(|_, entry| !entry.finished());
        }
        if self.entries.len() + count > HANDLE_LIMIT {
            return Err(invalid("standard handle table is full"));
        }
        Ok(())
    }

    /// Forget detached state whose producers and readers are gone; `true` while some remains.
    pub(super) fn purge(&mut self) -> bool {
        self.entries.retain(|_, entry| !entry.finished());
        self.entries
            .values()
            .any(|entry| matches!(entry, Entry::Detached(_)))
    }

    /// Remaining settlement work: detached entries upstream still holds plus accepted input
    /// bytes it has not consumed; `None` once no detached state remains.
    pub(super) fn unsettled(&mut self) -> Option<usize> {
        self.entries.retain(|_, entry| !entry.finished());
        let mut remaining = None;
        for entry in self.entries.values() {
            if let Entry::Detached(state) = entry {
                let bytes = state
                    .downcast_ref::<std::sync::Mutex<Inbox>>()
                    .map_or(0, |inbox| inbox.lock().expect("inbox").buffered());
                *remaining.get_or_insert(0) += 1 + bytes;
            }
        }
        remaining
    }

    /// Issue a globally fresh identity and journal it for the reply being encoded.
    pub(super) fn issue(&mut self, entry: Entry) -> wasmtime::Result<u64> {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        self.reserve(1)?;
        let id = next_identity(&NEXT)?;
        self.entries.insert(id, entry);
        self.issued.push(id);
        Ok(id)
    }

    /// Record an owned upstream resource for this revision. A refusal never drops the resource
    /// silently: it is kept as an orphan for rollback and the whole reply is refused.
    pub(super) fn own<T: 'static>(&mut self, kind: u32, resource: Resource<T>) -> u64 {
        let issued = if resource.owned() {
            // Only filesystem resources are open files; terminals and others draw nothing.
            let file = if super::BUDGETED.get(kind as usize).copied().unwrap_or(true) {
                self.prepaid.pop().map_or_else(|| self.file(), Ok).map(Some)
            } else {
                Ok(None)
            };
            file.and_then(|file| {
                self.issue(Entry::Resource {
                    kind,
                    rep: resource.rep(),
                    lent: Arc::default(),
                    _file: file,
                })
            })
        } else {
            Err(invalid("upstream returned a borrowed resource"))
        };
        issued.unwrap_or_else(|error| {
            if resource.owned() {
                self.orphans.push((kind, resource.rep()));
            }
            self.refusal.get_or_insert_with(|| error.to_string());
            0
        })
    }

    /// Build a borrow only for a live handle of the expected resource type.
    pub(super) fn borrow<T: 'static>(&self, kind: u32, id: u64) -> wasmtime::Result<Resource<T>> {
        match self.entries.get(&id) {
            Some(Entry::Resource {
                kind: actual, rep, ..
            }) if *actual == kind => Ok(Resource::new_borrow(*rep)),
            Some(
                Entry::Resource { .. } | Entry::Stream(_) | Entry::Future(_) | Entry::Writer(_),
            ) => Err(invalid("handle has a different type")),
            Some(Entry::Detached(_)) | None => Err(invalid("unknown or dropped handle")),
        }
    }

    /// Borrow a live handle for a Store task; the entry cannot be dropped until the lend ends.
    pub(super) fn lend<T: 'static>(
        &self,
        kind: u32,
        id: u64,
    ) -> wasmtime::Result<(Resource<T>, Lend)> {
        let resource = self.borrow(kind, id)?;
        let Some(Entry::Resource { lent, .. }) = self.entries.get(&id) else {
            unreachable!("checked by borrow")
        };
        lent.fetch_add(1, Ordering::AcqRel);
        let lease = Lease {
            lent: Arc::clone(lent),
            released: AtomicBool::new(false),
        };
        Ok((resource, Lend(Arc::new(lease))))
    }

    /// Remove a resource entry the guest may drop or transfer; refused while it is lent.
    pub(super) fn remove(&mut self, id: u64) -> wasmtime::Result<Entry> {
        if let Some(Entry::Resource { lent, .. }) = self.entries.get(&id)
            && lent.load(Ordering::Acquire) != 0
        {
            return Err(invalid("handle is borrowed by a pending call"));
        }
        match self.entries.remove(&id) {
            Some(Entry::Detached(state)) => {
                self.entries.insert(id, Entry::Detached(state));
                Err(invalid("unknown or dropped handle"))
            }
            Some(entry) => Ok(entry),
            None => Err(invalid("unknown or dropped handle")),
        }
    }

    /// Keep a dropped or consumed stream/future counted until its host state is released.
    pub(super) fn detach(&mut self, id: u64, state: Arc<dyn Any + Send + Sync>) {
        self.entries.insert(id, Entry::Detached(state));
    }

    /// Transfer ownership of a live handle back to upstream.
    #[allow(dead_code)]
    pub(super) fn take<T: 'static>(&mut self, kind: u32, id: u64) -> wasmtime::Result<Resource<T>> {
        self.borrow::<T>(kind, id)?;
        match self.remove(id)? {
            Entry::Resource { rep, .. } => Ok(Resource::new_own(rep)),
            _ => unreachable!("checked above"),
        }
    }

    /// Take one unit of the open-file budget, refusing (never waiting) when either this
    /// revision's share or the process pool is exhausted; a refusal holds nothing.
    fn file(&self) -> wasmtime::Result<FilePermit> {
        let revision = Arc::clone(&self.share)
            .try_acquire_owned()
            .map_err(|_| invalid("this Orb's share of the host open-file budget is exhausted"))?;
        let process = Arc::clone(&self.files)
            .try_acquire_owned()
            .map_err(|_| invalid("host open-file budget is exhausted"))?;
        Ok(FilePermit {
            _revision: revision,
            _process: process,
        })
    }

    /// Budget for upstream stream objects of one Store-method call, taken before upstream
    /// creates them; each permit then lives exactly as long as its stream object.
    pub(super) fn files(&self, count: usize) -> wasmtime::Result<Vec<FilePermit>> {
        (0..count).map(|_| self.file()).collect()
    }

    /// Reserve a byte stream handle whose producer binds later.
    pub(super) fn stream(
        &mut self,
        file: Option<FilePermit>,
    ) -> wasmtime::Result<(u64, PipeConsumer)> {
        let pipe = Shared::default();
        let id = self.issue(Entry::Stream(Arc::clone(&pipe)))?;
        Ok((id, PipeConsumer(pipe, file)))
    }

    /// Reserve a typed output stream handle whose upstream producer binds later.
    pub(super) fn items(&mut self) -> wasmtime::Result<(u64, Shared<Pipe>)> {
        let pipe = Shared::default();
        let id = self.issue(Entry::Stream(Arc::clone(&pipe)))?;
        Ok((id, pipe))
    }

    /// Create a guest-writable input byte stream.
    pub(super) fn writer(&mut self) -> wasmtime::Result<u64> {
        let inbox = Inbox::new(Arc::clone(&self.input));
        self.issue(Entry::Writer(Arc::new(std::sync::Mutex::new(inbox))))
    }

    /// Close every writer still open: input streams are owned by the callback that pumps them.
    /// Transferred streams deliver their accepted bytes and end; the ids stop being names.
    pub(super) fn close_writers(&mut self) {
        for entry in self.entries.values_mut() {
            if let Entry::Writer(inbox) = entry {
                inbox.lock().expect("inbox").close();
                let state: Arc<dyn Any + Send + Sync> = Arc::clone(inbox) as _;
                *entry = Entry::Detached(state);
            }
        }
    }

    /// Validate an input stream parameter without transferring it yet.
    pub(super) fn claim(&self, id: u64) -> wasmtime::Result<Claim> {
        match self.entries.get(&id) {
            Some(Entry::Writer(inbox)) if !inbox.lock().expect("inbox").transferred => {
                Ok(Claim(Arc::clone(inbox)))
            }
            Some(Entry::Writer(_)) => Err(invalid("stream was already transferred")),
            Some(Entry::Detached(_)) | None => Err(invalid("unknown or dropped handle")),
            Some(_) => Err(invalid("handle has a different type")),
        }
    }

    /// Reserve a future handle whose producer binds later.
    pub(super) fn future(&mut self) -> wasmtime::Result<(u64, Shared<Pending>)> {
        let pending = Shared::default();
        let id = self.issue(Entry::Future(Arc::clone(&pending)))?;
        Ok((id, pending))
    }
}

/// A validated, not yet transferred input stream parameter.
pub(super) struct Claim(Shared<Inbox>);

impl Claim {
    /// Hand the readable end to upstream; called only after the whole packet validated.
    pub(super) fn transfer(self, file: Option<FilePermit>) -> wasmtime::Result<Feed> {
        let mut inbox = self.0.lock().expect("inbox");
        if inbox.transferred {
            return Err(invalid("stream was already transferred"));
        }
        inbox.transferred = true;
        drop(inbox);
        Ok(Feed(self.0, file))
    }
}
