use super::*;

impl Connection {
    pub(in crate::rpc::connection) fn receive_reload_prepare(
        &self,
        transaction: PositiveId,
        epoch: PositiveId,
        facets: Vec<RemoteFacet>,
        remove: Vec<String>,
        timing: Timing,
        catchup: bool,
    ) -> Result<()> {
        if !matches!(self.profile(), ConnectionProfile::TrustedParent { .. }) {
            return Err(Self::forbidden());
        }
        crate::rpc::reload::validate_remote_batch(&facets, &remove)?;
        if transaction.get() <= self.inner.reload.received.get() {
            return Err(phase_error());
        }
        self.inner.reload.received.set(transaction.get());
        if self.inner.reload.inbound.borrow().is_some() {
            return self.send(Frame::ReloadVote {
                transaction,
                code_version: epoch,
                outcome: Vote::Rejected(
                    RuntimeError::unavailable(
                        "reload_busy",
                        "participant reload remains physically active",
                    )
                    .into(),
                ),
            });
        }
        let host = self.reload_host()?;
        if host
            .reload_children()
            .iter()
            .any(|connection| connection.status() != ConnectionStatus::Closed)
        {
            return self.send(Frame::ReloadVote {
                transaction,
                code_version: epoch,
                outcome: Vote::Rejected(
                    RuntimeError::invalid(
                        "reload_topology",
                        "nested participant reloads are unsupported",
                    )
                    .into(),
                ),
            });
        }
        let current = host.reload_current_version();
        if epoch.get() <= current || (!catchup && current.checked_add(1) != Some(epoch.get())) {
            return Err(RuntimeError::invalid(
                "reload_epoch",
                "parent prepare has an invalid target epoch",
            ));
        }
        let operation = match host.reload_administration().begin(None) {
            Ok(operation) => operation,
            Err(error) => {
                return self.send(Frame::ReloadVote {
                    transaction,
                    code_version: epoch,
                    outcome: Vote::Rejected(error.into()),
                });
            }
        };
        let inbound = Rc::new(Inbound {
            transaction,
            epoch,
            operation,
            prepared: RefCell::default(),
        });
        *self.inner.reload.inbound.borrow_mut() = Some(inbound.clone());
        let weak = Rc::downgrade(&self.inner);
        host.driver.clone().spawn(
            async move {
                let signal = inbound.operation.cancellation();
                let result = async {
                    let mut definitions = Vec::new();
                    for artifact in facets {
                        signal.check()?;
                        let loader = host.reload_administration().loader()?;
                        let future = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                            loader(artifact.clone())
                        }))
                        .map_err(|_| loader_panic())?;
                        let definition = std::panic::AssertUnwindSafe(future)
                            .catch_unwind()
                            .await
                            .map_err(|_| loader_panic())??;
                        if definition.kind() != FacetKind::Foreign
                            || definition.name() != artifact.name()
                        {
                            return Err(RuntimeError::invalid(
                                "reload_artifact",
                                "loader returned a different or native facet",
                            ));
                        }
                        definitions.push(definition);
                    }
                    signal.check()?;
                    host.prepare_reload_at(
                        definitions,
                        timing.options(remove, signal.clone()),
                        Some(epoch.get()),
                    )
                    .await
                }
                .await;
                match result {
                    Ok(mut prepared) => {
                        let check = signal.check().and_then(|()| prepared.preflight());
                        if let Err(error) = check {
                            let _ = prepared.abort(error).await;
                            clear_inbound(&weak, &inbound);
                            return;
                        }
                        let counts = prepared.summary_counts();
                        let summary = Summary {
                            prepared: SafeU64::new(counts.0 as u64).expect("bounded object count"),
                            gated: SafeU64::new(counts.1 as u64).expect("bounded object count"),
                        };
                        *inbound.prepared.borrow_mut() = Some(prepared);
                        if let Some(inner) = weak.upgrade() {
                            let connection = Connection { inner };
                            if let Err(error) = connection.send(Frame::ReloadVote {
                                transaction,
                                code_version: epoch,
                                outcome: Vote::Ready(summary),
                            }) {
                                connection.fail(error);
                            }
                        }
                    }
                    Err(error) => {
                        clear_inbound(&weak, &inbound);
                        if signal.check().is_ok()
                            && let Some(inner) = weak.upgrade()
                        {
                            let connection = Connection { inner };
                            if let Err(error) = connection.send(Frame::ReloadVote {
                                transaction,
                                code_version: epoch,
                                outcome: Vote::Rejected(error.into()),
                            }) {
                                connection.fail(error);
                            }
                        }
                    }
                }
            }
            .boxed_local(),
        );
        Ok(())
    }
    pub(in crate::rpc::connection) fn receive_reload_commit(
        &self,
        transaction: PositiveId,
        epoch: PositiveId,
    ) -> Result<()> {
        if !matches!(self.profile(), ConnectionProfile::TrustedParent { .. }) {
            return Err(Self::forbidden());
        }
        if self.inner.reload.ended.get() == Some((transaction.get(), epoch.get(), true)) {
            return self.send(Frame::ReloadAck {
                transaction,
                code_version: epoch,
            });
        }
        let inbound = self
            .inner
            .reload
            .inbound
            .borrow()
            .clone()
            .filter(|state| state.transaction == transaction && state.epoch == epoch)
            .ok_or_else(phase_error)?;
        let mut prepared = inbound
            .prepared
            .borrow_mut()
            .take()
            .ok_or_else(phase_error)?;
        // A parent decision is already irrevocable. Adoption happens in this dispatch,
        // before another application frame can enter the child.
        prepared.accept_parent_decision()?;
        let report = prepared.adopt()?;
        clear_inbound(&Rc::downgrade(&self.inner), &inbound);
        self.inner
            .reload
            .ended
            .set(Some((transaction.get(), epoch.get(), true)));
        self.reload_mark_synced(epoch);
        self.send(Frame::ReloadAck {
            transaction,
            code_version: epoch,
        })?;
        let host = self.reload_host()?;
        host.driver.clone().spawn(
            async move {
                let _inbound = inbound;
                if let Err(error) = prepared.finish(report).await {
                    host.report_reload_background(error);
                }
            }
            .boxed_local(),
        );
        Ok(())
    }
    pub(in crate::rpc::connection) fn receive_reload_abort(
        &self,
        transaction: PositiveId,
        error: RuntimeError,
    ) -> Result<()> {
        if !matches!(self.profile(), ConnectionProfile::TrustedParent { .. }) {
            return Err(Self::forbidden());
        }
        let inbound = self
            .inner
            .reload
            .inbound
            .borrow()
            .clone()
            .filter(|state| state.transaction == transaction);
        if let Some(inbound) = inbound {
            self.stop_inbound(inbound, error);
        }
        Ok(())
    }
    pub(super) fn stop_inbound(&self, inbound: Rc<Inbound>, error: RuntimeError) {
        inbound.operation.cancellation().cancel(error.clone());
        let prepared = inbound.prepared.borrow_mut().take();
        if let Some(prepared) = prepared {
            clear_inbound(&Rc::downgrade(&self.inner), &inbound);
            let driver = self.inner.driver.clone();
            driver.spawn(
                async move {
                    let _inbound = inbound;
                    let _ = prepared.abort(error).await;
                }
                .boxed_local(),
            );
        }
    }
    pub(in crate::rpc::connection) fn receive_reload_retire(
        &self,
        epoch: PositiveId,
    ) -> Result<()> {
        if !matches!(self.profile(), ConnectionProfile::TrustedParent { .. }) {
            return Err(Self::forbidden());
        }
        self.reload_host()?.grant_reload_retirement(epoch.get())
    }
}
