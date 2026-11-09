//! Fixed-buffer interpreter steps keep waits outside the interpreter's native stack.
use super::runtime::{State, request};
use crate::abi::{self, Completion, Operation, Reply, ReplyKind};
use futures::{FutureExt, StreamExt, stream::FuturesUnordered};
use std::collections::BTreeSet;
use wasmtime::{Instance, Memory, Store, TypedFunc};

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
        let mut pending = FuturesUnordered::new();
        let mut outstanding = BTreeSet::new();
        loop {
            let reply = Reply::decode(&bytes).map_err(violation)?;
            if let Some(outcome) = Self::finish(&reply)? {
                return Ok(outcome);
            }
            if pending.len() + reply.effects.len() > abi::EFFECT_LIMIT as usize {
                return Err(violation("host operation queue is full"));
            }
            for (id, packet) in &reply.effects {
                if !outstanding.insert(*id) {
                    return Err(violation("duplicate effect identity"));
                }
                let receive = request(store.data(), text(packet)?, true);
                let id = *id;
                pending.push(
                    async move {
                        let value = match receive {
                            Ok(receive) => receive
                                .await
                                .unwrap_or_else(|_| Err("host callback scope ended".into())),
                            Err(error) => Err(error),
                        };
                        (id, value)
                    }
                    .boxed(),
                );
            }
            let first = if pending.is_empty() {
                futures::future::pending().await
            } else {
                pending.next().await.expect("nonempty effect queue")
            };
            let mut settled = vec![first];
            while let Some(Some(ready)) = pending.next().now_or_never() {
                settled.push(ready);
            }
            for (id, _) in &settled {
                outstanding.remove(id);
            }
            let completions: Vec<Completion<'_>> = settled
                .iter()
                .map(|(id, value)| Completion {
                    id: *id,
                    ok: value.is_ok(),
                    value: match value {
                        Ok(value) | Err(value) => value.as_bytes(),
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
