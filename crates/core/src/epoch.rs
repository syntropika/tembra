//! One Host's callable code-version authority, separate from physical ownership.
use crate::{CancellationToken, ErrorCode, Result, RuntimeError};
use std::{
    cell::{Cell, RefCell},
    rc::Rc,
};

const MAX_EPOCH: u64 = 9_007_199_254_740_991;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct CodeEpoch(u64);
impl CodeEpoch {
    pub(crate) fn new(value: u64) -> Result<Self> {
        if value == 0 || value > MAX_EPOCH {
            return Err(RuntimeError::invalid(
                "code_version",
                "code version is not a positive safe integer",
            ));
        }
        Ok(Self(value))
    }
    pub(crate) fn get(self) -> u64 {
        self.0
    }
}

pub(crate) struct EpochRecord {
    pub(crate) version: CodeEpoch,
    pub(crate) admitted: Cell<bool>,
    pub(crate) cancellation: CancellationToken,
}
impl EpochRecord {
    fn new(version: CodeEpoch, admitted: bool) -> Rc<Self> {
        Rc::new(Self {
            version,
            admitted: Cell::new(admitted),
            cancellation: CancellationToken::new(),
        })
    }
}
struct State {
    current: Rc<EpochRecord>,
    previous: Option<Rc<EpochRecord>>,
    pending: Option<Rc<EpochRecord>>,
    slot: bool,
    failure: Option<RuntimeError>,
}
pub(crate) struct EpochAuthority {
    state: RefCell<State>,
}
pub(crate) struct EpochTransition {
    pub(crate) current: Rc<EpochRecord>,
    pub(crate) previous: Rc<EpochRecord>,
}
pub(crate) struct RetiredEpoch {
    pub(crate) record: Rc<EpochRecord>,
}
impl EpochAuthority {
    pub(crate) fn new() -> Rc<Self> {
        Rc::new(Self {
            state: RefCell::new(State {
                current: EpochRecord::new(CodeEpoch(1), true),
                previous: None,
                pending: None,
                slot: false,
                failure: None,
            }),
        })
    }
    pub(crate) fn check(&self) -> Result<()> {
        self.state.borrow().failure.clone().map_or(Ok(()), Err)
    }
    pub(crate) fn current(&self) -> Rc<EpochRecord> {
        self.state.borrow().current.clone()
    }
    pub(crate) fn previous(&self) -> Option<Rc<EpochRecord>> {
        self.state.borrow().previous.clone()
    }
    pub(crate) fn exact(&self, version: u64) -> Result<Rc<EpochRecord>> {
        self.check()?;
        let state = self.state.borrow();
        std::iter::once(&state.current)
            .chain(state.previous.iter())
            .find(|record| record.version.get() == version && record.admitted.get())
            .cloned()
            .ok_or_else(superseded)
    }
    pub(crate) fn reserve_slot(&self) -> Result<()> {
        self.check()?;
        let mut state = self.state.borrow_mut();
        if state.slot {
            return Err(RuntimeError::unavailable(
                "reload_busy",
                "a reload administration slot is already owned",
            ));
        }
        state.slot = true;
        Ok(())
    }
    pub(crate) fn release_slot(&self) {
        self.state.borrow_mut().slot = false;
    }
    pub(crate) fn reserve_target(&self, target: Option<u64>) -> Result<Rc<EpochRecord>> {
        self.check()?;
        let mut state = self.state.borrow_mut();
        if !state.slot || state.pending.is_some() || state.previous.is_some() {
            return Err(RuntimeError::unavailable(
                "reload_state",
                "pending epoch requires an owned slot and retired previous epoch",
            ));
        }
        let current = state.current.version.get();
        let target = CodeEpoch::new(match target {
            Some(target) if target > current => target,
            Some(_) => {
                return Err(RuntimeError::invalid(
                    "code_version",
                    "trusted target must advance current code",
                ));
            }
            None => current.checked_add(1).ok_or_else(superseded)?,
        })?;
        let next = EpochRecord::new(target, false);
        state.pending = Some(next.clone());
        Ok(next)
    }
    pub(crate) fn preflight_next(&self, expected: &Rc<EpochRecord>) -> Result<()> {
        self.check()?;
        if self
            .state
            .borrow()
            .pending
            .as_ref()
            .is_some_and(|pending| Rc::ptr_eq(pending, expected))
        {
            Ok(())
        } else {
            Err(RuntimeError::invalid(
                "reload_candidate",
                "pending code version changed",
            ))
        }
    }
    pub(crate) fn abort_pending(&self, expected: &Rc<EpochRecord>) -> Result<()> {
        self.preflight_next(expected)?;
        let retired = self.state.borrow_mut().pending.take();
        drop(retired);
        Ok(())
    }
    pub(crate) fn adopt_next(&self, expected: &Rc<EpochRecord>) -> EpochTransition {
        let mut state = self.state.borrow_mut();
        let next = state
            .pending
            .take()
            .expect("pending version preflighted before adoption");
        assert!(Rc::ptr_eq(&next, expected));
        next.admitted.set(true);
        let previous = std::mem::replace(&mut state.current, next.clone());
        state.previous = Some(previous.clone());
        EpochTransition {
            current: next,
            previous,
        }
    }
    pub(crate) fn retire(&self, expected: &Rc<EpochRecord>) -> Result<RetiredEpoch> {
        let mut state = self.state.borrow_mut();
        if !state
            .previous
            .as_ref()
            .is_some_and(|previous| Rc::ptr_eq(previous, expected))
        {
            return Err(superseded());
        }
        let record = state.previous.take().expect("checked previous");
        record.admitted.set(false);
        Ok(RetiredEpoch { record })
    }
    pub(crate) fn fail(&self, cause: RuntimeError) {
        let mut state = self.state.borrow_mut();
        if state.failure.is_none() {
            state.failure = Some(cause);
        }
    }
}
pub(crate) fn superseded() -> RuntimeError {
    RuntimeError::new(
        ErrorCode::Superseded,
        "code_version",
        "code version is not callable",
    )
}
