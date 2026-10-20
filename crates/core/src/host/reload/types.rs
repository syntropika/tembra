//! Concrete installed ownership and exact-pass replay targets shared with candidate domains.
use super::super::{Address, Service};
use crate::{
    Implementation, Reference,
    activity::ActivityDeclarations,
    epoch::{CodeEpoch, EpochRecord},
    object::ObjectRecord,
    registry::RegistrationDeclaration,
    resource::{ResourceReload, ResourceSet},
    state::{GraphDeclarationSet, GraphReload},
};
use std::{
    cell::RefCell,
    collections::HashMap,
    rc::{Rc, Weak},
};

pub(crate) struct DeclarationSet {
    pub(crate) dependencies: HashMap<Address, Reference>,
    pub(crate) resources: ResourceSet,
    pub(crate) state: GraphDeclarationSet,
    pub(crate) activities: Rc<ActivityDeclarations>,
    pub(crate) activity_runs: RefCell<Option<Rc<crate::activity::ActivityRunSet>>>,
    pub(crate) registrations: Vec<RegistrationDeclaration>,
}
pub(crate) struct InstalledVersion {
    pub(crate) epoch: Rc<EpochRecord>,
    pub(crate) service: Rc<Service>,
    pub(crate) implementation: Rc<Implementation>,
    pub(crate) declarations: Rc<DeclarationSet>,
    pub(crate) adapters: RefCell<HashMap<String, Rc<Implementation>>>,
}
#[derive(Clone)]
pub(in crate::host) struct ReadyBindings {
    pub(in crate::host) version: u64,
    pub(in crate::host) views: HashMap<(u64, String), Rc<Implementation>>,
    pub(in crate::host) blocked: std::collections::HashSet<u64>,
    pub(in crate::host) affected: std::collections::HashSet<u64>,
}
impl InstalledVersion {
    pub(crate) fn version(&self) -> CodeEpoch {
        self.epoch.version
    }
}
pub(crate) struct CandidateDeclarations {
    pub(crate) owner: Weak<ObjectRecord>,
    pub(crate) catalog: Rc<super::CandidateCatalog>,
    pub(crate) service: Rc<Service>,
    pub(crate) graph: Rc<GraphReload>,
    pub(crate) resources: Rc<ResourceReload>,
    pub(crate) activities: Rc<ActivityDeclarations>,
    pub(crate) dependencies: RefCell<HashMap<Address, Reference>>,
    pub(crate) registrations: RefCell<Vec<RegistrationDeclaration>>,
}
#[derive(Clone)]
pub(crate) enum DeclarationTarget {
    Activation,
    Reload(Weak<CandidateDeclarations>),
}
