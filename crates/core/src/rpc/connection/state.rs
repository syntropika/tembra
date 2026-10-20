#[cfg(test)]
mod tests;

use super::*;
use crate::{
    Readable, Snapshot, StateEvent,
    delta::{DeltaBatch, diff},
};
use tembra_contracts::Member;

struct Frontier {
    revision: u64,
    bytes: usize,
    sent: Duration,
    snapshot: Snapshot,
}
pub(super) struct Feed {
    source: Readable,
    acked: Option<Snapshot>,
    outstanding: Option<Frontier>,
    dirty: bool,
    reset: bool,
    recoveries: usize,
    last_ack: Option<u64>,
}

impl Connection {
    pub(super) fn delta_allowed(&self, batch: &DeltaBatch) -> bool {
        batch.ops().len() <= self.inner.options.limits.max_delta_ops
            && batch.ops().iter().all(|operation| {
                let path = match operation {
                    crate::delta::DeltaOp::Set { path, .. }
                    | crate::delta::DeltaOp::Remove { path }
                    | crate::delta::DeltaOp::Splice { path, .. } => path,
                };
                path.len() <= self.inner.options.limits.max_path_segments
            })
    }
    pub(super) fn start_served(&self, hold: HoldId, record: Rc<ServedHold>) {
        let generation = record.generation.get() + 1;
        record.generation.set(generation);
        let weak = Rc::downgrade(&self.inner);
        self.inner.driver.spawn(Box::pin(async move {
            let result = async {
                record.reference.ready().await?;
                let shape = record
                    .reference
                    .shape()
                    .ok_or_else(|| RuntimeError::unavailable("shape", "reference has no shape"))?;
                let variant = shape.variant.clone().unwrap_or_default();
                let mut sources = Vec::new();
                if record.state_on.get() {
                    for (name, member) in
                        record.request.consumer.members(&variant).ok_or_else(|| {
                            RuntimeError::invalid("variant", "unknown consumer variant")
                        })?
                    {
                        if matches!(member, Member::State { .. }) {
                            sources.push((name.clone(), record.reference.readable(name)?));
                        }
                    }
                    futures::future::try_join_all(
                        sources.iter().map(|(_, source)| source.await_hydrated()),
                    )
                    .await?;
                }
                let Some(inner) = weak.upgrade() else {
                    return Ok(());
                };
                let connection = Connection { inner };
                if connection.status() != ConnectionStatus::Open
                    || record.generation.get() != generation
                    || !connection
                        .inner
                        .served
                        .borrow()
                        .get(&hold)
                        .is_some_and(|current| Rc::ptr_eq(current, &record))
                {
                    return Ok(());
                }
                connection.capture_unit(hold, &record, variant, sources)
            }
            .await;
            if let Err(error) = result
                && let Some(inner) = weak.upgrade()
            {
                let connection = Connection { inner };
                let current = connection
                    .inner
                    .served
                    .borrow()
                    .get(&hold)
                    .is_some_and(|current| {
                        Rc::ptr_eq(current, &record) && current.generation.get() == generation
                    });
                if current {
                    connection.remove_feeds(hold);
                    connection.inner.served.borrow_mut().remove(&hold);
                    record.reference.release();
                    if connection.status() == ConnectionStatus::Open
                        && let Err(error) = connection.send(Frame::Gone {
                            hold,
                            error: connection.consumer_error(error).into(),
                        })
                    {
                        connection.fail(error);
                    }
                }
            }
        }));
    }
    fn capture_unit(
        &self,
        hold: HoldId,
        record: &Rc<ServedHold>,
        variant: String,
        sources: Vec<(String, Readable)>,
    ) -> Result<()> {
        if sources.len() > self.inner.options.limits.max_state_members
            || self.inner.streams.borrow().len() + sources.len()
                > self.inner.options.limits.max_streams
        {
            return Err(RuntimeError::unavailable(
                "state_limit",
                "state member budget exhausted",
            ));
        }
        let unit = UnitId::new(
            record
                .unit
                .get()
                .checked_add(1)
                .ok_or_else(|| RuntimeError::unavailable("rpc_id", "unit id exhausted"))?,
        )?;
        let (provider, mode) = record.reference.rpc_shape_data()?;
        let members: Vec<_> = record
            .request
            .consumer
            .members(&variant)
            .ok_or_else(|| RuntimeError::invalid("variant", "unknown consumer variant"))?
            .keys()
            .cloned()
            .collect();
        let mut state = Vec::new();
        let mut prepared = Vec::new();
        let mut subscriptions = Vec::new();
        for (name, source) in sources {
            let key = StateKey {
                hold,
                unit,
                member: name.clone(),
                stream: StreamId::new(Self::next(&self.inner.next_stream)?)?,
            };
            let weak = Rc::downgrade(&self.inner);
            let watched = key.clone();
            subscriptions.push(source.subscribe_events(move |event| {
                if let Some(inner) = weak.upgrade() {
                    Connection { inner }
                        .feed_changed(&watched, matches!(event, StateEvent::Reset(_)));
                }
                Ok(())
            })?);
            let snapshot = source.snapshot()?;
            let frame = Frame::Snapshot {
                key: key.clone(),
                snapshot: WireSnapshot {
                    revision: SafeU64::new(snapshot.revision())?,
                    value: snapshot.value().clone(),
                },
            };
            let bytes = encode(&frame, self.inner.peer_frame.get())?;
            state.push(StateMember {
                member: name,
                stream: key.stream,
            });
            prepared.push((key, source, snapshot, bytes));
        }
        let shape = Frame::Shape(ShapeFrame {
            hold,
            unit,
            consumer_fingerprint: record.request.consumer.fingerprint().into(),
            provider: tembra_contracts::to_portable_value(provider.descriptor())
                .map_err(crate::host::contract_error)?,
            mode,
            variant,
            members,
            state,
        });
        let shape = encode(&shape, self.inner.peer_frame.get())?;
        let total = prepared
            .iter()
            .try_fold(shape.len(), |total, (_, _, _, bytes)| {
                total.checked_add(bytes.len())
            })
            .ok_or_else(|| RuntimeError::invalid("unit_limit", "unit size overflow"))?;
        let state_bytes: usize = prepared.iter().map(|(_, _, _, bytes)| bytes.len()).sum();
        let limits = &self.inner.options.limits;
        if total > self.inner.peer_unit.get()
            || total > limits.max_unit_bytes
            || state_bytes
                > limits
                    .state_in_flight
                    .saturating_sub(self.inner.state_bytes.get())
            || !self.output_fits(total, prepared.len() + 1, !prepared.is_empty())
        {
            return Err(RuntimeError::unavailable(
                "unit_limit",
                "complete hydration unit exceeds bounded admission",
            ));
        }
        record.unit.set(unit.get());
        let old = std::mem::replace(&mut *record.subscriptions.borrow_mut(), subscriptions);
        // Commit the whole unit's credit, feeds and FIFO frames before any flush,
        // subscription destructor or transport callback can admit unrelated state.
        self.inner
            .state_bytes
            .set(self.inner.state_bytes.get() + state_bytes);
        let mut batch = Vec::with_capacity(prepared.len() + 1);
        batch.push(shape);
        for (key, source, snapshot, bytes) in prepared {
            self.inner.streams.borrow_mut().insert(
                key,
                Feed {
                    source,
                    acked: None,
                    outstanding: Some(Frontier {
                        revision: snapshot.revision(),
                        bytes: bytes.len(),
                        sent: self.inner.driver.now(),
                        snapshot,
                    }),
                    dirty: false,
                    reset: false,
                    recoveries: 0,
                    last_ack: None,
                },
            );
            batch.push(bytes);
        }
        self.append_output(batch);
        record.ready.set(true);
        drop(old);
        self.flush();
        Ok(())
    }
    pub(super) fn remove_feeds(&self, hold: HoldId) {
        let keys: Vec<_> = self
            .inner
            .streams
            .borrow()
            .keys()
            .filter(|key| key.hold == hold)
            .cloned()
            .collect();
        let mut removed = Vec::new();
        for key in keys {
            if let Some(feed) = self.inner.streams.borrow_mut().remove(&key) {
                if let Some(frontier) = &feed.outstanding {
                    self.inner
                        .state_bytes
                        .set(self.inner.state_bytes.get().saturating_sub(frontier.bytes));
                }
                removed.push(feed);
            }
        }
        drop(removed);
    }
    fn feed_changed(&self, key: &StateKey, reset: bool) {
        // A source subscription spans wire resets; its original stream is retired.
        let current = self
            .inner
            .streams
            .borrow()
            .keys()
            .find(|candidate| {
                candidate.hold == key.hold
                    && candidate.unit == key.unit
                    && candidate.member == key.member
            })
            .cloned();
        let Some(current) = current else { return };
        if let Some(feed) = self.inner.streams.borrow_mut().get_mut(&current) {
            feed.dirty = true;
            feed.reset |= reset;
        }
        self.flush();
    }
    pub(super) fn flush_feeds(&self) -> Result<()> {
        let mut keys: Vec<_> = self.inner.streams.borrow().keys().cloned().collect();
        keys.sort();
        let start = self
            .inner
            .last_feed
            .borrow()
            .as_ref()
            .map_or(0, |previous| keys.partition_point(|key| key <= previous));
        for key in keys[start..].iter().chain(&keys[..start]) {
            if let Err(error) = self.flush_feed(key) {
                self.end_served(key.hold, error)?;
            }
        }
        Ok(())
    }
    fn end_served(&self, hold: HoldId, error: RuntimeError) -> Result<()> {
        self.remove_feeds(hold);
        let record = self.inner.served.borrow_mut().remove(&hold);
        if let Some(record) = record {
            record.reference.release();
            self.send(Frame::Gone {
                hold,
                error: self.consumer_error(error).into(),
            })?;
        }
        Ok(())
    }
    fn flush_feed(&self, key: &StateKey) -> Result<()> {
        if self.status() != ConnectionStatus::Open {
            return Ok(());
        }
        let candidate = self.inner.streams.borrow().get(key).and_then(|feed| {
            (feed.dirty && feed.outstanding.is_none())
                .then(|| (feed.source.clone(), feed.acked.clone(), feed.reset))
        });
        let Some((source, previous, reset)) = candidate else {
            return Ok(());
        };
        let snapshot = source.snapshot()?;
        let Some(previous) = previous else {
            return Ok(());
        };
        if snapshot.revision() == previous.revision() && !reset {
            if let Some(feed) = self.inner.streams.borrow_mut().get_mut(key) {
                feed.dirty = false;
            }
            return Ok(());
        }
        let ops = diff(previous.value(), snapshot.value())
            .map_err(|error| RuntimeError::invalid("delta", error.to_string()))?;
        let mut replacement_key = key.clone();
        let delta = DeltaBatch::new(previous.revision(), snapshot.revision(), ops);
        let delta_frame = delta
            .ok()
            .filter(|batch| self.delta_allowed(batch))
            .map(|batch| Frame::Delta {
                key: key.clone(),
                batch,
            });
        let encoded_delta = delta_frame
            .as_ref()
            .and_then(|frame| encode(frame, self.inner.peer_frame.get()).ok());
        let use_reset = reset
            || encoded_delta
                .as_ref()
                .is_none_or(|bytes| bytes.len() > self.inner.options.limits.deferred_bytes);
        let bytes =
            if use_reset {
                replacement_key.stream =
                    StreamId::new(self.inner.next_stream.get().checked_add(1).ok_or_else(
                        || RuntimeError::unavailable("rpc_id", "stream id exhausted"),
                    )?)?;
                encode(
                    &Frame::Reset {
                        key: replacement_key.clone(),
                        snapshot: WireSnapshot {
                            revision: SafeU64::new(snapshot.revision())?,
                            value: snapshot.value().clone(),
                        },
                    },
                    self.inner.peer_frame.get(),
                )?
            } else {
                encoded_delta.expect("checked encoded delta")
            };
        let outstanding = self.inner.state_bytes.get();
        let limit = self.inner.peer_window.get();
        if bytes.len()
            > self
                .inner
                .options
                .limits
                .state_in_flight
                .saturating_sub(outstanding)
            || (outstanding != 0 && bytes.len() > limit.saturating_sub(outstanding))
        {
            return Ok(());
        }
        if !self.output_fits(bytes.len(), 1, true) {
            return Ok(());
        }
        let feed = self.inner.streams.borrow_mut().remove(key);
        if let Some(mut feed) = feed {
            if use_reset {
                self.inner.next_stream.set(replacement_key.stream.get());
            }
            feed.dirty = false;
            feed.reset = false;
            feed.outstanding = Some(Frontier {
                revision: snapshot.revision(),
                bytes: bytes.len(),
                sent: self.inner.driver.now(),
                snapshot,
            });
            self.inner.state_bytes.set(outstanding + bytes.len());
            *self.inner.last_feed.borrow_mut() = Some(replacement_key.clone());
            self.inner
                .streams
                .borrow_mut()
                .insert(replacement_key, feed);
            self.append_output(vec![bytes]);
        }
        Ok(())
    }
    pub(super) fn state_ack(&self, key: StateKey, revision: u64) -> Result<()> {
        let mut feed = self.inner.streams.borrow_mut().remove(&key);
        if let Some(current) = &mut feed {
            if current
                .outstanding
                .as_ref()
                .is_some_and(|frontier| frontier.revision == revision)
            {
                let frontier = current.outstanding.take().expect("matching frontier");
                self.inner
                    .state_bytes
                    .set(self.inner.state_bytes.get().saturating_sub(frontier.bytes));
                current.acked = Some(frontier.snapshot);
                current.last_ack = Some(revision);
                current.recoveries = 0;
            } else if current.last_ack != Some(revision) {
                self.inner
                    .streams
                    .borrow_mut()
                    .insert(key, feed.expect("present feed"));
                return Err(RuntimeError::invalid(
                    "state_ack",
                    "ACK does not name the exact outstanding frontier",
                ));
            }
            self.inner
                .streams
                .borrow_mut()
                .insert(key, feed.expect("present feed"));
            self.flush();
            return Ok(());
        }
        self.check_retired_key(&key)
    }
    pub(super) fn state_resync(&self, key: StateKey, failed: u64) -> Result<()> {
        let feed = self.inner.streams.borrow_mut().remove(&key);
        let Some(mut feed) = feed else {
            return self.check_retired_key(&key);
        };
        if !feed
            .outstanding
            .as_ref()
            .is_some_and(|frontier| frontier.revision == failed)
        {
            self.inner.streams.borrow_mut().insert(key, feed);
            return Err(RuntimeError::invalid(
                "state_resync",
                "negative ACK does not name outstanding frontier",
            ));
        }
        let frontier = feed.outstanding.take().expect("matching frontier");
        self.inner
            .state_bytes
            .set(self.inner.state_bytes.get().saturating_sub(frontier.bytes));
        feed.recoveries += 1;
        if feed.recoveries >= self.inner.options.limits.recoveries {
            return Err(RuntimeError::unavailable(
                "state_recovery",
                "consecutive state recovery limit reached",
            ));
        }
        feed.acked = Some(frontier.snapshot);
        feed.dirty = true;
        feed.reset = true;
        self.inner.streams.borrow_mut().insert(key, feed);
        self.flush();
        Ok(())
    }
    fn check_retired_key(&self, key: &StateKey) -> Result<()> {
        let record = self.inner.served.borrow().get(&key.hold).cloned();
        if let Some(record) = record {
            let variant = record
                .reference
                .shape()
                .and_then(|shape| shape.variant)
                .unwrap_or_default();
            let declared = matches!(
                record
                    .request
                    .consumer
                    .members(&variant)
                    .and_then(|members| members.get(&key.member)),
                Some(Member::State { .. })
            );
            let current_stream = self
                .inner
                .streams
                .borrow()
                .keys()
                .find(|current| {
                    current.hold == key.hold
                        && current.unit == key.unit
                        && current.member == key.member
                })
                .map(|current| current.stream);
            if key.unit.get() > record.unit.get()
                || !declared
                || current_stream.is_some_and(|stream| key.stream > stream)
            {
                return Err(RuntimeError::invalid(
                    "state_key",
                    "state frontier was never admitted",
                ));
            }
        }
        if key.stream.get() > self.inner.next_stream.get()
            || key.hold.get() > self.inner.received_hold.get()
        {
            Err(RuntimeError::invalid(
                "state_key",
                "unknown future state key",
            ))
        } else {
            Ok(())
        }
    }
    pub(super) fn check_ack_deadlines(&self) {
        let expired = self.inner.streams.borrow().values().any(|feed| {
            feed.outstanding.as_ref().is_some_and(|frontier| {
                self.inner.driver.now().saturating_sub(frontier.sent)
                    >= self.inner.options.ack_deadline
            })
        });
        if expired {
            self.fail(RuntimeError::unavailable(
                "state_ack_timeout",
                "state acknowledgement deadline elapsed",
            ));
        }
    }
    pub(super) fn receive_replica(&self, frame: ReplicaFrame) -> Result<()> {
        self.require_replies()?;
        let key = frame.key().clone();
        let revision = frame.revision();
        if let Some((binding, attachment)) = self.remote_binding(key.hold)? {
            let disposition = binding.rpc_state(attachment, frame)?;
            let staged = self
                .inner
                .remote
                .borrow()
                .values()
                .filter_map(|entry| entry.binding.upgrade())
                .try_fold(0usize, |total, binding| {
                    total.checked_add(binding.rpc_staged_bytes())
                });
            if staged.is_none_or(|bytes| bytes > self.inner.options.limits.staging_bytes) {
                let error = RuntimeError::unavailable(
                    "staging_limit",
                    "connection initial staging budget exhausted",
                );
                binding.rpc_gone(attachment, error.clone());
                return Err(error);
            }
            match disposition {
                ReplicaDisposition::Ignore => {}
                ReplicaDisposition::Accepted => {
                    if let Some(revision) = revision {
                        self.send(Frame::StateAck {
                            key,
                            revision: SafeU64::new(revision)?,
                        })?;
                    }
                    binding.rpc_publish(attachment)?;
                }
                ReplicaDisposition::Resync { seen_revision } => {
                    if let Some(failed_revision) = revision {
                        self.send(Frame::Resync {
                            key,
                            failed_revision: SafeU64::new(failed_revision)?,
                            seen_revision: seen_revision.map(SafeU64::new).transpose()?,
                        })?;
                    }
                }
            }
        }
        Ok(())
    }
    pub(super) fn replica_recovery(&self, key: StateKey, failed_revision: u64) -> Result<()> {
        self.require_replies()?;
        if let Some((binding, attachment)) = self.remote_binding(key.hold)? {
            // Check correlated unit/member/stream before acknowledging malformed work.
            let disposition = binding.rpc_state(
                attachment,
                ReplicaFrame::Error {
                    key: key.clone(),
                    error: RuntimeError::invalid("delta", "malformed checked delta payload"),
                },
            )?;
            if let ReplicaDisposition::Resync { seen_revision } = disposition {
                self.send(Frame::Resync {
                    key,
                    failed_revision: SafeU64::new(failed_revision)?,
                    seen_revision: seen_revision.map(SafeU64::new).transpose()?,
                })?;
            }
        }
        Ok(())
    }
}
