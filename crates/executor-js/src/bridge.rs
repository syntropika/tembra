//! Codec-bound requests let native plans call back into the same JS realm without reentering V8.
use super::*;
use futures::{
    FutureExt, StreamExt,
    channel::{mpsc, oneshot},
};

type Reply = oneshot::Sender<Result<Option<Value>>>;
struct Pending {
    reply: Reply,
    activation: Option<(tembra::core::Id, tembra::core::Id)>,
}

enum Request {
    Invoke(Value, Reply),
    Notify(u8, tembra::core::Id),
}

pub(super) struct CallbackBridge {
    receive: RefCell<Option<mpsc::UnboundedReceiver<Request>>>,
    pending: RefCell<BTreeMap<tembra::core::Id, Pending>>,
}

impl CallbackBridge {
    pub(super) fn attach(native: &Host) -> Self {
        let (send, receive) = mpsc::unbounded();
        #[cfg(feature = "hooks")]
        {
            let send = send.clone();
            native.set_hook_dispatcher(move |operation, context, input| {
                let send = send.clone();
                async move {
                    context.check()?;
                    request(&send,json!({
                        "kind":"hook", "operation":operation,"root":context.root,"frame":context.frame(),
                        "call":context.invocation()?.id,"input":input,
                    })).await
                }.boxed_local()
            });
        }
        {
            let send = send.clone();
            native.set_driver_wake(move || {
                let _ = send.unbounded_send(Request::Notify(1, 0));
            });
        }
        {
            let send = send.clone();
            native.set_foreign_notifier(move |notification| {
                let (kind, id) = match notification {
                    tembra::ForeignNotification::RootEnded(id) => (2, id),
                    tembra::ForeignNotification::ReferenceReleased(id) => (3, id),
                };
                let _ = send.unbounded_send(Request::Notify(kind, id));
            });
        }
        native.set_foreign_dispatcher(move |input| {
            let send = send.clone();
            async move {
                request(&send, input)
                    .await
                    .map(|result| result.unwrap_or(Value::Null))
            }
            .boxed_local()
        });
        Self {
            receive: RefCell::new(Some(receive)),
            pending: RefCell::new(BTreeMap::new()),
        }
    }
}

async fn request(send: &mpsc::UnboundedSender<Request>, input: Value) -> Result<Option<Value>> {
    let (reply, receive) = oneshot::channel();
    send.unbounded_send(Request::Invoke(input, reply))
        .map_err(|_| Error::invalid(Reason::Javascript, "JS dispatcher is closed"))?;
    receive
        .await
        .map_err(|_| Error::invalid(Reason::Javascript, "JS reply was dropped"))?
}

#[op2(fast)]
fn op_tembra_has_contribution(
    state: &mut OpState,
    operation: f64,
) -> std::result::Result<bool, deno_error::JsErrorBox> {
    #[cfg(feature = "hooks")]
    return Ok(state
        .borrow::<Rc<bindings::Bindings>>()
        .native
        .has_contribution(bindings::number_id(operation)?));
    #[cfg(not(feature = "hooks"))]
    {
        let _ = (state, operation);
        Ok(false)
    }
}

/// One pump receives requests, while each dispatched callback can await further native work.
#[op2]
#[buffer]
async fn op_tembra_callback_next(
    state: Rc<RefCell<OpState>>,
) -> std::result::Result<Vec<u8>, deno_error::JsErrorBox> {
    let bindings = state.borrow().borrow::<Rc<bindings::Bindings>>().clone();
    let mut receive = bindings
        .callbacks
        .receive
        .borrow_mut()
        .take()
        .ok_or_else(|| deno_error::JsErrorBox::generic("hook receiver already in use"))?;
    loop {
        match receive.next().await {
            None => {
                *bindings.callbacks.receive.borrow_mut() = Some(receive);
                return Ok(Vec::new());
            }
            Some(Request::Notify(kind, id)) => {
                *bindings.callbacks.receive.borrow_mut() = Some(receive);
                return Ok(frame(kind, id, Vec::new()));
            }
            Some(Request::Invoke(request, reply)) => {
                let activation = (request["kind"] == "activate"
                    && request["acquired"]["activate"] == true)
                    .then(|| {
                        Some((
                            request["acquired"]["object"].as_u64()?,
                            request["acquired"]["reference"].as_u64()?,
                        ))
                    })
                    .flatten();
                let result = bindings
                    .native
                    .kernel()
                    .allocate_handle()
                    .and_then(|id| bindings.codec.serialize(&request).map(|bytes| (id, bytes)));
                match result {
                    Ok((id, bytes)) => {
                        bindings
                            .callbacks
                            .pending
                            .borrow_mut()
                            .insert(id, Pending { reply, activation });
                        *bindings.callbacks.receive.borrow_mut() = Some(receive);
                        return Ok(frame(0, id, bytes));
                    }
                    Err(error) => {
                        // A rejected payload settles only its request; keep receiving subsequent work.
                        reject_activation(&bindings.native, activation);
                        let _ = reply.send(Err(error));
                    }
                }
            }
        }
    }
}

