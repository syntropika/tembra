use super::*;
use crate::{
    ExecutableFacet, Host, ReloadOptions,
    host::HostInner,
    rpc::{Connection, ConnectionStatus, protocol::PositiveId},
};
use futures::{channel::oneshot, future::join_all};
use std::time::Duration;

impl Host {
    /// Sets the trusted participant loader without fetching or evaluating an artifact.
    ///
    /// # Errors
    /// Refuses closed administration or a running coordinated operation. Replacing this
    /// callback does not change already installed facets or remembered deployment metadata.
    pub fn set_reload_loader(&self, loader: ReloadFacetLoader) -> Result<()> {
        let administration = self.inner.reload_administration();
        administration.stopped.check()?;
        if administration.busy.get()
            || administration
                .active
                .borrow()
                .iter()
                .any(|entry| entry.strong_count() > 0)
        {
            return Err(RuntimeError::unavailable(
                "reload_busy",
                "reload loading is active",
            ));
        }
        let prior = administration.loader.replace(Some(loader));
        drop(prior);
        Ok(())
    }

    /// Submits one Driver-owned transaction to this parent and its direct synced children.
    ///
    /// Artifact identifiers are resolved by each child's configured loader. Browser clients
    /// never participate. Dropping the observer does not cancel admitted administration.
    /// A failed vote aborts preparation; after the commit decision a lost link can leave an
    /// unknown remote outcome. This is not durable consensus or rollback of external effects.
    ///
    /// # Errors
    /// Refuses child-side coordination, overlapping administration, invalid bounded metadata,
    /// failed preparation, missing/rejected votes and failed commit acknowledgement. A returned
    /// error after the decision does not imply the local installation was rolled back.
    /// `reload_unknown_outcome` details identify each participant as acknowledged or unknown;
    /// absence of an acknowledgement does not establish whether that child installed the code.
    ///
    /// # Examples
    /// An empty deployment advances the local epoch without inventing participants.
    ///
    /// ```
    /// use serde_json::json;
    /// use tembra_core::{Host, HostOptions, ManualDriver, ReloadOptions, RuntimeError};
    ///
    /// let driver = ManualDriver::new();
    /// let host = Host::new(HostOptions::new("example", json!(null)), driver.clone());
    /// driver.run_until(async {
    ///     let report = host.reload_cluster(vec![], vec![], ReloadOptions::default()).await?;
    ///     assert_eq!(report.local().current_version(), 2);
    ///     assert!(report.children().is_empty());
    ///     host.close().await?;
    ///     Ok::<_, RuntimeError>(())
    /// }).unwrap();
    /// ```
    pub fn reload_cluster(
        &self,
        definitions: Vec<ExecutableFacet>,
        remote: Vec<RemoteFacet>,
        mut options: ReloadOptions,
    ) -> LocalBoxFuture<'static, Result<ClusterReloadReport>> {
        let host = self.inner.clone();
        let administration = host.reload_administration();
        let admission = (|| {
            if host.has_reload_parent() {
                return Err(RuntimeError::new(
                    crate::ErrorCode::Forbidden,
                    "reload_parent",
                    "a child cannot coordinate reload",
                ));
            }
            types::validate_batch(&remote, &options.remove)?;
            administration.check_remembered(&remote, &options.remove)?;
            let timing = Timing::from_options(&options)?;
            let vote_deadline = phase_timeout(&host, &options)?;
            let slot = administration.slot()?;
            let operation = administration.begin(options.cancellation.as_ref())?;
            let transaction = administration.next_transaction()?;
            Ok((timing, vote_deadline, slot, operation, transaction))
        })();
        let (timing, deadline, slot, operation, transaction) = match admission {
            Ok(admission) => admission,
            Err(error) => return async move { Err(error) }.boxed_local(),
        };
        let signal = operation.cancellation();
        options.cancellation = Some(signal.clone());
        let participants = host
            .reload_children()
            .into_iter()
            .filter(Connection::reload_synced)
            .collect::<Vec<_>>();
        let removed = options.remove.clone();
        // Core admission is synchronous even though candidate preparation is driven later.
        let local = host.prepare_reload_at(definitions, options, None);
        let (send, receive) = oneshot::channel();
        host.driver.clone().spawn(
            async move {
                let _slot = slot;
                let _operation = operation;
                let result = coordinate(
                    &host,
                    &administration,
                    transaction,
                    remote,
                    removed,
                    timing,
                    deadline,
                    signal,
                    participants,
                    local,
                )
                .await;
                let _ = send.send(result);
            }
            .boxed_local(),
        );
        async move {
            receive.await.unwrap_or_else(|_| {
                Err(RuntimeError::unavailable(
                    "reload_abandoned",
                    "coordinated reload owner ended",
                ))
            })
        }
        .boxed_local()
    }
}

