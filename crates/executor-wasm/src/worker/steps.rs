//! Fixed-buffer interpreter steps keep waits outside the interpreter's native stack.
use super::runtime::{State, request};
use crate::abi::{self, Completion, Operation, Reply, ReplyKind};
use futures::{FutureExt, StreamExt, future::BoxFuture, stream::FuturesUnordered};
use std::collections::BTreeSet;
use wasmtime::{AsContextMut, Instance, Memory, Store, TypedFunc};

/// One settled effect: identity, value, and the publication journal of a standard reply.
pub(super) type Settled = (u64, Result<Vec<u8>, Vec<u8>>, super::standard::Journal);

/// Await one admitted (or refused) standard packet as a settled effect carrying its journal.
pub(super) fn standard_effect(
    id: u64,
    admitted: Result<super::standard::Completion, String>,
) -> BoxFuture<'static, Settled> {
    async move {
        let value = match admitted {
            Ok(receive) => receive
                .await
                .unwrap_or_else(|_| Err("standard call was revoked".into())),
            Err(error) => Err(error),
        };
        match value {
            Ok(delivery) => {
                let (bytes, journal) = delivery.split();
                (id, Ok(bytes), journal)
            }
            Err(error) => (id, Err(error.into_bytes()), Default::default()),
        }
    }
    .boxed()
}

/// Undo completions the guest will never receive: release their handles and poison streams
/// whose read drained data. Callers first drop the callback's tasks: an aborted task never runs
/// again, so every issued reply is already queued.
pub(super) fn discard(
    store: &mut Store<State>,
    pending: &mut FuturesUnordered<BoxFuture<'static, Settled>>,
) -> wasmtime::Result<()> {
    while let Some(Some((_, _, journal))) = pending.next().now_or_never() {
        super::standard::abandon(store.data_mut(), journal)?;
    }
    pending.clear();
    Ok(())
}

/// One logical callback result; `interrupted` quarantines the Store.
pub(super) struct Outcome {
    pub value: Result<String, String>,
    pub interrupted: bool,
}

pub(super) struct Driver {
    alloc: TypedFunc<u32, u32>,
    call: TypedFunc<(u32, u32, u32), u32>,
    memory: Memory,
}

/// Any layout or encoding violation is a trap of this instance, never a callback error.
fn violation(message: impl Into<String>) -> wasmtime::Error {
    wasmtime::Error::msg(format!("interpreter ABI violation: {}", message.into()))
}

/// Convert a checked UTF-8 body into an owned string.
fn text(bytes: &[u8]) -> wasmtime::Result<String> {
    String::from_utf8(bytes.to_vec()).map_err(|_| violation("text body is not UTF-8"))
}

impl Driver {
    /// Run the reactor constructor once and resolve checked typed exports for this installation.
    pub async fn new(
        instance: &Instance,
        store: &mut Store<State>,
        memory: Memory,
    ) -> wasmtime::Result<Self> {
        instance
            .get_typed_func::<(), ()>(&mut *store, abi::INITIALIZE_EXPORT)?
            .call_async(&mut *store, ())
            .await?;
        Ok(Self {
            alloc: instance.get_typed_func(&mut *store, abi::ALLOC_EXPORT)?,
            call: instance.get_typed_func(&mut *store, abi::CALL_EXPORT)?,
            memory,
        })
    }

    /// Copy one input into the guest-reserved buffer, run the operation and copy its reply out.
    async fn invoke(
        &self,
        store: &mut Store<State>,
        operation: Operation,
        input: Option<&[u8]>,
    ) -> wasmtime::Result<Vec<u8>> {
        let (pointer, length) = match input {
            Some(input) => {
                let length = u32::try_from(input.len())
                    .ok()
                    .filter(|length| *length <= abi::LENGTH_LIMIT)
                    .ok_or_else(|| violation("input exceeds the ABI length limit"))?;
                let pointer = self.alloc.call_async(&mut *store, length).await?;
                if pointer == 0 {
                    return Err(violation("input reservation was refused"));
                }
                self.memory.write(&mut *store, pointer as usize, input)?;
                (pointer, length)
            }
            None => (0, 0),
        };
        let reply = self
            .call
            .call_async(&mut *store, (operation as u32, pointer, length))
            .await? as usize;
        // Read through the current memory view; the call may have grown memory.
        let data = self.memory.data(&*store);
        let total = data
            .get(reply..reply + 4)
            .map(|header| u32::from_le_bytes(header.try_into().expect("header")) as usize)
            .ok_or_else(|| violation("reply header is outside guest memory"))?;
        Ok(data
            .get(reply..reply + total)
            .ok_or_else(|| violation("reply is outside guest memory"))?
            .to_vec())
    }

    /// Run one operation whose reply must be a final outcome.
    async fn outcome(
        &self,
        store: &mut Store<State>,
        operation: Operation,
        input: Option<&[u8]>,
    ) -> wasmtime::Result<Outcome> {
        let bytes = self.invoke(store, operation, input).await?;
        let reply = Reply::decode(&bytes).map_err(violation)?;
        Self::finish(&reply)?.ok_or_else(|| violation("operation cannot return effects"))
    }

    /// Convert a final reply into an outcome; `None` means effects are pending.
    fn finish(reply: &Reply<'_>) -> wasmtime::Result<Option<Outcome>> {
        let value = match reply.kind {
            ReplyKind::Pending => return Ok(None),
            ReplyKind::Ok => Ok(text(reply.value)?),
            ReplyKind::Error => Err(text(reply.value)?),
        };
        Ok(Some(Outcome {
            value,
            interrupted: reply.interrupted,
        }))
    }

