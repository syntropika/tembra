//! Bounded host-side buffers between upstream WASI streams and a component guest reader.
//!
//! A byte pipe buffers raw `stream<u8>` bytes up to [`PIPE_BYTES`]; the upstream producer pauses
//! while the guest has not read. An item pipe carries one accepted socket at a time, each with
//! the upstream resource that closes it if nobody reads it.
use std::{
    collections::VecDeque,
    io::Read,
    pin::Pin,
    sync::{Arc, Mutex},
    task::{Context, Poll, Waker},
};
use wasmtime::{
    StoreContextMut,
    component::{Source, StreamConsumer, StreamResult},
};

/// Bytes buffered per stream before the upstream producer is paused.
pub(crate) const PIPE_BYTES: usize = 64 << 10;

pub(super) type Shared<T> = Arc<Mutex<T>>;

/// What releases an unread item: an owned upstream resource of a kind, closed by the Store.
#[derive(Debug)]
pub(crate) enum Close {
    Resource(u32, u32),
}

/// Output moved from an upstream stream to the guest reader with bounded buffering.
#[derive(Default)]
pub(crate) struct Pipe {
    /// Raw bytes of a `stream<u8>`.
    buffer: Vec<u8>,
    /// Encoded items of a typed stream, each with what releases it if nobody reads it (the
    /// handles it issued, or the upstream resource it carries), and their total size.
    items: VecDeque<(Vec<u8>, Vec<Close>)>,
    item_bytes: usize,
    /// Close queue for unread items' handles or resources; set for item streams whose items
    /// carry owned handles.
    closing: Option<Shared<Vec<Close>>>,
    /// The revision's closing-pump ledger, while an adapter pump feeds this stream.
    pump: Option<crate::http::Ledger>,
    /// Held from the close of a pumped stream until its pump physically finished: the adapter's
    /// `close`, any in-flight read, and the pump itself. A cancelled call's unit is held here
    /// too, so the call completes only after that physical close.
    closing_pump: Vec<crate::http::Held>,
    /// The single guest reader of a compiled stream (its host producer), parked for data.
    guest: Option<Waker>,
    /// The owning call's cancellation closed this stream before it was fully delivered, so its
    /// completion reports the cancellation rather than a clean end.
    cancelled: bool,
    ended: bool,
    /// Why every later read fails, if the stream was failed.
    failure: Option<String>,
    /// The stream's producer itself failed, as opposed to a close by the consumer side.
    producer_failed: bool,
    closed: bool,
    producer: Option<Waker>,
    /// A host observer waiting for the reader side to close the stream.
    close_observer: Option<Waker>,
}

impl Pipe {
    /// The guest dropped its handle: stop the producer and settle any pending read now. Handles
    /// issued by unread items are released through the close queue.
    pub(crate) fn close(&mut self) {
        if !self.closed
            && let Some(pump) = &self.pump
        {
            self.closing_pump.push(pump.hold());
        }
        self.closed = true;
        self.buffer.clear();
        let unread: Vec<Close> = self
            .items
            .drain(..)
            .flat_map(|(_, release)| release)
            .collect();
        self.release(unread);
        self.item_bytes = 0;
        if let Some(producer) = self.producer.take() {
            producer.wake();
        }
        if let Some(observer) = self.close_observer.take() {
            observer.wake();
        }
        self.wake_readers();
    }

    /// Queue handles or resources nobody will receive for release where the Store is held.
    fn release(&self, release: Vec<Close>) {
        if release.is_empty() {
            return;
        }
        let closing = self
            .closing
            .as_ref()
            .expect("handle-bearing items have a close queue");
        closing.lock().expect("closing").extend(release);
    }

    /// Route handles of unread items to `closing`.
    pub(super) fn release_into(&mut self, closing: Shared<Vec<Close>>) {
        self.closing = Some(closing);
    }

    /// An adapter pump starts feeding this stream; a close from now on is counted in `ledger`
    /// until [`Self::pump_finished`].
    pub(super) fn attach_pump(&mut self, ledger: &crate::http::Ledger) {
        self.pump = Some(ledger.clone());
        if self.closed {
            self.closing_pump.push(ledger.hold());
        }
    }

