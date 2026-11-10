//! Host side of a compiled export's byte-stream result.
//!
//! The host creates the pipe before the call and hands it to the Store with the command. The
//! Store pipes the guest's stream into it exactly once and signals the head. The caller then
//! receives the stream end at once, while the rest of the same dispatch (the revision's
//! admission, its busy state and callback scope, and the host requests of the call's task
//! group) becomes the call's physical work. The managed call drives that work to its end under
//! the call's own invocation; it never travels with the stream end.
//!
//! A clean end is reported only once that work has succeeded; a trap, timeout or failed
//! dispatch fails the read instead. Closing the end closes the pipe, so the guest's next write
//! sees it dropped; a producer that never writes again ends only at the call's timeout. A
//! caller refusing the head is also a close, observed by the producer at its next write.
use super::*;
use crate::worker::{Pipe, StreamCall};
use futures::{FutureExt, channel::oneshot, future::Either};
use std::{
    future::Future,
    sync::{Arc, Mutex},
    task::{Context as Task, Poll, Waker},
};
use tembra::portable::{ByteSource, ByteStream, Outcome, Payload};

impl Environment {
    /// Serve one export whose whole result is a `stream<u8>`.
    pub(crate) async fn invoke_stream(
        &self,
        context: Context,
        method: &str,
        slot: u32,
        payload: Payload,
    ) -> Result<Outcome> {
        let args = self
            .contract
            .to_resource_wire(slot, true, payload.into_data()?)
            .map_err(failure)?;
        let context = context.enter_execution()?;
        let cancellation = context.signal();
        let pipe = Arc::<Mutex<Pipe>>::default();
        let (head, published) = oneshot::channel();
        let carrier = Carrier::Stream(StreamCall {
            pipe: pipe.clone(),
            head,
        });
        let environment = self.clone();
        let method = method.to_owned();
        let mut dispatch = async move {
            let packet = GuestCommand::Invoke {
                environment: environment.id,
                service: environment.service.as_deref(),
                method: &method,
                args: &args,
                receiver: None,
            };
            environment
                .execute(Scope::Call(context), Some(cancellation), packet, carrier)
                .await
        }
        .boxed_local();
        // Drive the call until its head. It stays admitted throughout: the same future goes on.
        match futures::future::select(published, &mut dispatch).await {
            Either::Left((Ok(()), _)) => {}
            Either::Left((Err(_), _)) => return Err(no_stream(dispatch.await)),
            Either::Right((result, _)) => return Err(no_stream(result)),
        }
        let settlement = Rc::new(RefCell::new(Settlement::default()));
        let mut head = Payload::data(Value::Null);
        let index = head.attach_stream(ByteStream::new(Source {
            pipe,
            settlement: settlement.clone(),
        }));
        let head = head.map_value(|_| Ok(serde_json::json!(index)));
        let guard = Settle(Some(settlement));
        Ok(Outcome::continuing(head, async move {
            let result = dispatch.await.map(drop);
            guard.finish(result.clone());
            result
        }))
    }
}

/// The call ended before it published a stream.
fn no_stream(result: Result<Value>) -> tembra::Error {
    result
        .err()
        .unwrap_or_else(|| failure("the stream export returned no stream"))
}

/// The physical outcome of a stream's call, and who waits for it.
#[derive(Default)]
struct Settlement {
    outcome: Option<Result<()>>,
    reader: Option<Waker>,
    acknowledgement: Option<Waker>,
}

/// Record the first outcome and wake a waiting read and close.
fn settle(cell: &RefCell<Settlement>, outcome: Result<()>) {
    let (reader, acknowledgement) = {
        let mut settlement = cell.borrow_mut();
        settlement.outcome.get_or_insert(outcome);
        (settlement.reader.take(), settlement.acknowledgement.take())
    };
    reader
        .into_iter()
        .chain(acknowledgement)
        .for_each(Waker::wake);
}

/// The physical work's drop guard: whether the work returns, fails, panics or is dropped, the
/// stream's reads and its close acknowledgement are resolved.
struct Settle(Option<Rc<RefCell<Settlement>>>);

impl Settle {
    fn finish(mut self, outcome: Result<()>) {
        if let Some(settlement) = self.0.take() {
            settle(&settlement, outcome);
        }
    }
}

impl Drop for Settle {
    fn drop(&mut self) {
        if let Some(settlement) = self.0.take() {
            settle(
                &settlement,
                Err(failure("the stream's producer stopped before it settled")),
            );
        }
    }
}

/// The stream end's view of the call: the bounded pipe the Store fills, and the call's outcome.
struct Source {
    pipe: Arc<Mutex<Pipe>>,
    settlement: Rc<RefCell<Settlement>>,
}

