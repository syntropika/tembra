use super::*;

impl Connection {
    pub(crate) fn reload_prepare(
        &self,
        transaction: PositiveId,
        epoch: PositiveId,
        facets: Vec<RemoteFacet>,
        remove: Vec<String>,
        timing: Timing,
        catchup: bool,
    ) -> LocalBoxFuture<'static, Result<Summary>> {
        let admission = (|| {
            self.require_open()?;
            if !matches!(self.profile(), ConnectionProfile::AcceptedChild { .. }) {
                return Err(Self::forbidden());
            }
            if self.inner.reload.outbound.borrow().is_some() {
                return Err(RuntimeError::unavailable(
                    "reload_busy",
                    "peer reload already pending",
                ));
            }
            if self.inner.reload.outbound_ended.borrow().len() >= 64 {
                return Err(RuntimeError::unavailable(
                    "reload_history",
                    "peer has too many unacknowledged aborted reloads; reconnect to reset history",
                ));
            }
            let (vote, receive) = oneshot::channel();
            let (acknowledge, observer) = oneshot::channel();
            *self.inner.reload.outbound.borrow_mut() = Some(Outbound {
                transaction,
                epoch,
                ready: false,
                committed: false,
                vote: Some(vote),
                acknowledge: Some(acknowledge),
                observer: Some(observer),
            });
            if let Err(error) = self.send(Frame::ReloadPrepare {
                transaction,
                code_version: epoch,
                facets,
                remove,
                timing,
                catchup,
            }) {
                let removed = self.inner.reload.outbound.borrow_mut().take();
                drop(removed);
                return Err(error);
            }
            Ok(receive)
        })();
        async move {
            admission?.await.unwrap_or_else(|_| {
                Err(RuntimeError::unavailable(
                    "reload_peer_closed",
                    "peer ended before its vote",
                ))
            })
        }
        .boxed_local()
    }
    pub(crate) fn reload_prepare_commit(
        &self,
        transaction: PositiveId,
        epoch: PositiveId,
    ) -> Result<Rc<[u8]>> {
        self.require_open()?;
        let state = self.inner.reload.outbound.borrow();
        if !state.as_ref().is_some_and(|state| {
            state.transaction == transaction
                && state.epoch == epoch
                && state.ready
                && !state.committed
        }) {
            return Err(RuntimeError::invalid(
                "reload_phase",
                "peer has no matching ready vote",
            ));
        }
        drop(state);
        let bytes = encode(
            &Frame::ReloadCommit {
                transaction,
                code_version: epoch,
            },
            self.inner.peer_frame.get(),
        )?;
        if !self.output_fits(bytes.len(), 1, false) {
            return Err(RuntimeError::unavailable(
                "output_limit",
                "commit control cannot fit bounded FIFO",
            ));
        }
        Ok(bytes)
    }
    pub(crate) fn reload_append_commit(
        &self,
        transaction: PositiveId,
        epoch: PositiveId,
        bytes: Rc<[u8]>,
    ) -> LocalBoxFuture<'static, Result<()>> {
        let observer = {
            let mut state = self.inner.reload.outbound.borrow_mut();
            let state = state
                .as_mut()
                .expect("commit was synchronously preflighted");
            assert!(
                state.transaction == transaction
                    && state.epoch == epoch
                    && state.ready
                    && !state.committed
            );
            state.committed = true;
            state.observer.take().expect("one commit observer")
        };
        self.append_output(vec![bytes]);
        async move {
            observer.await.unwrap_or_else(|_| {
                Err(RuntimeError::unavailable(
                    "reload_unknown_outcome",
                    "peer ended after commit admission",
                ))
            })
        }
        .boxed_local()
    }
    pub(crate) fn reload_retire(&self, epoch: PositiveId) -> Result<()> {
        if !matches!(self.profile(), ConnectionProfile::AcceptedChild { .. }) {
            return Err(Self::forbidden());
        }
        self.send(Frame::ReloadRetire {
            code_version: epoch,
        })
    }
    pub(crate) fn reload_abort(&self, transaction: PositiveId, error: RuntimeError) {
        let removed = {
            let mut state = self.inner.reload.outbound.borrow_mut();
            if state
                .as_ref()
                .is_some_and(|state| state.transaction == transaction && !state.committed)
            {
                state.take()
            } else {
                None
            }
        };
        if let Some(state) = &removed {
            self.inner.reload.remember_outbound(state);
        }
        drop(removed);
        if let Err(error) = self.send(Frame::ReloadAbort {
            transaction,
            error: error.into(),
        }) {
            self.fail(error);
        }
    }
    pub(in crate::rpc::connection) fn receive_reload_vote(
        &self,
        transaction: PositiveId,
        epoch: PositiveId,
        vote: Vote,
    ) -> Result<()> {
        if !matches!(self.profile(), ConnectionProfile::AcceptedChild { .. }) {
            return Err(Self::forbidden());
        }
        if self.inner.reload.ended_outbound(transaction, epoch, false) {
            self.inner.reload.acknowledged_outbound(transaction);
            return Ok(());
        }
        let sender = {
            let mut state = self.inner.reload.outbound.borrow_mut();
            let state = state
                .as_mut()
                .filter(|state| {
                    state.transaction == transaction && state.epoch == epoch && !state.committed
                })
                .ok_or_else(phase_error)?;
            state.ready = matches!(vote, Vote::Ready(_));
            state.vote.take().ok_or_else(phase_error)?
        };
        self.inner.reload.acknowledged_outbound(transaction);
        let result = match vote {
            Vote::Ready(summary) => Ok(summary),
            Vote::Rejected(error) => Err(error.into()),
        };
        let _ = sender.send(result);
        Ok(())
    }
    pub(in crate::rpc::connection) fn receive_reload_ack(
        &self,
        transaction: PositiveId,
        epoch: PositiveId,
    ) -> Result<()> {
        if !matches!(self.profile(), ConnectionProfile::AcceptedChild { .. }) {
            return Err(Self::forbidden());
        }
        if self.inner.reload.ended_outbound(transaction, epoch, true) {
            return Ok(());
        }
        let removed = {
            let mut state = self.inner.reload.outbound.borrow_mut();
            if !state.as_ref().is_some_and(|state| {
                state.transaction == transaction && state.epoch == epoch && state.committed
            }) {
                return Err(phase_error());
            }
            state.take().expect("checked state")
        };
        self.inner.reload.acknowledged_outbound(transaction);
        self.inner.reload.remember_outbound(&removed);
        self.reload_mark_synced(epoch);
        if let Some(sender) = removed.acknowledge {
            let _ = sender.send(Ok(()));
        }
        Ok(())
    }
}
