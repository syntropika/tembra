use super::*;
use crate::delta;

impl RemoteBinding {
    pub(crate) fn rpc_shape(&self, attachment: AttachmentId, frame: ShapeFrame) -> Result<()> {
        let result = self.receive_shape(attachment, frame);
        if let Err(error) = &result {
            self.fail(error.clone());
        }
        result
    }
    fn receive_shape(&self, attachment: AttachmentId, frame: ShapeFrame) -> Result<()> {
        if !self.attached(attachment) {
            return Ok(());
        }
        let attached_hold = self
            .attachment
            .borrow()
            .as_ref()
            .map(|current| current.hold);
        if attached_hold != Some(frame.hold)
            || frame.consumer_fingerprint != self.contract.fingerprint()
        {
            return Err(mismatch(
                "shape",
                "shape is not bound to the requested consumer",
            ));
        }
        let provider: Contract = serde_json::from_value(frame.provider.clone())
            .map_err(|error| mismatch("descriptor", error.to_string()))?;
        if provider.kind() != tembra_contracts::ContractKind::Remote
            || provider.name() != self.contract.name()
            || provider.placement() != self.contract.placement()
        {
            return Err(mismatch(
                "placement",
                "provider contract does not match the requested remote type",
            ));
        }
        tembra_contracts::identity_compatible(provider.input(), self.contract.input())
            .map_err(|error| mismatch("identity", error.message))?;
        if provider
            .identity(&self.input)
            .map_err(|error| mismatch("identity", error.message))?
            != self
                .contract
                .identity(&self.input)
                .map_err(|error| mismatch("identity", error.message))?
        {
            return Err(mismatch(
                "identity",
                "provider and consumer identify different objects",
            ));
        }
        match &frame.mode {
            BindingMode::Direct => provider
                .covers(&self.contract)
                .map_err(|error| mismatch("coverage", error.message))?,
            BindingMode::Adapted {
                consumer_fingerprint,
            } => {
                if consumer_fingerprint != self.contract.fingerprint()
                    || provider.version() == self.contract.version()
                {
                    return Err(mismatch("adapter", "invalid consumer adapter attestation"));
                }
            }
        }
        let members = self
            .contract
            .members(&frame.variant)
            .ok_or_else(|| mismatch("variant", "provider selected an unknown consumer variant"))?;
        let expected: std::collections::BTreeSet<_> = members.keys().cloned().collect();
        if frame
            .members
            .iter()
            .cloned()
            .collect::<std::collections::BTreeSet<_>>()
            != expected
            || frame.members.len() != expected.len()
        {
            return Err(mismatch(
                "members",
                "shape does not contain exactly the consumer-visible members",
            ));
        }
        let states: std::collections::BTreeSet<_> = members
            .iter()
            .filter_map(|(name, member)| {
                matches!(member, Member::State { .. }).then_some(name.clone())
            })
            .collect();
        let manifest: std::collections::BTreeSet<_> = frame
            .state
            .iter()
            .map(|state| state.member.clone())
            .collect();
        if manifest.len() != frame.state.len() || (!manifest.is_empty() && manifest != states) {
            return Err(mismatch("state", "incomplete or unexpected state manifest"));
        }
        let connection = self
            .attachment
            .borrow()
            .as_ref()
            .and_then(|attachment| self.connection(attachment.connection))
            .ok_or_else(closed)?;
        if frame.state.len() > connection.limits().max_state_members {
            return Err(RuntimeError::invalid(
                "state_limit",
                "too many state members in hydration unit",
            ));
        }
        if self
            .current
            .borrow()
            .as_ref()
            .is_some_and(|current| current.attachment == attachment && frame.unit <= current.unit)
            || self
                .pending
                .borrow()
                .as_ref()
                .is_some_and(|pending| frame.unit <= pending.frame.unit)
        {
            return Err(RuntimeError::invalid("unit", "shape unit must advance"));
        }
        let bytes = encode(
            &Frame::Shape(frame.clone()),
            connection.limits().max_frame_bytes,
        )?
        .len();
        let host = self.host.upgrade().ok_or_else(closed)?;
        let previous = self.rpc_staged_bytes();
        if bytes > connection.limits().max_unit_bytes
            || host
                .rpc_staged_bytes()
                .saturating_sub(previous)
                .checked_add(bytes)
                .is_none_or(|total| total > host.rpc_limits.max_staging_bytes)
        {
            return Err(RuntimeError::invalid(
                "staging_limit",
                "shape exceeds initial unit or Host staging budget",
            ));
        }
        *self.pending.borrow_mut() = Some(Unit {
            frame,
            provider: Rc::new(provider),
            staged: HashMap::new(),
            bytes,
        });
        Ok(())
    }

