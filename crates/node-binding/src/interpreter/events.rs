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
                Ok(PortEvent::Complete(result))=>{
                    replies.lock().expect("response map").clear();
                    if delivering {let _=callback.call(json!({"event":"complete","result":envelope(result)}).to_string(),ThreadsafeFunctionCallMode::NonBlocking);}
                    break;
                }
                Err(error)=>{
                    replies.lock().expect("response map").clear();
                    if delivering {let _=callback.call(json!({"event":"complete","result":envelope(Err(error))}).to_string(),ThreadsafeFunctionCallMode::NonBlocking);}
                    break;
                }
            }
        }
    })
}