#[allow(
    clippy::too_many_arguments,
    reason = "one explicit owned transaction boundary"
)]
async fn coordinate(
    host: &Rc<HostInner>,
    administration: &Rc<Administration>,
    transaction: PositiveId,
    remote: Vec<RemoteFacet>,
    removed: Vec<String>,
    timing: Timing,
    deadline: Duration,
    signal: CancellationToken,
    participants: Vec<Connection>,
    local: LocalBoxFuture<'static, Result<crate::host::reload::PreparedReload>>,
) -> Result<ClusterReloadReport> {
    let target = PositiveId::new(
        host.reload_current_version()
            .checked_add(1)
            .ok_or_else(|| RuntimeError::unavailable("code_version", "code epochs exhausted"))?,
    )?;
    // Complete parent preparation first: its existing previous epoch must actually
    // retire before children receive permission to retire that same epoch.
    let mut prepared = local.await?;
    let mut votes = Vec::new();
    for connection in &participants {
        let permission = host
            .reload_current_version()
            .checked_sub(1)
            .filter(|version| *version > 0);
        let retirement = permission.map_or(Ok(()), |version| {
            connection.reload_retire(PositiveId::new(version)?)
        });
        let wait = match retirement {
            Ok(()) => connection.reload_prepare(
                transaction,
                target,
                remote.clone(),
                removed.clone(),
                timing.clone(),
                false,
            ),
            Err(error) => async move { Err(error) }.boxed_local(),
        };
        let signal = signal.clone();
        let peers = participants.clone();
        let driver = host.driver.clone();
        votes.push(
            async move {
                let result = await_phase(wait, driver, deadline, &signal).await;
                if let Err(error) = &result {
                    signal.cancel(error.clone());
                    abort_all(&peers, transaction, error);
                }
                result
            }
            .boxed_local(),
        );
    }
    let votes = join_all(votes).await;
    if let Some(error) = votes
        .iter()
        .find_map(|result| result.as_ref().err())
        .cloned()
    {
        abort_all(&participants, transaction, &error);
        prepared.abort(error.clone()).await?;
        return Err(error);
    }
    let preflight = (|| {
        signal.check()?;
        if prepared.code_version() != target.get() {
            return Err(RuntimeError::invalid(
                "reload_epoch",
                "prepared parent epoch changed",
            ));
        }
        for connection in &participants {
            if connection.status() != ConnectionStatus::Open || !connection.reload_synced() {
                return Err(RuntimeError::unavailable(
                    "reload_member",
                    "participant changed before decision",
                ));
            }
        }
        prepared.preflight()?;
        participants
            .iter()
            .map(|connection| connection.reload_prepare_commit(transaction, target))
            .collect::<Result<Vec<_>>>()
    })();
    let controls = match preflight {
        Ok(controls) => controls,
        Err(error) => {
            abort_all(&participants, transaction, &error);
            prepared.abort(error.clone()).await?;
            return Err(error);
        }
    };
    if let Err(error) = prepared.decide() {
        abort_all(&participants, transaction, &error);
        prepared.abort(error.clone()).await?;
        return Err(error);
    }
    // No await, callback or flush between capacity preflight and all FIFO appends.
    let acknowledgements = participants
        .iter()
        .zip(controls)
        .map(|(connection, bytes)| connection.reload_append_commit(transaction, target, bytes))
        .collect::<Vec<_>>();
    let report = prepared.adopt()?;
    administration.remember(&remote, &removed);
    for connection in &participants {
        connection.reload_flush();
    }
    let local = prepared.finish(report).await?;
    let outcomes = join_all(acknowledgements.into_iter().map(|acknowledge| {
        let signal = CancellationToken::new();
        let driver = host.driver.clone();
        async move { await_phase(acknowledge, driver, deadline, &signal).await }
    }))
    .await;
    if outcomes.iter().any(Result::is_err) {
        let participants = participants
            .iter()
            .zip(&outcomes)
            .map(|(connection, outcome)| {
                serde_json::json!({
                    "name": connection.reload_peer_name(),
                    "codeVersion": target.get(),
                    "outcome": if outcome.is_ok() { "acknowledged" } else { "unknown" },
                    "cause": outcome.as_ref().err(),
                })
            })
            .collect::<Vec<_>>();
        return Err(RuntimeError::unavailable(
            "reload_unknown_outcome",
            "one or more child commit acknowledgements were lost",
        )
        .with_details(serde_json::json!({
            "codeVersion": target.get(),
            "participants": participants,
        })));
    }
    let children = participants
        .iter()
        .zip(votes)
        .map(|(connection, vote)| {
            let summary = vote.expect("all votes checked before decision");
            ParticipantReloadReport {
                name: connection.reload_peer_name().to_owned(),
                version: target.get(),
                prepared: usize::try_from(summary.prepared.get()).unwrap_or(usize::MAX),
                gated: usize::try_from(summary.gated.get()).unwrap_or(usize::MAX),
            }
        })
        .collect();
    Ok(ClusterReloadReport { local, children })
}