    pub(crate) fn rpc_state(
        &self,
        attachment: AttachmentId,
        frame: ReplicaFrame,
    ) -> Result<ReplicaDisposition> {
        let result = self.receive_state(attachment, frame);
        if let Err(error) = &result {
            self.fail(error.clone());
        }
        result
    }
    fn receive_state(
        &self,
        attachment: AttachmentId,
        frame: ReplicaFrame,
    ) -> Result<ReplicaDisposition> {
        if !self.attached(attachment) {
            return Ok(ReplicaDisposition::Ignore);
        }
        let key = frame.key().clone();
        if self
            .attachment
            .borrow()
            .as_ref()
            .is_none_or(|current| current.hold != key.hold)
        {
            return Err(RuntimeError::invalid("state", "state hold is not current"));
        }
        let pending = self.pending.borrow().as_ref().map(|pending| {
            (
                pending.frame.unit,
                pending.frame.variant.clone(),
                pending.frame.state.clone(),
            )
        });
        if let Some((unit, variant, manifest)) = pending
            && key.unit == unit
        {
            let Some(member) = manifest.iter().find(|member| member.member == key.member) else {
                return Err(RuntimeError::invalid(
                    "state",
                    "initial snapshot is outside manifest",
                ));
            };
            if key.stream != member.stream {
                return Err(RuntimeError::invalid(
                    "stream",
                    "initial stream generation mismatch",
                ));
            }
            let connection = self
                .attachment
                .borrow()
                .as_ref()
                .and_then(|attachment| self.connection(attachment.connection))
                .ok_or_else(closed)?;
            let wire = match &frame {
                ReplicaFrame::Snapshot { key, snapshot } => Frame::Snapshot {
                    key: key.clone(),
                    snapshot: snapshot.clone(),
                },
                ReplicaFrame::Reset { key, snapshot } => Frame::Reset {
                    key: key.clone(),
                    snapshot: snapshot.clone(),
                },
                _ => {
                    return Err(RuntimeError::invalid(
                        "state",
                        "only a snapshot can initialize state",
                    ));
                }
            };
            let bytes = encode(&wire, connection.limits().max_frame_bytes)?.len();
            let snapshot = match frame {
                ReplicaFrame::Snapshot { snapshot, .. } | ReplicaFrame::Reset { snapshot, .. } => {
                    self.checked_snapshot(&variant, &key.member, snapshot)?
                }
                ReplicaFrame::Error { error, .. } => return Err(error),
                ReplicaFrame::Delta { .. } => {
                    return Err(RuntimeError::invalid(
                        "state",
                        "delta cannot initialize a state stream",
                    ));
                }
            };
            let host = self.host.upgrade().ok_or_else(closed)?;
            let connection = self
                .attachment
                .borrow()
                .as_ref()
                .and_then(|attachment| self.connection(attachment.connection))
                .ok_or_else(closed)?;
            if host
                .rpc_staged_bytes()
                .checked_add(bytes)
                .is_none_or(|total| total > host.rpc_limits.max_staging_bytes)
            {
                return Err(RuntimeError::invalid(
                    "staging_limit",
                    "Host initial state staging budget reached",
                ));
            }
            let maximum = connection.limits().max_unit_bytes;
            let mut pending = self.pending.borrow_mut();
            let pending = pending.as_mut().ok_or_else(closed)?;
            if pending.staged.contains_key(&key.member) {
                return Err(RuntimeError::invalid(
                    "state",
                    "duplicate initial state snapshot",
                ));
            }
            if pending
                .bytes
                .checked_add(bytes)
                .is_none_or(|bytes| bytes > maximum)
            {
                return Err(RuntimeError::invalid(
                    "unit_limit",
                    "initial unit staging exceeds its byte budget",
                ));
            }
            pending.bytes += bytes;
            pending.staged.insert(key.member, snapshot);
            return Ok(ReplicaDisposition::Accepted);
        }
        let current = self.current.borrow().as_ref().map(|current| {
            (
                current.unit,
                current.shape.variant.clone(),
                current.streams.get(&key.member).copied(),
            )
        });
        let Some((unit, variant, stream)) = current else {
            return Err(RuntimeError::invalid(
                "state",
                "state arrived before a shape",
            ));
        };
        if key.unit < unit {
            return Ok(ReplicaDisposition::Ignore);
        }
        if key.unit != unit || stream.is_none() {
            return Err(RuntimeError::invalid(
                "state",
                "state unit/member is not current",
            ));
        }
        let stream = stream.expect("checked current stream");
        if key.stream < stream {
            return Ok(ReplicaDisposition::Ignore);
        }
        if key.stream > stream && !matches!(frame, ReplicaFrame::Reset { .. }) {
            return Err(RuntimeError::invalid(
                "stream",
                "only Reset establishes a new stream generation",
            ));
        }
        if key.stream == stream && matches!(frame, ReplicaFrame::Reset { .. }) {
            return Err(RuntimeError::invalid(
                "stream",
                "Reset must establish a fresh stream generation",
            ));
        }
        let port = self.port(&key.member)?;
        let previous = port.current().ok();
        let result = match frame {
            ReplicaFrame::Snapshot { .. } => {
                return Err(RuntimeError::invalid(
                    "state",
                    "duplicate snapshot outside hydration",
                ));
            }
            ReplicaFrame::Reset { snapshot, .. } => self
                .checked_snapshot(variant.as_deref().unwrap_or(""), &key.member, snapshot)
                .map(StateEvent::Reset),
            ReplicaFrame::Delta { batch, .. } => previous
                .as_ref()
                .ok_or_else(|| {
                    RuntimeError::new(ErrorCode::NotReady, "state", "delta base is not hydrated")
                })
                .and_then(|previous| {
                    delta::apply(previous.value(), previous.revision(), &batch)
                        .map_err(|error| RuntimeError::invalid("delta", error.to_string()))
                })
                .and_then(|value| {
                    self.checked_snapshot(
                        variant.as_deref().unwrap_or(""),
                        &key.member,
                        WireSnapshot {
                            value,
                            revision: SafeU64::new(batch.result_revision())?,
                        },
                    )
                })
                .map(|snapshot| StateEvent::Ops {
                    delta: batch,
                    snapshot,
                }),
            ReplicaFrame::Error { error, .. } => Err(error),
        };
        let Ok(event) = result else {
            let limit = self
                .attachment
                .borrow()
                .as_ref()
                .and_then(|attachment| self.connection(attachment.connection))
                .ok_or_else(closed)?
                .limits()
                .recoveries;
            let failures = {
                let mut recoveries = self.recoveries.borrow_mut();
                let failures = recoveries.entry(key.member.clone()).or_default();
                *failures += 1;
                *failures
            };
            if failures >= limit {
                return Err(RuntimeError::unavailable(
                    "state_recovery",
                    "consecutive state recovery limit reached",
                ));
            }
            return Ok(ReplicaDisposition::Resync {
                seen_revision: previous.map(|snapshot| snapshot.revision()),
            });
        };
        self.recoveries.borrow_mut().remove(&key.member);
        let host = self.host.upgrade().ok_or_else(closed)?;
        let hold = host.publication_queue.hold();
        let values = vec![(key.member.clone(), event.clone())];
        let mut ports = vec![(port, event)];
        for claim in self.claims() {
            ports.extend(claim.remote_candidates(&values)?);
        }
        let publications = ports
            .into_iter()
            .filter_map(|(port, event)| port.adopt(event))
            .collect();
        if let Some(current) = self.current.borrow_mut().as_mut() {
            current.streams.insert(key.member, key.stream);
        }
        host.publication_queue.append_batch(publications);
        drop(hold);
        Ok(ReplicaDisposition::Accepted)
    }