    /// The pump physically finished; the returned units (if closing) are the acknowledgement.
    pub(super) fn pump_finished(&mut self) -> Vec<crate::http::Held> {
        self.pump = None;
        std::mem::take(&mut self.closing_pump)
    }

    /// Whether the owning call's cancellation cut this stream short.
    pub(super) fn cancelled(&self) -> bool {
        self.cancelled
    }

    /// Whether the guest dropped (or abandoned) the stream.
    pub(super) fn closed(&self) -> bool {
        self.closed
    }

    /// Pump side: room for the next chunk, or `None` once the stream is closed. Byte streams
    /// have room up to `PIPE_BYTES`; handle-bearing item streams take the next item only when
    /// the previous one was read, so at most one accepted handle waits for the guest.
    pub(crate) fn poll_room(
        &mut self,
        context: &mut Context<'_>,
        items: bool,
    ) -> Poll<Option<usize>> {
        if self.closed {
            return Poll::Ready(None);
        }
        let room = if items {
            usize::from(self.items.is_empty())
        } else {
            PIPE_BYTES.saturating_sub(self.buffer.len())
        };
        if room > 0 {
            return Poll::Ready(Some(room));
        }
        self.producer = Some(context.waker().clone());
        Poll::Pending
    }

    /// Pump side: ready once the stream is closed.
    pub(crate) fn poll_closed(&mut self, context: &mut Context<'_>) -> Poll<()> {
        if self.closed {
            return Poll::Ready(());
        }
        self.producer = Some(context.waker().clone());
        Poll::Pending
    }

    /// Host observer: ready once the reader side closed the stream. It never takes the
    /// producer's wake-up.
    pub(crate) fn poll_reader_closed(&mut self, context: &mut Context<'_>) -> Poll<()> {
        if self.closed {
            return Poll::Ready(());
        }
        self.close_observer = Some(context.waker().clone());
        Poll::Pending
    }

    /// Pump side: buffer one chunk read from upstream.
    pub(crate) fn push_bytes(&mut self, bytes: &[u8]) {
        if !self.closed {
            self.buffer.extend_from_slice(bytes);
            self.wake_head();
        }
    }

    /// Pump side: buffer one item with what releases it if nobody reads it; released at once
    /// if the stream is already closed.
    pub(super) fn push_owned(&mut self, bytes: Vec<u8>, release: Vec<Close>) {
        if self.closed {
            self.release(release);
            return;
        }
        self.item_bytes += bytes.len();
        self.items.push_back((bytes, release));
        self.wake_head();
    }

    /// Compiled reader: wake the guest reader when a read could complete.
    pub(super) fn park_guest(&mut self, context: &mut Context<'_>) {
        self.guest = Some(context.waker().clone());
    }

    /// Compiled reader: whether a read would complete now (data, an item, end or close).
    pub(super) fn readable(&self) -> bool {
        self.closed || self.ended || !self.buffer.is_empty() || !self.items.is_empty()
    }

    /// Compiled reader: take up to `max` buffered bytes; `None` at end of stream or once closed.
    pub(crate) fn poll_bytes(
        &mut self,
        context: &mut Context<'_>,
        max: usize,
    ) -> Poll<Option<Vec<u8>>> {
        if self.closed {
            return Poll::Ready(None);
        }
        if !self.buffer.is_empty() {
            let count = self.buffer.len().min(max.max(1));
            let chunk = self.buffer.drain(..count).collect();
            if let Some(producer) = self.producer.take() {
                producer.wake();
            }
            return Poll::Ready(Some(chunk));
        }
        if self.ended {
            return Poll::Ready(None);
        }
        self.guest = Some(context.waker().clone());
        Poll::Pending
    }

    /// Compiled reader: take the next item; the caller now owns what its release would close.
    /// `None` at end of stream or once closed.
    pub(super) fn poll_item(&mut self, context: &mut Context<'_>) -> Poll<Option<Vec<u8>>> {
        if self.closed {
            return Poll::Ready(None);
        }
        if let Some((item, _)) = self.items.pop_front() {
            self.item_bytes -= item.len();
            if let Some(producer) = self.producer.take() {
                producer.wake();
            }
            return Poll::Ready(Some(item));
        }
        if self.ended {
            return Poll::Ready(None);
        }
        self.guest = Some(context.waker().clone());
        Poll::Pending
    }