/// Retire a new reservation only when transport rejected it before JavaScript activation.
fn reject_activation(native: &Host, activation: Option<(tembra::core::Id, tembra::core::Id)>) {
    if let Some((object, reference)) = activation {
        if matches!(
            native.kernel().status(reference),
            Ok(tembra::core::Status::Live)
        ) {
            return;
        }
        let _ = native.kernel().fail(object);
        native.flush();
    }
}

/// Keep transport identity and control messages outside the application codec.
fn frame(kind: u8, id: tembra::core::Id, payload: Vec<u8>) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(9 + payload.len());
    bytes.push(kind);
    bytes.extend_from_slice(&id.to_le_bytes());
    bytes.extend_from_slice(&payload);
    bytes
}

/// Settle a decode or encode failure even when the application codec cannot carry errors.
#[op2(fast)]
fn op_tembra_callback_error(
    state: &mut OpState,
    id: f64,
    #[string] metadata: String,
    rejected_request: bool,
) -> std::result::Result<(), deno_error::JsErrorBox> {
    let bindings = state.borrow::<Rc<bindings::Bindings>>();
    let id = bindings::number_id(id)?;
    let value: Value = serde_json::from_str(&metadata)
        .map_err(|error| deno_error::JsErrorBox::generic(error.to_string()))?;
    let reply = bindings
        .callbacks
        .pending
        .borrow_mut()
        .remove(&id)
        .ok_or_else(|| deno_error::JsErrorBox::generic("unknown callback request"))?;
    if rejected_request {
        reject_activation(&bindings.native, reply.activation);
    }
    let _ = reply.reply.send(Err(callback_error(&value)));
    Ok(())
}

/// Decode structured callback failures consistently on both reply paths.
fn callback_error(error: &Value) -> Error {
    Error::new(
        tembra::Code::parse(error["code"].as_str().unwrap_or(""))
            .unwrap_or(tembra::Code::Unavailable),
        error["reason"].as_str().unwrap_or("javascript"),
        error["message"].as_str().unwrap_or("JS callback failed"),
    )
}

/// Reply only after the JS callback physically settles, preserving undefined separately from null.
#[op2(fast)]
fn op_tembra_callback_complete(
    state: &mut OpState,
    id: f64,
    #[buffer] payload: &[u8],
) -> std::result::Result<(), deno_error::JsErrorBox> {
    let bindings = state.borrow::<Rc<bindings::Bindings>>();
    let reply = bindings
        .callbacks
        .pending
        .borrow_mut()
        .remove(&bindings::number_id(id)?)
        .ok_or_else(|| deno_error::JsErrorBox::generic("unknown hook request"))?;
    let result = bindings.codec.deserialize(payload).and_then(|envelope| {
        if let Some(error) = envelope.get("error") {
            Err(callback_error(error))
        } else {
            Ok(envelope.get("value").cloned())
        }
    });
    let _ = reply.reply.send(result);
    Ok(())
}

extension!(
    tembra_callback_ops,
    ops = [
        op_tembra_has_contribution,
        op_tembra_callback_next,
        op_tembra_callback_complete,
        op_tembra_callback_error
    ]
);