    pub(crate) fn rpc_publish(&self, attachment: AttachmentId) -> Result<()> {
        let result = self.publish_unit(attachment);
        if let Err(error) = &result {
            self.fail(error.clone());
        }
        result
    }
    fn publish_unit(&self, attachment: AttachmentId) -> Result<()> {
        if !self.attached(attachment) {
            return Ok(());
        }
        let complete = self
            .pending
            .borrow()
            .as_ref()
            .is_some_and(|pending| pending.staged.len() == pending.frame.state.len());
        if !complete {
            return Ok(());
        }
        let pending = self.pending.borrow_mut().take().ok_or_else(closed)?;
        let host = self.host.upgrade().ok_or_else(closed)?;
        let hold = host.publication_queue.hold();
        let reset = self.current.borrow().is_some();
        let values: Vec<_> = pending
            .staged
            .into_iter()
            .map(|(name, snapshot)| {
                (
                    name,
                    if reset {
                        StateEvent::Reset(snapshot)
                    } else {
                        StateEvent::Snapshot(snapshot)
                    },
                )
            })
            .collect();
        let mut ports = Vec::new();
        for (name, event) in &values {
            ports.push((self.port(name)?, event.clone()));
        }
        let claims = self.claims();
        for claim in &claims {
            ports.extend(claim.remote_candidates(&values)?);
        }
        let publications = ports
            .into_iter()
            .filter_map(|(port, event)| port.adopt(event))
            .collect();
        let members = self
            .contract
            .members(&pending.frame.variant)
            .ok_or_else(|| mismatch("variant", "consumer variant disappeared"))?;
        let state_count = members
            .values()
            .filter(|member| matches!(member, Member::State { .. }))
            .count();
        self.hydrated.set(state_count == values.len());
        *self.current.borrow_mut() = Some(Current {
            attachment,
            provider: pending.provider.clone(),
            mode: pending.frame.mode.clone(),
            shape: Shape {
                variant: Some(pending.frame.variant),
                members: pending.frame.members,
                contract: pending.provider.fingerprint().into(),
            },
            unit: pending.frame.unit,
            streams: pending
                .frame
                .state
                .into_iter()
                .map(|state| (state.member, state.stream))
                .collect(),
        });
        self.connected.set(true);
        host.publication_queue.append_batch(publications);
        for claim in &claims {
            claim.remote_set_ready(!claim.wants_state || self.hydrated.get());
        }
        for claim in claims {
            if self.claim_status(&claim) == RefStatus::Live {
                claim.remote_publish_ready();
            }
        }
        drop(hold);
        Ok(())
    }

    fn checked_snapshot(&self, variant: &str, name: &str, value: WireSnapshot) -> Result<Snapshot> {
        let Some(Member::State { schema }) = self
            .contract
            .members(variant)
            .and_then(|members| members.get(name))
        else {
            return Err(RuntimeError::new(
                ErrorCode::NotInVariant,
                "state",
                "state is outside consumer shape",
            ));
        };
        tembra_contracts::validate(schema, &value.value)
            .map_err(|error| RuntimeError::invalid("state", error.message))?;
        Snapshot::new(Rc::new(value.value), value.revision.get())
    }
}
fn mismatch(reason: &str, message: impl Into<String>) -> RuntimeError {
    RuntimeError::new(ErrorCode::ContractMismatch, reason, message)
}
