//! Codec-bound requests let native plans call back into the same JS realm without reentering V8.
use super::*;
use futures::{
    FutureExt, StreamExt,
    channel::{mpsc, oneshot},
};

type Reply = oneshot::Sender<Result<Option<Value>>>;
type Request = (Value, Reply);

pub(super) struct CallbackBridge {
    receive: RefCell<Option<mpsc::UnboundedReceiver<Request>>>,
    pending: RefCell<BTreeMap<tembra::core::Id, Reply>>,
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
                let (reply, _receive) = oneshot::channel();
                let _ = send.unbounded_send((json!({"kind":"flush"}), reply));
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
    send.unbounded_send((input, reply))
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
    let request = receive.next().await;
    *bindings.callbacks.receive.borrow_mut() = Some(receive);
    let Some((mut request, reply)) = request else {
        return bindings
            .codec
            .serialize(&Value::Null)
            .map_err(|error| deno_error::JsErrorBox::generic(error.to_string()));
    };
    if request["kind"] == "flush" {
        return bindings
            .codec
            .serialize(&request)
            .map_err(|error| deno_error::JsErrorBox::generic(error.to_string()));
    }
    let id = bindings
        .native
        .kernel()
        .allocate_handle()
        .map_err(|error| deno_error::JsErrorBox::generic(error.to_string()))?;
    request["id"] = json!(id);
    let encoded = bindings.codec.serialize(&request);
    match encoded {
        Ok(bytes) => {
            bindings.callbacks.pending.borrow_mut().insert(id, reply);
            Ok(bytes)
        }
        Err(error) => {
            let _ = reply.send(Err(error.clone()));
            Err(deno_error::JsErrorBox::generic(error.to_string()))
        }
    }
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
            Err(Error::new(
                tembra::Code::parse(error["code"].as_str().unwrap_or(""))
                    .unwrap_or(tembra::Code::Unavailable),
                error["reason"].as_str().unwrap_or("javascript"),
                error["message"].as_str().unwrap_or("JS hook failed"),
            ))
        } else {
            Ok(envelope.get("value").cloned())
        }
    });
    let _ = reply.send(result);
    Ok(())
}

extension!(
    tembra_callback_ops,
    ops = [
        op_tembra_has_contribution,
        op_tembra_callback_next,
        op_tembra_callback_complete
    ]
);