impl ByteSource for Source {
    fn poll_next(&mut self, context: &mut Task<'_>) -> Poll<Option<Result<Vec<u8>>>> {
        if let Some(Err(error)) = &self.settlement.borrow().outcome {
            return Poll::Ready(Some(Err(error.clone())));
        }
        let next = {
            let mut pipe = self.pipe.lock().expect("pipe");
            if let Some(reason) = pipe.failure() {
                return Poll::Ready(Some(Err(failure(reason))));
            }
            // Everything buffered at once: the pipe itself bounds it to its byte capacity.
            pipe.poll_bytes(context, usize::MAX)
        };
        if let Poll::Ready(Some(chunk)) = next {
            return Poll::Ready(Some(Ok(chunk)));
        }
        let mut settlement = self.settlement.borrow_mut();
        match &settlement.outcome {
            // The body's end is reported only after the call physically succeeded.
            Some(Ok(())) if next.is_ready() => Poll::Ready(None),
            Some(Ok(())) => Poll::Ready(Some(Err(failure(
                "the stream's call ended while its writer was still open",
            )))),
            Some(Err(error)) => Poll::Ready(Some(Err(error.clone()))),
            None => {
                settlement.reader = Some(context.waker().clone());
                Poll::Pending
            }
        }
    }

    fn close(&mut self) {
        self.pipe.lock().expect("pipe").close();
    }

    fn poll_closed(&mut self, context: &mut Task<'_>) -> Poll<Result<()>> {
        let mut settlement = self.settlement.borrow_mut();
        match &settlement.outcome {
            Some(outcome) => Poll::Ready(outcome.clone()),
            None => {
                settlement.acknowledgement = Some(context.waker().clone());
                Poll::Pending
            }
        }
    }
}

/// Copy a delivered stream end into a compiled importer's pipe.
///
/// The importer callback's dispatch drives this, so the end, which is not `Send`, stays on the
/// host thread, and the pipe is the only thing the importer's Store reads. Each step moves at
/// most the pipe's free room, so the importer never buffers more than the pipe holds, plus one
/// chunk here. The pump ends:
///
/// - at the stream's end, which the end reports only after its producer physically succeeded;
///   the guest reads the end after the data;
/// - at a failure or cancellation of the end, or when the importer callback's epoch `ended`;
///   the pipe is failed, so the guest's reader sees its writer dropped. A plain `stream<u8>`
///   carries no error, so that guest cannot tell this from an end; the cause stays recorded on
///   the host;
/// - when the guest closed its reader, abandoned the import, or its Store stopped: the pipe is
///   closed, and dropping the end here asks the producer to stop, once.
pub(crate) async fn pump(
    mut stream: ByteStream,
    pipe: Arc<Mutex<Pipe>>,
    ended: impl Future<Output = ()>,
) {
    use futures::StreamExt;
    let copying = async {
        let mut ended = std::pin::pin!(ended);
        let (mut chunk, mut offset) = (Vec::new(), 0);
        loop {
            if offset == chunk.len() {
                let next = futures::future::select(stream.next(), ended.as_mut()).await;
                match next {
                    Either::Left((Some(Ok(next)), _)) => (chunk, offset) = (next, 0),
                    Either::Left((None, _)) => {
                        pipe.lock().expect("pipe").finish();
                        // The guest may retain an unread tail after its producer finished.
                        // Keep observing this callback until it closes the reader or ends.
                        ended.await;
                        return ended_call(&pipe);
                    }
                    Either::Left((Some(Err(error)), _)) => {
                        return pipe.lock().expect("pipe").fail(error.message);
                    }
                    Either::Right(_) => return ended_call(&pipe),
                }
                continue;
            }
            let room = futures::future::poll_fn(|context| {
                pipe.lock().expect("pipe").poll_room(context, false)
            });
            let room = match futures::future::select(room, ended.as_mut()).await {
                Either::Left((room, _)) => room,
                Either::Right(_) => return ended_call(&pipe),
            };
            let Some(room) = room else { return };
            let count = room.min(chunk.len() - offset);
            pipe.lock()
                .expect("pipe")
                .push_bytes(&chunk[offset..offset + count]);
            offset += count;
        }
    };
    // A quiet producer may never wake its next read. Observe the consumer independently so
    // dropping its reader asks the producer to close without waiting for callback expiry.
    let closed =
        futures::future::poll_fn(|context| pipe.lock().expect("pipe").poll_closed(context));
    futures::pin_mut!(closed, copying);
    let _ = futures::future::select(closed, copying).await;
}