    /// Fail every later read with `message` and stop the producer like a DROP.
    pub(crate) fn fail(&mut self, message: String) {
        self.failure.get_or_insert(message);
        self.close();
    }

    /// Why every read fails, if the stream was failed.
    pub(crate) fn failure(&self) -> Option<&str> {
        self.failure.as_deref()
    }

    /// The producer failed after publishing the stream: a raw `stream<u8>` has no error lane,
    /// so its reader must never observe this as a clean end.
    pub(crate) fn fail_producer(&mut self, message: String) {
        self.producer_failed = true;
        self.fail(message);
    }

    /// The producer's failure, if the stream ended that way.
    pub(crate) fn producer_failure(&self) -> Option<&str> {
        self.failure.as_deref().filter(|_| self.producer_failed)
    }

    /// Wake the guest reader so it observes data, the end, a close or a failure.
    fn wake_readers(&mut self) {
        if let Some(guest) = self.guest.take() {
            guest.wake();
        }
    }

    /// Wake the guest reader for newly buffered data.
    fn wake_head(&mut self) {
        self.wake_readers();
    }

    fn end(&mut self) {
        self.ended = true;
        self.wake_readers();
    }

    /// Pump side: the upstream producer ended cleanly; the reader gets the end after the data.
    pub(crate) fn finish(&mut self) {
        self.end();
    }
}

/// Upstream consumer side of a byte [`Pipe`], for any Store: it accepts at most the pipe's free
/// bytes, then parks the writer until a read makes room or the pipe is closed. Dropping it
/// ends the stream.
pub(crate) struct PipeConsumer(pub(super) Shared<Pipe>);

impl PipeConsumer {
    /// Consume a guest stream into `pipe`.
    pub(crate) fn new(pipe: Shared<Pipe>) -> Self {
        Self(pipe)
    }
}

impl<D: 'static> StreamConsumer<D> for PipeConsumer {
    type Item = u8;

    fn poll_consume(
        self: Pin<&mut Self>,
        context: &mut Context<'_>,
        store: StoreContextMut<'_, D>,
        source: Source<'_, u8>,
        finish: bool,
    ) -> Poll<wasmtime::Result<StreamResult>> {
        let mut pipe = self.0.lock().expect("pipe");
        if pipe.closed {
            return Poll::Ready(Ok(StreamResult::Dropped));
        }
        let room = PIPE_BYTES.saturating_sub(pipe.buffer.len());
        if room > 0 {
            let mut chunk = vec![0; room];
            let read = source.as_direct(store).read(&mut chunk)?;
            if read > 0 {
                pipe.buffer.extend_from_slice(&chunk[..read]);
                pipe.wake_head();
                return Poll::Ready(Ok(StreamResult::Completed));
            }
        }
        if finish {
            return Poll::Ready(Ok(StreamResult::Cancelled));
        }
        pipe.producer = Some(context.waker().clone());
        Poll::Pending
    }
}

impl Drop for PipeConsumer {
    fn drop(&mut self) {
        self.0.lock().expect("pipe").end();
    }
}

/// A compiled stream read by a call: the call's cancellation closes it, and the call's unit is
/// held until the stream's pump physically finished (or released now if no pump runs).
impl crate::http::CallOwned for std::sync::Mutex<Pipe> {
    fn cancel(&self, held: crate::http::Held) {
        let mut pipe = self.lock().expect("pipe");
        // Only a close that cuts the stream short is reported; a fully delivered stream keeps
        // its own completion.
        let delivered = pipe.ended && pipe.buffer.is_empty() && pipe.items.is_empty();
        if !pipe.closed && !delivered {
            pipe.cancelled = true;
        }
        pipe.close();
        if pipe.pump.is_some() {
            pipe.closing_pump.push(held);
        }
    }
}