// This bounds remote phases; it is not an upper bound on arbitrary application callbacks.
fn phase_timeout(host: &HostInner, options: &ReloadOptions) -> Result<Duration> {
    options
        .gate_deadline
        .checked_add(options.retire_deadline)
        .and_then(|duration| duration.checked_add(options.task_stop_deadline))
        .and_then(|duration| duration.checked_add(host.ready_timeout))
        .and_then(|duration| duration.checked_add(Duration::from_secs(5)))
        .filter(|duration| host.driver.now().checked_add(*duration).is_some())
        .ok_or_else(|| {
            RuntimeError::invalid(
                "reload_deadline",
                "remote phase timeout overflows Driver clock",
            )
        })
}

fn abort_all(connections: &[Connection], transaction: PositiveId, error: &RuntimeError) {
    for connection in connections {
        connection.reload_abort(transaction, error.clone());
    }
}
async fn await_phase<T: 'static>(
    wait: LocalBoxFuture<'static, Result<T>>,
    driver: Rc<dyn crate::Driver>,
    deadline: Duration,
    signal: &CancellationToken,
) -> Result<T> {
    let timeout = async move {
        match futures::future::select(driver.sleep(deadline), signal.cancelled()).await {
            futures::future::Either::Left(_) => {
                RuntimeError::unavailable("reload_vote_deadline", "reload phase deadline elapsed")
            }
            futures::future::Either::Right((cause, _)) => cause,
        }
    }
    .boxed_local();
    match futures::future::select(wait, timeout).await {
        futures::future::Either::Left((result, _)) => result,
        futures::future::Either::Right((cause, _)) => Err(cause),
    }
}

pub(crate) fn reconcile_child(host: Rc<HostInner>, connection: Connection) {
    let administration = host.reload_administration();
    let operation = match administration.begin(None) {
        Ok(operation) => operation,
        Err(error) => {
            connection.reload_fail(error);
            return;
        }
    };
    host.driver.clone().spawn(
        async move {
            let signal = operation.cancellation();
            let result = async {
                loop {
                    signal.check()?;
                    let changed = administration.changed.borrow().clone();
                    if !administration.busy.get() {
                        break;
                    }
                    match futures::future::select(changed.cancelled(), signal.cancelled()).await {
                        futures::future::Either::Left(_) => {}
                        futures::future::Either::Right((error, _)) => return Err(error),
                    }
                }
                let _slot = administration.slot()?;
                let target = PositiveId::new(host.reload_current_version())?;
                if connection.reload_peer_epoch() == target.get() {
                    connection.reload_mark_synced(target);
                    return Ok(());
                }
                if connection.reload_peer_epoch() > target.get() {
                    return Err(RuntimeError::new(
                        crate::ErrorCode::UpgradeRequired,
                        "reload_epoch",
                        "child is ahead of parent",
                    ));
                }
                let transaction = administration.next_transaction()?;
                let remote = administration
                    .desired
                    .borrow()
                    .values()
                    .cloned()
                    .collect::<Vec<_>>();
                let remove = administration
                    .removed
                    .borrow()
                    .iter()
                    .cloned()
                    .collect::<Vec<_>>();
                types::validate_batch(&remote, &remove)?;
                if let Some(version) = connection
                    .reload_peer_epoch()
                    .checked_sub(1)
                    .filter(|version| *version > 0)
                {
                    connection.reload_retire(PositiveId::new(version)?)?;
                }
                let options = ReloadOptions::default();
                let deadline = phase_timeout(&host, &options)?;
                let timing = Timing::from_options(&options)?;
                let wait =
                    connection.reload_prepare(transaction, target, remote, remove, timing, true);
                if let Err(error) = await_phase(wait, host.driver.clone(), deadline, &signal).await
                {
                    connection.reload_abort(transaction, error.clone());
                    return Err(error);
                }
                if host.reload_current_version() != target.get() {
                    let error = RuntimeError::unavailable(
                        "reload_changed",
                        "parent moved during reconciliation",
                    );
                    connection.reload_abort(transaction, error.clone());
                    return Err(error);
                }
                let bytes = connection.reload_prepare_commit(transaction, target)?;
                let acknowledge = connection.reload_append_commit(transaction, target, bytes);
                connection.reload_flush();
                await_phase(acknowledge, host.driver.clone(), deadline, &signal).await?;
                Ok(())
            }
            .await;
            if let Err(error) = result {
                connection.reload_fail(error);
            }
            drop(operation);
        }
        .boxed_local(),
    );
}
