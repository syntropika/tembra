use super::*;
use crate::{ErrorCode, RefStatus};
use tembra_contracts::{Contract, ContractKind};

impl Connection {
    pub(super) fn message(&self, bytes: &[u8]) {
        if self.status() == ConnectionStatus::Closed {
            return;
        }
        self.inner.last_seen.set(self.inner.driver.now());
        let result = match decode(bytes, self.inner.options.limits.max_frame_bytes) {
            Ok(frame) => self.dispatch(frame),
            Err(DecodeError::RecoverableDelta {
                key,
                failed_revision,
                error,
            }) => {
                if self.status() != ConnectionStatus::Open {
                    Err(*error)
                } else {
                    self.replica_recovery(key, failed_revision)
                }
            }
            Err(DecodeError::Fatal(error)) => Err(error),
        };
        if let Err(error) = result {
            self.fail(error);
        }
    }

    fn dispatch(&self, frame: Frame) -> Result<()> {
        if let Frame::Hello { hello } = frame {
            return self.receive_hello(hello);
        }
        self.require_open()?;
        match frame {
            Frame::ReloadPrepare {
                transaction,
                code_version,
                facets,
                remove,
                timing,
                catchup,
            } => {
                self.receive_reload_prepare(
                    transaction,
                    code_version,
                    facets,
                    remove,
                    timing,
                    catchup,
                )?;
            }
            Frame::ReloadVote {
                transaction,
                code_version,
                outcome,
            } => self.receive_reload_vote(transaction, code_version, outcome)?,
            Frame::ReloadCommit {
                transaction,
                code_version,
            } => self.receive_reload_commit(transaction, code_version)?,
            Frame::ReloadAck {
                transaction,
                code_version,
            } => self.receive_reload_ack(transaction, code_version)?,
            Frame::ReloadAbort { transaction, error } => {
                self.receive_reload_abort(transaction, error.into())?
            }
            Frame::ReloadRetire { code_version } => self.receive_reload_retire(code_version)?,
            Frame::Hello { .. } => unreachable!(),
            Frame::Bye { error } => self.close(CloseReason::new(error.into())),
            Frame::Ping { nonce } => self.send(Frame::Pong { nonce })?,
            Frame::Pong { nonce } => {
                if nonce.get() > self.inner.next_ping.get() {
                    return Err(RuntimeError::invalid(
                        "rpc_ping",
                        "pong names an unsent ping",
                    ));
                }
            }
            Frame::Hold(hold) => {
                self.require_requests()?;
                self.receive_hold(hold, None)?;
            }
            Frame::ForwardHold(hold) => {
                if !matches!(self.profile(), ConnectionProfile::TrustedParent { .. }) {
                    return Err(Self::forbidden());
                }
                self.receive_hold(
                    HoldFrame {
                        hold: hold.hold,
                        target: hold.target,
                        consumer: hold.consumer,
                        input: hold.input,
                        state: hold.state,
                    },
                    Some(hold.caller),
                )?;
            }
            Frame::Release { hold } => {
                self.require_requests()?;
                let removed = self.inner.served.borrow_mut().remove(&hold);
                if let Some(record) = removed {
                    self.remove_feeds(hold);
                    record.reference.release();
                    drop(record);
                    self.flush();
                } else if hold.get() > self.inner.received_hold.get() {
                    return Err(RuntimeError::invalid(
                        "rpc_hold",
                        "release names an unissued hold",
                    ));
                }
            }
            Frame::Subscribe { hold } | Frame::Unsubscribe { hold } => {
                self.require_requests()?;
                let wanted = matches!(frame, Frame::Subscribe { .. });
                let record = self.inner.served.borrow().get(&hold).cloned();
                if let Some(record) = record {
                    if record.state_on.replace(wanted) != wanted {
                        self.remove_feeds(hold);
                        let subscriptions = std::mem::take(&mut *record.subscriptions.borrow_mut());
                        drop(subscriptions);
                        self.start_served(hold, record);
                        self.flush();
                    }
                } else if hold.get() > self.inner.received_hold.get() {
                    return Err(RuntimeError::invalid(
                        "rpc_hold",
                        "subscription names an unissued hold",
                    ));
                }
            }
            Frame::Call {
                call,
                hold,
                member,
                args,
            } => {
                self.require_requests()?;
                self.receive_call(call, hold, member, args.into(), None)?;
            }
            Frame::TrustedCall {
                call,
                hold,
                member,
                args,
                root,
            } => {
                if !self.trusted() {
                    return Err(Self::forbidden());
                }
                self.receive_call(call, hold, member, args.into(), Some(root))?;
            }
            Frame::Cancel { call } => {
                self.require_requests()?;
                let token = self
                    .inner
                    .incoming
                    .borrow()
                    .get(&call)
                    .map(|job| job.cancellation.clone());
                if let Some(token) = token {
                    token.cancel(RuntimeError::aborted(
                        "cancelled",
                        "remote caller cancelled",
                    ));
                } else if call.get() > self.inner.received_call.get() {
                    return Err(RuntimeError::invalid(
                        "rpc_call",
                        "cancel names an unissued call",
                    ));
                }
            }
            Frame::Result { call, outcome } => {
                self.require_replies()?;
                let sender = self.inner.calls.borrow_mut().remove(&call);
                if let Some(sender) = sender {
                    let result = match outcome {
                        CallOutcome::Success(value) => Ok(value.into()),
                        CallOutcome::Error(error) => Err(error.into()),
                    };
                    let _ = sender.send(result);
                } else if call.get() > self.inner.next_call.get() {
                    return Err(RuntimeError::invalid(
                        "rpc_call",
                        "result names an unissued call",
                    ));
                }
            }
            Frame::Shape(shape) => {
                self.require_replies()?;
                if let Some((binding, attachment)) = self.remote_binding(shape.hold)? {
                    binding.rpc_shape(attachment, shape)?;
                    binding.rpc_publish(attachment)?;
                }
            }
            Frame::Gone { hold, error } => {
                self.require_replies()?;
                let entry = self.inner.remote.borrow_mut().remove(&hold);
                if let Some(entry) = entry {
                    if let Some(binding) = entry.binding.upgrade() {
                        binding.rpc_gone(entry.attachment, error.into());
                    }
                } else if hold.get() > self.inner.next_hold.get() {
                    return Err(RuntimeError::invalid(
                        "rpc_hold",
                        "gone names an unissued hold",
                    ));
                }
            }
            Frame::Snapshot { key, snapshot } => {
                self.receive_replica(ReplicaFrame::Snapshot { key, snapshot })?;
            }
            Frame::Delta { key, batch } => {
                if self.delta_allowed(&batch) {
                    self.receive_replica(ReplicaFrame::Delta { key, batch })?;
                } else {
                    self.replica_recovery(key, batch.result_revision())?;
                }
            }
            Frame::Reset { key, snapshot } => {
                self.receive_replica(ReplicaFrame::Reset { key, snapshot })?;
            }
            Frame::StateError { key, error } => {
                self.receive_replica(ReplicaFrame::Error {
                    key,
                    error: error.into(),
                })?;
            }
            Frame::StateAck { key, revision } => {
                self.require_requests()?;
                self.state_ack(key, revision.get())?;
            }
            Frame::Resync {
                key,
                failed_revision,
                seen_revision: _,
            } => {
                self.require_requests()?;
                self.state_resync(key, failed_revision.get())?;
            }
            Frame::RootEnd { root } => {
                if !self.trusted() {
                    return Err(Self::forbidden());
                }
                if let Some(host) = self.inner.host.upgrade() {
                    host.rpc_root_end(self.id(), &root);
                }
            }
            Frame::InvalidatePrincipal {
                principal,
                origin,
                through_generation,
            } => {
                if !matches!(self.profile(), ConnectionProfile::TrustedParent { .. }) {
                    return Err(Self::forbidden());
                }
                self.invalidate(principal, origin, through_generation.get())?;
            }
        }
        Ok(())
    }
    fn receive_hello(&self, hello: Hello) -> Result<()> {
        if self.status() != ConnectionStatus::AwaitHello {
            return Err(RuntimeError::invalid("rpc_hello", "duplicate hello"));
        }
        if hello.protocol != "tembra-rpc"
            || hello.version.get() != 1
            || hello.descriptor_version.get() != 1
            || hello.delta_codec.get() != 1
        {
            return Err(RuntimeError::new(
                ErrorCode::UpgradeRequired,
                "protocol",
                "incompatible Tembra RPC protocol; upgrade required",
            ));
        }
        let valid = match (self.profile(), &hello.peer) {
            (ConnectionProfile::AcceptedClient, PeerHello::Client {}) => true,
            (
                ConnectionProfile::ClientToProvider { expected }
                | ConnectionProfile::AcceptedChild { expected }
                | ConnectionProfile::TrustedParent { expected },
                PeerHello::Host {
                    identity,
                    code_version: _,
                },
            ) => identity.host() == expected,
            _ => false,
        };
        if !valid {
            return Err(Self::forbidden());
        }
        let peer_epoch = match &hello.peer {
            PeerHello::Host { code_version, .. } => Some(code_version.get()),
            PeerHello::Client {} => None,
        };
        let offer = hello.receive;
        if offer.max_message_bytes.get() == 0
            || offer.max_unit_bytes.get() == 0
            || offer.state_window_bytes.get() == 0
        {
            return Err(RuntimeError::invalid("rpc_limits", "zero receive offer"));
        }
        let clamp = |offer: SafeU64, local: usize| {
            usize::try_from(offer.get()).map_or(local, |offer| offer.min(local))
        };
        self.inner.peer_frame.set(clamp(
            offer.max_message_bytes,
            self.inner.options.limits.max_frame_bytes,
        ));
        self.inner.peer_unit.set(clamp(
            offer.max_unit_bytes,
            self.inner.options.limits.max_unit_bytes,
        ));
        self.inner.peer_window.set(clamp(
            offer.state_window_bytes,
            self.inner.options.limits.state_window,
        ));
        self.inner.status.set(ConnectionStatus::Open);
        if self.reload_hello(peer_epoch)? {
            self.inner.opened.cancel(RuntimeError::unavailable(
                "open",
                "compatible hello received",
            ));
        }
        Ok(())
    }
    pub(super) fn forbidden() -> RuntimeError {
        RuntimeError::new(
            ErrorCode::Forbidden,
            "rpc_role",
            "frame is forbidden for configured connection role",
        )
    }
    pub(super) fn consumer_error(&self, mut error: RuntimeError) -> RuntimeError {
        if matches!(self.profile(), ConnectionProfile::AcceptedClient)
            && error.code == ErrorCode::ContractMismatch
        {
            error.code = ErrorCode::UpgradeRequired;
        }
        error
    }
    fn require_requests(&self) -> Result<()> {
        if self.inner.profile.accepts_requests() {
            Ok(())
        } else {
            Err(Self::forbidden())
        }
    }
    pub(super) fn require_replies(&self) -> Result<()> {
        if !matches!(self.profile(), ConnectionProfile::AcceptedClient) {
            Ok(())
        } else {
            Err(Self::forbidden())
        }
    }
    pub(super) fn remote_binding(
        &self,
        hold: HoldId,
    ) -> Result<Option<(Rc<RemoteBinding>, AttachmentId)>> {
        let entry = self.inner.remote.borrow().get(&hold).and_then(|entry| {
            entry
                .binding
                .upgrade()
                .map(|binding| (binding, entry.attachment))
        });
        if entry.is_none() && hold.get() > self.inner.next_hold.get() {
            return Err(RuntimeError::invalid(
                "rpc_hold",
                "reply names an unissued hold",
            ));
        }
        Ok(entry)
    }
    fn receive_hold(&self, hold: HoldFrame, caller: Option<ForwardedCaller>) -> Result<()> {
        if hold.hold.get() <= self.inner.received_hold.get() {
            return Err(RuntimeError::invalid(
                "rpc_hold",
                "duplicate or reused hold id",
            ));
        }
        self.inner.received_hold.set(hold.hold.get());
        let outcome = (|| {
            if self.inner.served.borrow().len() >= self.inner.options.limits.max_holds {
                return Err(RuntimeError::unavailable(
                    "hold_limit",
                    "connection hold budget exhausted",
                ));
            }
            let descriptor = tembra_contracts::canonical_json(&hold.consumer)
                .map_err(crate::host::contract_error)?;
            let consumer =
                Rc::new(Contract::from_json(&descriptor).map_err(crate::host::contract_error)?);
            if consumer.kind() != ContractKind::Remote {
                return Err(RuntimeError::invalid(
                    "remote_contract",
                    "local contracts cannot cross RPC",
                ));
            }
            let principal = caller.as_ref().map_or_else(
                || self.inner.principal.clone(),
                |caller| caller.principal.clone(),
            );
            if let Some(caller) = &caller {
                let identity = tembra_contracts::canonical_json(&caller.principal)
                    .map_err(crate::host::contract_error)?;
                if self
                    .inner
                    .revoked
                    .borrow()
                    .get(&(caller.origin.clone(), identity))
                    .is_some_and(|cutoff| caller.authorization_generation.get() <= *cutoff)
                {
                    return Err(RuntimeError::new(
                        ErrorCode::Forbidden,
                        "revoked",
                        "forwarded origin grant was revoked",
                    ));
                }
            }
            let host = self
                .inner
                .host
                .upgrade()
                .ok_or_else(|| RuntimeError::unavailable("host", "host no longer exists"))?;
            let caller = match caller {
                Some(caller) => Some(caller),
                None => {
                    let identity = host.rpc_identity()?;
                    Some(ForwardedCaller {
                        principal: principal.clone(),
                        origin: format!(
                            "{}/{}/{}",
                            identity.host(),
                            identity.incarnation(),
                            self.id().get()
                        ),
                        authorization_generation: SafeU64::new(0)?,
                    })
                }
            };
            let request = BindRequest {
                target: hold.target,
                consumer,
                input: hold.input,
                state: hold.state,
                caller,
            };
            let reference = host.rpc_hold(request.clone(), principal.clone(), self.id())?;
            let record = Rc::new(ServedHold {
                reference,
                request,
                principal,
                unit: Cell::new(0),
                subscriptions: RefCell::default(),
                state_on: Cell::new(hold.state),
                ready: Cell::new(false),
                generation: Cell::new(0),
                status_subscription: RefCell::default(),
            });
            self.inner
                .served
                .borrow_mut()
                .insert(hold.hold, record.clone());
            let weak = Rc::downgrade(&self.inner);
            let watched = Rc::downgrade(&record);
            let hold_id = hold.hold;
            *record.status_subscription.borrow_mut() =
                Some(record.reference.on_status(move |status, error| {
                    if !matches!(status, RefStatus::Gone | RefStatus::Released) {
                        return;
                    }
                    let (Some(inner), Some(record)) = (weak.upgrade(), watched.upgrade()) else {
                        return;
                    };
                    let connection = Connection { inner };
                    let current = connection
                        .inner
                        .served
                        .borrow()
                        .get(&hold_id)
                        .is_some_and(|current| Rc::ptr_eq(current, &record));
                    if current {
                        let removed = connection.inner.served.borrow_mut().remove(&hold_id);
                        connection.remove_feeds(hold_id);
                        drop(removed);
                        if connection.status() == ConnectionStatus::Open
                            && let Err(error) = connection.send(Frame::Gone {
                                hold: hold_id,
                                error: error
                                    .unwrap_or_else(|| {
                                        RuntimeError::new(
                                            ErrorCode::Gone,
                                            "reference",
                                            "provider reference ended",
                                        )
                                    })
                                    .into(),
                            })
                        {
                            connection.fail(error);
                        }
                    }
                }));
            self.start_served(hold.hold, record);
            Ok(())
        })();
        if let Err(error) = outcome {
            self.send(Frame::Gone {
                hold: hold.hold,
                error: self.consumer_error(error).into(),
            })?;
        }
        Ok(())
    }
    fn receive_call(
        &self,
        call: CallId,
        hold: HoldId,
        member: String,
        args: Option<Value>,
        root: Option<RootRef>,
    ) -> Result<()> {
        if call.get() <= self.inner.received_call.get() {
            return Err(RuntimeError::invalid(
                "rpc_call",
                "duplicate or reused call id",
            ));
        }
        self.inner.received_call.set(call.get());
        let outcome = (|| {
            if self.inner.incoming.borrow().len() >= self.inner.options.limits.max_calls {
                return Err(RuntimeError::unavailable(
                    "call_limit",
                    "physical incoming call budget exhausted",
                ));
            }
            let record = self
                .inner
                .served
                .borrow()
                .get(&hold)
                .cloned()
                .ok_or_else(|| {
                    RuntimeError::invalid("rpc_hold", "call has no live held reference")
                })?;
            if !record.ready.get() || record.reference.status() != RefStatus::Live {
                return Err(RuntimeError::new(
                    ErrorCode::NotReady,
                    "reference",
                    "held reference is not ready",
                ));
            }
            let host = self
                .inner
                .host
                .upgrade()
                .ok_or_else(|| RuntimeError::unavailable("host", "host ended"))?;
            let entry = host.rpc_enter(root.as_ref(), self.id(), record.principal.clone())?;
            let cancellation = entry.context().cancellation().clone();
            self.inner
                .incoming
                .borrow_mut()
                .insert(call, IncomingCall { cancellation });
            let weak = Rc::downgrade(&self.inner);
            self.inner.driver.spawn(Box::pin(async move {
                // This job owns entry and Reference until the user future physically settles.
                let result = record
                    .reference
                    .call_json(&member, args, Some(entry.context()))
                    .await;
                drop(entry);
                if let Some(inner) = weak.upgrade() {
                    let finished = inner.incoming.borrow_mut().remove(&call);
                    drop(finished);
                    let connection = Connection { inner };
                    if connection.status() == ConnectionStatus::Open
                        && let Err(error) = connection.send_result(call, result)
                    {
                        connection.fail(error);
                    }
                }
            }));
            Ok(())
        })();
        if let Err(error) = outcome {
            self.send_result(call, Err(error))?;
        }
        Ok(())
    }
    fn send_result(&self, call: CallId, result: Result<Option<Value>>) -> Result<()> {
        let outcome = match result {
            Ok(value) => CallOutcome::Success(value.into()),
            Err(error) => CallOutcome::Error(error.into()),
        };
        let bytes = match encode(
            &Frame::Result { call, outcome },
            self.inner.peer_frame.get(),
        ) {
            Ok(bytes) => bytes,
            Err(error) => encode(
                &Frame::Result {
                    call,
                    outcome: CallOutcome::Error(
                        RuntimeError::new(
                            error.code,
                            error.reason,
                            "method outcome cannot fit the negotiated RPC envelope",
                        )
                        .into(),
                    ),
                },
                self.inner.peer_frame.get(),
            )?,
        };
        self.enqueue(bytes)
    }
    fn invalidate(&self, principal: Value, origin: String, generation: u64) -> Result<()> {
        let identity =
            tembra_contracts::canonical_json(&principal).map_err(crate::host::contract_error)?;
        let key = (origin.clone(), identity.clone());
        let additional = origin.len().checked_add(identity.len()).ok_or_else(|| {
            RuntimeError::unavailable("revocation_history", "revocation history size overflow")
        })?;
        let refused = {
            let history = self.inner.revoked.borrow();
            let bytes = history
                .keys()
                .try_fold(0usize, |total, (origin, principal)| {
                    total
                        .checked_add(origin.len())?
                        .checked_add(principal.len())
                });
            !history.contains_key(&key)
                && (history.len() >= self.inner.options.limits.max_revocation_history
                    || bytes
                        .and_then(|bytes| bytes.checked_add(additional))
                        .is_none_or(|bytes| bytes > self.inner.options.limits.max_revocation_bytes))
        };
        if refused {
            return Err(RuntimeError::unavailable(
                "revocation_history",
                "connection revocation history budget exhausted",
            ));
        }
        if let Some(host) = self.inner.host.upgrade() {
            host.rpc_invalidate_caller(self.id(), &principal, &origin, generation)?;
        }
        self.inner
            .revoked
            .borrow_mut()
            .entry(key)
            .and_modify(|cutoff| *cutoff = (*cutoff).max(generation))
            .or_insert(generation);
        let holds: Vec<_> = self
            .inner
            .served
            .borrow()
            .iter()
            .filter_map(|(id, record)| {
                let caller = record.request.caller.as_ref()?;
                let same = tembra_contracts::canonical_json(&caller.principal)
                    .is_ok_and(|value| value == identity);
                (same
                    && caller.origin == origin
                    && caller.authorization_generation.get() <= generation)
                    .then_some(*id)
            })
            .collect();
        for hold in holds {
            let record = self.inner.served.borrow_mut().remove(&hold);
            if let Some(record) = record {
                record.reference.release();
                self.remove_feeds(hold);
                self.send(Frame::Gone {
                    hold,
                    error: RuntimeError::new(
                        ErrorCode::Forbidden,
                        "revoked",
                        "origin grant revoked",
                    )
                    .into(),
                })?;
            }
        }
        Ok(())
    }
    pub(crate) fn bind(&self, weak: Weak<RemoteBinding>) -> Result<HoldId> {
        self.require_open()?;
        if matches!(self.profile(), ConnectionProfile::AcceptedClient) {
            return Err(Self::forbidden());
        }
        if self.inner.remote.borrow().len() >= self.inner.options.limits.max_holds {
            return Err(RuntimeError::unavailable(
                "hold_limit",
                "outgoing hold budget exhausted",
            ));
        }
        let hold = HoldId::new(Self::next(&self.inner.next_hold)?)?;
        let binding = weak.upgrade().ok_or_else(|| {
            RuntimeError::unavailable("released", "binding ended before attachment")
        })?;
        let attachment = binding.rpc_attach(self, hold)?;
        let result = (|| {
            let request = binding.rpc_bind_data(attachment)?;
            if request.caller.is_some()
                && !matches!(self.profile(), ConnectionProfile::AcceptedChild { .. })
            {
                return Err(Self::forbidden());
            }
            let consumer = tembra_contracts::to_portable_value(request.consumer.descriptor())
                .map_err(crate::host::contract_error)?;
            let frame = if let Some(caller) = request.caller {
                Frame::ForwardHold(ForwardHoldFrame {
                    hold,
                    target: request.target,
                    consumer,
                    input: request.input,
                    state: request.state,
                    caller,
                })
            } else {
                Frame::Hold(HoldFrame {
                    hold,
                    target: request.target,
                    consumer,
                    input: request.input,
                    state: request.state,
                })
            };
            self.inner.remote.borrow_mut().insert(
                hold,
                RemoteHold {
                    binding: weak,
                    attachment,
                },
            );
            self.send(frame)
        })();
        if let Err(error) = result {
            self.inner.remote.borrow_mut().remove(&hold);
            binding.rpc_detached(attachment, error.clone());
            return Err(error);
        }
        Ok(hold)
    }
    pub(crate) fn release(&self, hold: HoldId) {
        self.inner.remote.borrow_mut().remove(&hold);
        if self.status() == ConnectionStatus::Open
            && let Err(error) = self.send(Frame::Release { hold })
        {
            self.fail(error);
        }
    }
    pub(crate) fn want_state(&self, hold: HoldId, wanted: bool) -> Result<()> {
        self.require_open()?;
        let result = self.send(if wanted {
            Frame::Subscribe { hold }
        } else {
            Frame::Unsubscribe { hold }
        });
        if let Err(error) = &result {
            self.fail(error.clone());
        }
        result
    }
    pub(crate) fn root_end(&self, root: &RootId) -> Result<()> {
        if !self.trusted() {
            return Err(Self::forbidden());
        }
        let result = self.send(Frame::RootEnd { root: root.clone() });
        if let Err(error) = &result {
            self.fail(error.clone());
        }
        result
    }
    pub(crate) fn call(
        &self,
        hold: HoldId,
        member: String,
        args: Option<Value>,
        context: &Context,
    ) -> LocalBoxFuture<'static, Result<Option<Value>>> {
        let connection = self.clone();
        let context = context.clone();
        async move {
            connection.require_open()?;
            let host = connection
                .inner
                .host
                .upgrade()
                .ok_or_else(|| RuntimeError::unavailable("host", "host ended"))?;
            context.check(host.id)?;
            if connection.inner.calls.borrow().len() >= connection.inner.options.limits.max_calls {
                return Err(RuntimeError::unavailable(
                    "call_limit",
                    "outgoing call budget exhausted",
                ));
            }
            let call = CallId::new(Self::next(&connection.inner.next_call)?)?;
            let frame = if connection.trusted() {
                Frame::TrustedCall {
                    call,
                    hold,
                    member,
                    args: args.into(),
                    root: host.rpc_export(&context, connection.id())?,
                }
            } else {
                Frame::Call {
                    call,
                    hold,
                    member,
                    args: args.into(),
                }
            };
            let (sender, receiver) = oneshot::channel();
            connection.inner.calls.borrow_mut().insert(call, sender);
            if let Err(error) = connection.send(frame) {
                connection.inner.calls.borrow_mut().remove(&call);
                return Err(error);
            }
            let mut guard = OutgoingCall {
                inner: Rc::downgrade(&connection.inner),
                call,
                done: false,
            };
            let stopped =
                futures::future::select(context.cancellation().cancelled(), context.scope_ended())
                    .map(|either| match either {
                        futures::future::Either::Left((error, _))
                        | futures::future::Either::Right((error, _)) => error,
                    })
                    .boxed_local();
            let result = match futures::future::select(receiver, stopped).await {
                futures::future::Either::Left((result, _)) => {
                    guard.done = true;
                    result.unwrap_or_else(|_| {
                        Err(RuntimeError::unavailable(
                            "unknown_outcome",
                            "call correlation ended",
                        ))
                    })
                }
                futures::future::Either::Right((error, _)) => Err(error),
            };
            context.check(host.id)?;
            result
        }
        .boxed_local()
    }
}
struct OutgoingCall {
    inner: Weak<ConnectionInner>,
    call: CallId,
    done: bool,
}
impl Drop for OutgoingCall {
    fn drop(&mut self) {
        if let Some(inner) = self.inner.upgrade() {
            inner.calls.borrow_mut().remove(&self.call);
            if !self.done && inner.status.get() == ConnectionStatus::Open {
                let connection = Connection { inner };
                if let Err(error) = connection.send(Frame::Cancel { call: self.call }) {
                    connection.fail(error);
                }
            }
        }
    }
}
