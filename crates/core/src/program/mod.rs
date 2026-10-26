//! Language-neutral lifecycle programs. Adapters execute effects; this module decides their order.
use crate::Reason;
use crate::{
    Code, Error, Host, Id, Migration, Owner, Resource, Result, Snapshot, Version,
    host::{conflict, gone},
};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Debug)]
pub struct Scope {
    pub id: Id,
    pub owner: Owner,
    pub facet: Id,
    pub parents: Vec<Id>,
    pub dependencies: Vec<Id>,
    pub resources: Vec<Id>,
    pub state_version: Option<Version>,
    pub migrate: bool,
    pub active: bool,
    pub closing: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Replacement {
    pub previous: Option<Id>,
    pub next: Option<Id>,
}

/// These are execution requests, not policy hooks. Every platform runs the same program.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Effect {
    Stop {
        scope: Id,
        keep_listeners: bool,
    },
    Open {
        resource: Id,
    },
    Close {
        resource: Id,
    },
    Restore {
        previous: Id,
        resource: Id,
    },
    Prepare {
        scope: Id,
        previous: Option<Id>,
    },
    Validate {
        scope: Id,
        value: Id,
    },
    Initialize {
        scope: Id,
    },
    Migrate {
        scope: Id,
        value: Option<Id>,
        version: Version,
    },
    Adopt {
        replacements: Vec<Replacement>,
    },
    Start {
        scope: Id,
    },
    Drop {
        scope: Id,
    },
}

#[derive(Clone, Debug)]
pub enum Step {
    Execute(Vec<Effect>),
    Wait { deadline: Option<u64> },
    Prepared,
    Complete,
    Failed(Error),
}

#[derive(Clone, Debug)]
pub struct Completion {
    pub value: Option<Id>,
    pub error: Option<Error>,
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Phase {
    Stop,
    Gate,
    CloseExclusive,
    Open,
    Prepare,
    State,
    Prepared,
    Adopt,
    Start,
    Drop,
    Complete,
    AbortStop,
    AbortClose,
    Restore,
    Resume,
    AbortDrop,
    Failed,
}

/// Only a change to one of these identities can unblock a waiting program.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub(crate) enum WaitKey {
    Owner(Owner),
    Scope(Id),
    Object(Id),
    Reference(Id),
    HostScopes,
    Resource(Id),
    Program(Id),
}

#[derive(Clone, Debug)]
pub(crate) struct Program {
    pub(crate) ticket: Option<Id>,
    pub(crate) replacements: Vec<Replacement>,
    previous_scopes: BTreeMap<Id, Option<Id>>,
    scope_set: BTreeSet<Id>,
    scopes: Vec<Id>,
    object: Option<Id>,
    pub(crate) activation_scope: Option<Id>,
    activation_owner: Option<Owner>,
    phase: Phase,
    pending: Vec<Effect>,
    migrations: Vec<Migration>,
    expected: BTreeMap<Id, Snapshot>,
    objects: Vec<Id>,
    facets: Vec<Id>,
    exclusive: Vec<Resource>,
    retained: BTreeSet<Id>,
    pub(crate) deadline: Option<u64>,
    error: Option<Error>,
    restored: BTreeMap<Id, Id>,
    committed: bool,
    running: bool,
    cancelled_effects: bool,
    pub(crate) waiting: BTreeSet<WaitKey>,
}

mod activation;
mod adoption;
mod cleanup;
mod recovery;
mod reload;
mod scheduler;
mod scopes;
mod state;

#[cfg(test)]
mod tests;

impl Program {
    /// Schedule replacement recovery when the host closes.
    pub(crate) fn close_reload(&mut self) -> bool {
        if self.ticket.is_none() {
            return false;
        }
        if !self.committed {
            self.error.get_or_insert_with(|| {
                Error::new(Code::Cancelled, Reason::ReloadAborted, "reload was aborted")
            });
        }
        self.running = true;
        true
    }

    /// Initialize one program with its scoped membership set.
    fn new(scopes: Vec<Id>, phase: Phase) -> Self {
        Self {
            ticket: None,
            replacements: Vec::new(),
            previous_scopes: BTreeMap::new(),
            scope_set: scopes.iter().copied().collect(),
            scopes,
            object: None,
            activation_scope: None,
            activation_owner: None,
            phase,
            pending: Vec::new(),
            migrations: Vec::new(),
            expected: BTreeMap::new(),
            objects: Vec::new(),
            facets: Vec::new(),
            exclusive: Vec::new(),
            retained: BTreeSet::new(),
            deadline: None,
            error: None,
            restored: BTreeMap::new(),
            committed: false,
            running: false,
            cancelled_effects: false,
            waiting: BTreeSet::new(),
        }
    }
}
