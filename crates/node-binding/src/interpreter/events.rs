//! Event forwarding never abandons a worker that is waiting on a native response sender.
use super::{EventCallback, envelope};
use napi::{Status, threadsafe_function::ThreadsafeFunctionCallMode};
use serde_json::json;
use std::{
    collections::BTreeMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU32, Ordering},
        mpsc,
    },
    thread,
};
use tembra_executor_wasm::{PortEvent, PortEvents, PortReply};

/// Keep request identities unique across exchanges so a late reply cannot resolve a newer call.
pub(super) fn spawn(
    callback: EventCallback,
    start: mpsc::Receiver<PortEvents>,
    replies: Arc<Mutex<BTreeMap<u32, PortReply>>>,
    sequence: Arc<AtomicU32>,
) -> std::io::Result<thread::JoinHandle<()>> {
    thread::Builder::new().name("tembra-node-events".into()).spawn(move || {
        let Ok(events)=start.recv() else {return};
        let mut delivering=true;
        loop {
            match events.receive() {
                Ok(PortEvent::Host {packet,asynchronous,reply}) => {
                    if !delivering {reply.complete(Err(tembra::Error::invalid("platform_callback","Node callback is closing")));continue;}
                    let id=match sequence.try_update(Ordering::Relaxed,Ordering::Relaxed,|value|value.checked_add(1)) {
                        Ok(previous)=>previous+1,
                        Err(_)=>{reply.complete(Err(tembra::Error::invalid("platform_callback","response identity space exhausted")));continue;}
                    };
                    replies.lock().expect("response map").insert(id,reply);
                    let packet=json!({"event":"host","id":id,"asynchronous":asynchronous,"packet":packet});
                    if callback.call(packet.to_string(),ThreadsafeFunctionCallMode::NonBlocking)!=Status::Ok {
                        delivering=false;
                        // A closing JS environment can no longer service buffered callbacks.
                        for reply in std::mem::take(&mut *replies.lock().expect("response map")).into_values() {
                            reply.complete(Err(tembra::Error::invalid("platform_callback","Node callback is closing")));
                        }
                    }
                }
                // Ordinary exchanges carry no incoming HTTP events.
                Ok(PortEvent::Ingress(_))=>{}
                Ok(PortEvent::Complete(result))=>{
                    replies.lock().expect("response map").clear();
                    if delivering {let _=callback.call(json!({"event":"complete","result":envelope(result)}).to_string(),ThreadsafeFunctionCallMode::NonBlocking);}
                    break;
                }
                Err(error)=>{
                    replies.lock().expect("response map").clear();
                    if delivering {let _=callback.call(json!({"event":"complete","result":envelope::<String>(Err(error))}).to_string(),ThreadsafeFunctionCallMode::NonBlocking);}
                    break;
                }
            }
        }
    })
}

/// Forward one incoming HTTP exchange's events as typed objects.
///
/// If the JavaScript callback can no longer be called (its environment is closing), nothing is
/// left waiting on it: pending host requests are refused, the carrier disconnects (the guest's
/// request read ends and its response is dropped), and the events are drained until the
/// exchange's physical completion.
pub(super) fn spawn_ingress(
    callback: super::IngressCallback,
    events: PortEvents,
    carrier: Arc<tembra_executor_wasm::ingress::IngressPort>,
    replies: Arc<Mutex<BTreeMap<u32, PortReply>>>,
    sequence: Arc<AtomicU32>,
) -> std::io::Result<thread::JoinHandle<()>> {
    use super::{NodeField, NodeIngressEvent};
    use tembra_executor_wasm::ingress::{IngressEvent, IngressField, IngressResponseFrame};
    fn fields(fields: Vec<IngressField>) -> Vec<NodeField> {
        fields
            .into_iter()
            .map(|field| NodeField {
                name: field.name,
                value: field.value.into(),
            })
            .collect()
    }
    fn event(kind: &str) -> NodeIngressEvent {
        NodeIngressEvent {
            kind: kind.into(),
            id: None,
            asynchronous: None,
            packet: None,
            result: None,
            status: None,
            fields: None,
            data: None,
            message: None,
        }
    }
    thread::Builder::new()
        .name("tembra-node-ingress".into())
        .spawn(move || {
            let mut delivering = true;
            let refuse = |reply: PortReply| {
                reply.complete(Err(tembra::Error::invalid(
                    "platform_callback",
                    "Node callback is closing",
                )))
            };
            loop {
                let next = match events.receive() {
                    Ok(PortEvent::Host {
                        packet,
                        asynchronous,
                        reply,
                    }) => {
                        if !delivering {
                            refuse(reply);
                            continue;
                        }
                        let id = match sequence.try_update(
                            Ordering::Relaxed,
                            Ordering::Relaxed,
                            |value| value.checked_add(1),
                        ) {
                            Ok(previous) => previous + 1,
                            Err(_) => {
                                reply.complete(Err(tembra::Error::invalid(
                                    "platform_callback",
                                    "response identity space exhausted",
                                )));
                                continue;
                            }
                        };
                        replies.lock().expect("response map").insert(id, reply);
                        NodeIngressEvent {
                            id: Some(id),
                            asynchronous: Some(asynchronous),
                            packet: Some(packet),
                            ..event("host")
                        }
                    }
                    Ok(PortEvent::Ingress(ingress)) => match ingress {
                        IngressEvent::Head(head) => NodeIngressEvent {
                            status: Some(head.status),
                            fields: Some(fields(head.fields)),
                            ..event("head")
                        },
                        IngressEvent::RequestPull => event("request-pull"),
                        IngressEvent::Response(IngressResponseFrame::Data(bytes)) => {
                            NodeIngressEvent {
                                data: Some(bytes.into()),
                                ..event("data")
                            }
                        }
                        IngressEvent::Response(IngressResponseFrame::Trailers(trailers)) => {
                            NodeIngressEvent {
                                fields: Some(fields(trailers)),
                                ..event("trailers")
                            }
                        }
                        IngressEvent::Response(IngressResponseFrame::End) => event("end"),
                        IngressEvent::Response(IngressResponseFrame::Error(message)) => {
                            NodeIngressEvent {
                                message: Some(message),
                                ..event("error")
                            }
                        }
                    },
                    Ok(PortEvent::Complete(result)) => {
                        replies.lock().expect("response map").clear();
                        if delivering {
                            let _ = callback.call(
                                NodeIngressEvent {
                                    result: Some(envelope(result).to_string()),
                                    ..event("complete")
                                },
                                ThreadsafeFunctionCallMode::NonBlocking,
                            );
                        }
                        break;
                    }
                    Err(error) => {
                        replies.lock().expect("response map").clear();
                        if delivering {
                            let _ = callback.call(
                                NodeIngressEvent {
                                    result: Some(envelope::<String>(Err(error)).to_string()),
                                    ..event("complete")
                                },
                                ThreadsafeFunctionCallMode::NonBlocking,
                            );
                        }
                        break;
                    }
                };
                if !delivering {
                    continue;
                }
                if callback.call(next, ThreadsafeFunctionCallMode::NonBlocking) != Status::Ok {
                    delivering = false;
                    carrier.disconnect();
                    for reply in
                        std::mem::take(&mut *replies.lock().expect("response map")).into_values()
                    {
                        refuse(reply);
                    }
                }
            }
        })
}