    /// Wasm steps return pending host effects without retaining an I/O wait on the guest stack.
    pub async fn call(
        &self,
        operation: Operation,
        packet: String,
        store: &mut Store<State>,
    ) -> wasmtime::Result<Outcome> {
        if operation != Operation::Start {
            return self
                .outcome(store, operation, Some(packet.as_bytes()))
                .await;
        }
        let mut bytes = self
            .invoke(&mut *store, Operation::Start, Some(packet.as_bytes()))
            .await?;
        let cancelled = store.data().callback_scope.cancelled();
        let mut cancelled = std::pin::pin!(cancelled);
        let mut revoked = false;
        let mut pending: FuturesUnordered<BoxFuture<'static, Settled>> = FuturesUnordered::new();
        let mut outstanding = BTreeSet::new();
        // Standard WASI tasks of this callback; dropping the owner revokes pending work and
        // borrows, while revision-owned descriptors and streams stay in the Store.
        let mut tasks = super::standard::Tasks::default();
        loop {
            super::standard::check_trap(store.data_mut())?;
            let reply = Reply::decode(&bytes).map_err(violation)?;
            if let Some(outcome) = Self::finish(&reply)? {
                // Effects the guest abandoned cannot publish resources after the callback ends,
                // and their admitted work is disposed before the Orb goes idle.
                super::standard::retire(store, &mut tasks).await?;
                discard(store, &mut pending)?;
                return Ok(outcome);
            }
            if pending.len() + reply.effects.len() > abi::EFFECT_LIMIT as usize {
                return Err(violation("host operation queue is full"));
            }
            for (id, packet) in &reply.effects {
                if !outstanding.insert(*id) {
                    return Err(violation("duplicate effect identity"));
                }
                let id = *id;
                if packet.first() == Some(&super::standard::PACKET_TAG) {
                    let receive = match super::standard::spawn(store.as_context_mut(), packet)? {
                        Ok((handle, leases, receive)) => {
                            tasks.push(handle, leases);
                            Ok(receive)
                        }
                        Err(error) => Err(error),
                    };
                    pending.push(standard_effect(id, receive));
                    continue;
                }
                let receive = request(store.data(), text(packet)?, true);
                pending.push(
                    async move {
                        let value = match receive {
                            Ok(receive) => receive
                                .await
                                .unwrap_or_else(|_| Err("host callback scope ended".into())),
                            Err(error) => Err(error),
                        };
                        (
                            id,
                            value.map(String::into_bytes).map_err(String::into_bytes),
                            Default::default(),
                        )
                    }
                    .boxed(),
                );
            }
            // Qualified upstream filesystem calls block only inside this bounded poll. Pending
            // clocks and readers release the worker budget before awaiting another wakeup.
            let files = super::standard::uses_filesystem(store.data());
            let call = store.run_concurrent(async |_| {
                tokio::select! {
                    biased;
                    _ = &mut cancelled, if !revoked => None,
                    ready = async {
                        if pending.is_empty() { futures::future::pending().await }
                        else { pending.next().await.expect("nonempty effect queue") }
                    } => Some(ready),
                }
            });
            let first = if files {
                crate::io::file_turn(call)
                    .await
                    .map_err(|error| violation(error.to_string()))??
            } else {
                call.await?
            };
            super::standard::check_trap(store.data_mut())?;
            let mut settled = if let Some(first) = first {
                vec![first]
            } else {
                // Reject issued promises while the heap can still run bounded finally blocks.
                // Subsequent standard calls lose authority; resource drops remain cleanup.
                // A reply that completed in the same turn is unpublished: release its handles.
                revoked = true;
                super::standard::retire(store, &mut tasks).await?;
                discard(store, &mut pending)?;
                if outstanding.is_empty() {
                    return self.outcome(store, Operation::Cancel, None).await;
                }
                outstanding
                    .iter()
                    .map(|id| (*id, Err(b"callback cancelled".to_vec()), Default::default()))
                    .collect()
            };
            while let Some(Some(ready)) = pending.next().now_or_never() {
                settled.push(ready);
            }
            for (id, _, _) in &settled {
                outstanding.remove(id);
            }
            let completions: Vec<Completion<'_>> = settled
                .iter()
                .map(|(id, value, _)| Completion {
                    id: *id,
                    ok: value.is_ok(),
                    value: match value {
                        Ok(value) | Err(value) => value.as_slice(),
                    },
                })
                .collect();
            let batch = abi::encode_completions(&completions).map_err(violation)?;
            bytes = self
                .invoke(&mut *store, Operation::Resume, Some(&batch))
                .await?;
        }
    }

    /// Capture idle heap bytes without retaining any host authority.
    #[cfg(test)]
    pub async fn capture(
        &self,
        store: &mut Store<State>,
    ) -> wasmtime::Result<Result<Vec<u8>, String>> {
        let bytes = self.invoke(store, Operation::Capture, None).await?;
        let reply = Reply::decode(&bytes).map_err(violation)?;
        Ok(match reply.kind {
            ReplyKind::Ok => Ok(reply.value.to_vec()),
            ReplyKind::Error => Err(text(reply.value)?),
            ReplyKind::Pending => return Err(violation("capture cannot return effects")),
        })
    }
}