/// The importer callback ended while its stream was open: the guest's reader sees it end.
fn ended_call(pipe: &Mutex<Pipe>) {
    pipe.lock()
        .expect("pipe")
        .fail("the stream's receiving call ended".into());
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        sync::atomic::{AtomicUsize, Ordering},
        task::Wake,
    };

    #[derive(Default)]
    struct Wakes(AtomicUsize);

    impl Wake for Wakes {
        fn wake(self: Arc<Self>) {
            self.wake_by_ref();
        }

        fn wake_by_ref(self: &Arc<Self>) {
            self.0.fetch_add(1, Ordering::Relaxed);
        }
    }

    #[test]
    fn a_closed_importer_reader_stops_a_pump_waiting_for_its_producer() {
        use std::cell::Cell;

        struct Quiet(Rc<Cell<usize>>);
        impl ByteSource for Quiet {
            fn poll_next(&mut self, _: &mut Task<'_>) -> Poll<Option<Result<Vec<u8>>>> {
                Poll::Pending
            }

            fn close(&mut self) {
                self.0.set(self.0.get() + 1);
            }

            fn poll_closed(&mut self, _: &mut Task<'_>) -> Poll<Result<()>> {
                Poll::Ready(Ok(()))
            }
        }

        for already_closed in [false, true] {
            let closed = Rc::new(Cell::new(0));
            let pipe = Arc::<Mutex<Pipe>>::default();
            if already_closed {
                pipe.lock().unwrap().close();
            }
            let mut pumping = std::pin::pin!(pump(
                ByteStream::new(Quiet(closed.clone())),
                pipe.clone(),
                futures::future::pending(),
            ));
            let wakes = Arc::<Wakes>::default();
            let waker = Waker::from(wakes.clone());
            let mut context = Task::from_waker(&waker);
            if !already_closed {
                assert!(pumping.as_mut().poll(&mut context).is_pending());
                assert_eq!(closed.get(), 0);

                // The callback stays alive while its quiet producer's reader is dropped.
                pipe.lock().unwrap().close();
                assert_eq!(wakes.0.load(Ordering::Relaxed), 1);
            }
            assert!(pumping.as_mut().poll(&mut context).is_ready());
            assert_eq!(closed.get(), 1);
        }
    }

    #[test]
    fn callback_end_discards_an_unread_tail_even_after_the_producer_finished() {
        struct Finished(Option<Vec<u8>>);
        impl ByteSource for Finished {
            fn poll_next(&mut self, _: &mut Task<'_>) -> Poll<Option<Result<Vec<u8>>>> {
                Poll::Ready(self.0.take().map(Ok))
            }

            fn close(&mut self) {}

            fn poll_closed(&mut self, _: &mut Task<'_>) -> Poll<Result<()>> {
                Poll::Ready(Ok(()))
            }
        }

        let pipe = Arc::<Mutex<Pipe>>::default();
        let (end, ended) = futures::channel::oneshot::channel::<()>();
        let mut pumping = std::pin::pin!(pump(
            ByteStream::new(Finished(Some(vec![1, 2, 3, 4]))),
            pipe.clone(),
            async { ended.await.unwrap() },
        ));
        let waker = Waker::noop();
        let mut context = Task::from_waker(waker);
        assert!(pumping.as_mut().poll(&mut context).is_pending());
        // The producer already ended; consume only a prefix of its fully buffered body.
        assert_eq!(
            pipe.lock().unwrap().poll_bytes(&mut context, 1),
            Poll::Ready(Some(vec![1]))
        );
        end.send(()).unwrap();
        assert!(pumping.as_mut().poll(&mut context).is_ready());
        let mut pipe = pipe.lock().unwrap();
        assert_eq!(pipe.poll_bytes(&mut context, usize::MAX), Poll::Ready(None));
        assert_eq!(pipe.failure(), Some("the stream's receiving call ended"));
    }

    #[test]
    fn a_dropped_driver_wakes_and_fails_the_read_and_physical_close() {
        let settlement = Rc::new(RefCell::new(Settlement::default()));
        let guard = Settle(Some(settlement.clone()));
        let mut source = Source {
            pipe: Arc::default(),
            settlement,
        };
        let reader = Arc::<Wakes>::default();
        let closing = Arc::<Wakes>::default();
        let reader_waker = Waker::from(reader.clone());
        let close_waker = Waker::from(closing.clone());
        let mut read_context = Task::from_waker(&reader_waker);
        let mut close_context = Task::from_waker(&close_waker);
        assert!(source.poll_next(&mut read_context).is_pending());
        assert!(source.poll_closed(&mut close_context).is_pending());
        drop(guard);
        assert_eq!(reader.0.load(Ordering::Relaxed), 1);
        assert_eq!(closing.0.load(Ordering::Relaxed), 1);
        assert!(matches!(
            source.poll_next(&mut read_context),
            Poll::Ready(Some(Err(_)))
        ));
        assert!(matches!(
            source.poll_closed(&mut close_context),
            Poll::Ready(Err(_))
        ));
    }
}
