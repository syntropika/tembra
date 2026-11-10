//! Adapt upstream streams and futures with bounded backpressure.
//!
//! Output streams buffer raw bytes (`stream<u8>`) or whole encoded items (typed streams) up to
//! `PIPE_BYTES`; the upstream producer pauses while the guest has not read. Input byte streams
//! buffer guest chunks in an [`Inbox`] drained by a [`Feed`] producer that upstream consumes.
use super::{PIPE_BYTES, Shared, State, Writer, handles::FilePermit};
use std::{
    collections::VecDeque,
    io::Read,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll, Waker},
};
use wasmtime::{
    AsContextMut, StoreContextMut,
    component::{
        Destination, FutureConsumer, Source, StreamConsumer, StreamProducer, StreamResult,
    },
};

/// Output moved from an upstream stream to the guest reader with bounded buffering.
#[derive(Default)]
pub(super) struct Pipe {
    /// Raw bytes of a `stream<u8>`.
    buffer: Vec<u8>,
    /// Encoded items of a typed stream, and their total size.
    items: VecDeque<Vec<u8>>,
    item_bytes: usize,
    ended: bool,
    /// Set when a read drained data the guest never received: every later read fails rather
    /// than silently skipping the lost bytes or items.
    failure: Option<String>,
    closed: bool,
    producer: Option<Waker>,
    /// Admitted STREAM_READs in guest order, each with its parked waker; only the front drains.
    readers: VecDeque<(u64, Option<Waker>)>,
    /// Next read ticket.
    tickets: u64,
}

/// Error of every read after an earlier read's reply was abandoned undelivered.
pub(super) const ABANDONED: &str = "stream read was abandoned";

impl Pipe {
    /// The guest dropped its handle: stop the producer and settle any pending read now.
    pub(super) fn close(&mut self) {
        self.closed = true;
        self.buffer.clear();
        self.items.clear();
        self.item_bytes = 0;
        if let Some(producer) = self.producer.take() {
            producer.wake();
        }
        self.wake_readers();
    }

    /// A completed read's reply never reached the guest. Its data is gone, so the stream is
    /// poisoned instead of skipping it, and the physical producer is stopped like a DROP; the
    /// guest still names the handle and releases it with DROP.
    pub(super) fn abandon(&mut self) {
        self.failure.get_or_insert_with(|| ABANDONED.into());
        self.close();
    }

    /// Wake every queued reader so each observes DROP, poison or its turn.
    fn wake_readers(&mut self) {
        for (_, waker) in &mut self.readers {
            if let Some(waker) = waker.take() {
                waker.wake();
            }
        }
    }

    /// Wake the read at the front of the queue, the only one that may drain new data.
    fn wake_head(&mut self) {
        if let Some(waker) = self.readers.front_mut().and_then(|(_, waker)| waker.take()) {
            waker.wake();
        }
    }

    /// Stand in for an upstream byte producer in tests: buffer `bytes` and wake the head read.
    #[cfg(test)]
    pub(super) fn produce(&mut self, bytes: &[u8]) {
        self.buffer.extend_from_slice(bytes);
        self.wake_head();
    }

    /// Bytes or encoded items waiting for the guest reader.
    #[cfg(test)]
    pub(super) fn buffered(&self) -> usize {
        self.buffer.len() + self.item_bytes
    }

    fn end(&mut self) {
        self.ended = true;
        self.wake_readers();
    }
}

/// One admitted STREAM_READ's place in its stream's FIFO, claimed at admission so reads are
/// served in guest order like the browser host. Dropping it (completion, or an aborted task
/// that was never polled) leaves the queue and hands the turn to the next read.
pub(super) struct ReadTurn {
    pipe: Shared<Pipe>,
    ticket: u64,
}

impl ReadTurn {
    /// Queue a read behind every earlier pending read of the same stream.
    pub(super) fn begin(pipe: Shared<Pipe>) -> Self {
        let mut state = pipe.lock().expect("pipe");
        let ticket = state.tickets;
        state.tickets += 1;
        state.readers.push_back((ticket, None));
        drop(state);
        Self { pipe, ticket }
    }
}

impl Drop for ReadTurn {
    fn drop(&mut self) {
        let mut pipe = self.pipe.lock().expect("pipe");
        let head = pipe.readers.front().is_some_and(|(t, _)| *t == self.ticket);
        pipe.readers.retain(|(t, _)| *t != self.ticket);
        if head {
            pipe.wake_head();
        }
    }
}

/// Upstream consumer side of a byte [`Pipe`].
/// The permit is the stream's share of the open-file budget while upstream holds the file.
pub(super) struct PipeConsumer(
    pub(super) Shared<Pipe>,
    #[allow(dead_code, reason = "held for its drop")] pub(super) Option<FilePermit>,
);

impl StreamConsumer<State> for PipeConsumer {
    type Item = u8;

    fn poll_consume(
        self: Pin<&mut Self>,
        context: &mut Context<'_>,
        store: StoreContextMut<'_, State>,
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

/// Upstream consumer of a typed stream; each item is encoded by its generated WIT encoder.
pub(super) struct ItemConsumer<T, F> {
    pipe: Shared<Pipe>,
    _file: Option<FilePermit>,
    encode: F,
    _item: std::marker::PhantomData<fn(T)>,
}

impl<T, F> ItemConsumer<T, F> {
    /// Bind a generated encoder to one typed output stream.
    pub(super) fn new(pipe: Shared<Pipe>, file: Option<FilePermit>, encode: F) -> Self {
        Self {
            pipe,
            _file: file,
            encode,
            _item: std::marker::PhantomData,
        }
    }
}

impl<T, F> StreamConsumer<State> for ItemConsumer<T, F>
where
    T: wasmtime::component::Lift + Send + Sync + 'static,
    F: FnMut(&mut Writer, T) -> wasmtime::Result<()> + Send + Unpin + 'static,
{
    type Item = T;

    fn poll_consume(
        mut self: Pin<&mut Self>,
        context: &mut Context<'_>,
        mut store: StoreContextMut<'_, State>,
        mut source: Source<'_, T>,
        finish: bool,
    ) -> Poll<wasmtime::Result<StreamResult>> {
        let this = &mut *self;
        let mut pipe = this.pipe.lock().expect("pipe");
        if pipe.closed {
            return Poll::Ready(Ok(StreamResult::Dropped));
        }
        if pipe.item_bytes < PIPE_BYTES {
            let mut value = None;
            source.read(store.as_context_mut(), &mut value)?;
            if let Some(value) = value {
                let mut writer = Writer::default();
                (this.encode)(&mut writer, value)?;
                pipe.item_bytes += writer.0.len();
                pipe.items.push_back(writer.0);
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

impl<T, F> Drop for ItemConsumer<T, F> {
    fn drop(&mut self) {
        self.pipe.lock().expect("pipe").end();
    }
}

/// An encoded future value waiting for the guest reader.
#[derive(Default)]
pub(super) struct Pending {
    value: Option<Result<Vec<u8>, String>>,
    reader: Option<Waker>,
}

/// Upstream consumer that encodes the single future value with its generated WIT encoder.
pub(super) struct Slot<T, F> {
    pending: Shared<Pending>,
    encode: F,
    _value: std::marker::PhantomData<fn(T)>,
}

impl<T, F> Slot<T, F> {
    /// Bind a generated encoder to one standard future.
    pub(super) fn new(pending: Shared<Pending>, encode: F) -> Self {
        Self {
            pending,
            encode,
            _value: std::marker::PhantomData,
        }
    }
}

impl<T, F> FutureConsumer<State> for Slot<T, F>
where
    T: wasmtime::component::Lift + Send + Sync + 'static,
    F: FnMut(&mut Writer, T) -> wasmtime::Result<()> + Send + Unpin + 'static,
{
    type Item = T;

    fn poll_consume(
        mut self: Pin<&mut Self>,
        _: &mut Context<'_>,
        mut store: StoreContextMut<'_, State>,
        mut source: Source<'_, T>,
        finish: bool,
    ) -> Poll<wasmtime::Result<()>> {
        let mut value = None;
        source.read(store.as_context_mut(), &mut value)?;
        if let Some(value) = value {
            let mut writer = Writer::default();
            let encoded = (self.encode)(&mut writer, value).map(|()| writer.0);
            debug_assert!(
                store.data().standard.handles.issued.is_empty(),
                "future payload handles need a publication owner"
            );
            if let Err(error) = &encoded {
                store.data_mut().standard.trapped = Some(format!("{error:#}"));
            }
            let mut pending = self.pending.lock().expect("future");
            pending.value = Some(encoded.map_err(|error| format!("{error:#}")));
            if let Some(reader) = pending.reader.take() {
                reader.wake();
            }
            Poll::Ready(Ok(()))
        } else if finish {
            Poll::Ready(Ok(()))
        } else {
            Poll::Pending
        }
    }
}

impl<T, F> Drop for Slot<T, F> {
    fn drop(&mut self) {
        let mut pending = self.pending.lock().expect("future");
        if pending.value.is_none() {
            pending.value = Some(Err("future was dropped by its producer".into()));
        }
        if let Some(reader) = pending.reader.take() {
            reader.wake();
        }
    }
}

/// Read one reply: `u8 0` + chunk, or `u8 1` at end of stream. A byte chunk is `u32 length` +
/// up to `limit` bytes; a typed chunk is `u32 count` + up to `limit` whole encoded items whose
/// reply fits `room` bytes.
/// Reads of one stream are served FIFO by their [`ReadTurn`]; no lock is held across a wait.
pub(super) async fn read_stream(
    turn: ReadTurn,
    limit: usize,
    room: usize,
) -> Result<Vec<u8>, String> {
    std::future::poll_fn(|context| {
        let mut pipe = turn.pipe.lock().expect("pipe");
        if let Some(failure) = &pipe.failure {
            return Poll::Ready(Err(failure.clone()));
        }
        if pipe.closed {
            return Poll::Ready(Err("stream was dropped".into()));
        }
        if pipe.readers.front().map(|(t, _)| *t) != Some(turn.ticket) {
            // Queued behind an earlier read; its turn's drop wakes this one.
            if let Some((_, waker)) = pipe.readers.iter_mut().find(|(t, _)| *t == turn.ticket) {
                *waker = Some(context.waker().clone());
            }
            return Poll::Pending;
        }
        let mut writer = Writer::default();
        if !pipe.buffer.is_empty() {
            let count = pipe
                .buffer
                .len()
                .min(limit)
                .min(room.saturating_sub(5).max(1));
            writer.u8(0);
            writer.bytes(&pipe.buffer[..count]);
            pipe.buffer.drain(..count);
        } else if let Some(first) = pipe.items.front() {
            if 5 + first.len() > room {
                return Poll::Ready(Err("stream item exceeds the packet limit".into()));
            }
            writer.u8(0);
            writer.u32(0);
            let mut count = 0u32;
            while (count as usize) < limit
                && let Some(item) = pipe.items.front()
                && writer.0.len() + item.len() <= room
            {
                let item = pipe.items.pop_front().expect("front");
                pipe.item_bytes -= item.len();
                writer.raw(&item);
                count += 1;
            }
            writer.0[1..5].copy_from_slice(&count.to_le_bytes());
        } else if pipe.ended {
            return Poll::Ready(Ok(vec![1]));
        } else {
            if let Some((_, waker)) = pipe.readers.front_mut() {
                *waker = Some(context.waker().clone());
            }
            return Poll::Pending;
        }
        if let Some(producer) = pipe.producer.take() {
            producer.wake();
        }
        Poll::Ready(Ok(writer.0))
    })
    .await
}

/// Accepted-but-unconsumed input bytes of one revision, across all its input streams.
pub(super) const INPUT_BYTES: usize = 1 << 20;

/// Revision-wide accounting of accepted input bytes; writers wait (never allocate) past it.
#[derive(Default)]
pub(super) struct InputBudget {
    used: usize,
    waiters: Vec<Waker>,
}

impl InputBudget {
    /// Accepted bytes currently buffered by the revision.
    #[cfg(test)]
    pub(super) fn used(&self) -> usize {
        self.used
    }

    /// Park a writer once; a writer polled again while waiting does not add another waker.
    fn wait(&mut self, waker: &Waker) {
        if !self.waiters.iter().any(|known| known.will_wake(waker)) {
            self.waiters.push(waker.clone());
        }
    }

    fn release(&mut self, count: usize) {
        self.used -= count;
        for waiter in self.waiters.drain(..) {
            waiter.wake();
        }
    }
}

/// Guest chunks waiting for the upstream consumer of an input byte stream.
pub(super) struct Inbox {
    /// At most `PIPE_BYTES`; every byte is also counted in the revision's `InputBudget`.
    buffer: Vec<u8>,
    budget: Shared<InputBudget>,
    /// The readable end was handed to upstream; it cannot be transferred again.
    pub(super) transferred: bool,
    /// The guest dropped its writer: end of stream once `buffer` drains.
    closed: bool,
    /// Upstream dropped the readable end; further writes fail.
    reader_gone: bool,
    /// A STREAM_WRITE is pending; writes are serialized to keep their order.
    writing: bool,
    producer: Option<Waker>,
    writer: Option<Waker>,
}

impl Inbox {
    /// A writer drawing on its revision's input budget.
    pub(super) fn new(budget: Shared<InputBudget>) -> Self {
        Self {
            buffer: Vec::new(),
            budget,
            transferred: false,
            closed: false,
            reader_gone: false,
            writing: false,
            producer: None,
            writer: None,
        }
    }

    /// Accepted bytes not yet consumed by upstream.
    pub(super) fn buffered(&self) -> usize {
        self.buffer.len()
    }

    /// Forget bytes nobody can consume and return them to the revision budget.
    fn discard(&mut self) {
        let count = self.buffer.len();
        self.buffer = Vec::new();
        self.budget.lock().expect("input budget").release(count);
    }

    /// The writer closed: a transferred stream still delivers what was accepted and then ends;
    /// bytes of a never-transferred stream have no reader and are discarded now.
    pub(super) fn close(&mut self) {
        self.closed = true;
        if !self.transferred {
            self.discard();
        }
        if let Some(producer) = self.producer.take() {
            producer.wake();
        }
        if let Some(writer) = self.writer.take() {
            writer.wake();
        }
    }
}

/// Upstream-facing producer of an input byte stream, holding its share of the file budget.
pub(super) struct Feed(
    pub(super) Shared<Inbox>,
    #[allow(dead_code, reason = "held for its drop")] pub(super) Option<FilePermit>,
);

impl StreamProducer<State> for Feed {
    type Item = u8;
    type Buffer = Option<u8>;

    fn poll_produce<'a>(
        self: Pin<&mut Self>,
        context: &mut Context<'_>,
        store: StoreContextMut<'a, State>,
        destination: Destination<'a, u8, Option<u8>>,
        finish: bool,
    ) -> Poll<wasmtime::Result<StreamResult>> {
        let mut inbox = self.0.lock().expect("inbox");
        if !inbox.buffer.is_empty() {
            let mut destination = destination.as_direct(store, inbox.buffer.len());
            let room = destination.remaining()?;
            let count = room.len().min(inbox.buffer.len());
            room[..count].copy_from_slice(&inbox.buffer[..count]);
            destination.mark_written(count);
            inbox.buffer.drain(..count);
            inbox.budget.lock().expect("input budget").release(count);
            if let Some(writer) = inbox.writer.take() {
                writer.wake();
            }
            return Poll::Ready(Ok(StreamResult::Completed));
        }
        if inbox.closed {
            return Poll::Ready(Ok(StreamResult::Dropped));
        }
        if finish {
            return Poll::Ready(Ok(StreamResult::Cancelled));
        }
        inbox.producer = Some(context.waker().clone());
        Poll::Pending
    }
}

impl Drop for Feed {
    fn drop(&mut self) {
        let mut inbox = self.0.lock().expect("inbox");
        inbox.reader_gone = true;
        inbox.discard();
        if let Some(writer) = inbox.writer.take() {
            writer.wake();
        }
    }
}

/// The one pending write of an input stream; created before its task so an aborted, never
/// polled task still releases the slot.
pub(super) struct Writing(Shared<Inbox>);

impl Writing {
    /// Claim the stream's single write slot.
    pub(super) fn begin(inbox: Shared<Inbox>) -> Result<Self, &'static str> {
        let mut state = inbox.lock().expect("inbox");
        if state.writing {
            return Err("a stream write is already pending");
        }
        state.writing = true;
        drop(state);
        Ok(Self(inbox))
    }

    /// Accept the whole chunk (at most `PIPE_BYTES`, checked at admission) once both the
    /// stream's buffer and the revision's input budget have room; it is never split.
    pub(super) async fn write(self, chunk: Vec<u8>) -> Result<Vec<u8>, String> {
        let mut chunk = Some(chunk);
        std::future::poll_fn(|context| {
            let mut inbox = self.0.lock().expect("inbox");
            if inbox.closed {
                return Poll::Ready(Err("stream was dropped".into()));
            }
            if inbox.reader_gone {
                return Poll::Ready(Err("stream reader was dropped".into()));
            }
            let length = chunk.as_ref().map_or(0, Vec::len);
            let budget = Arc::clone(&inbox.budget);
            let mut budget = budget.lock().expect("input budget");
            if inbox.buffer.len() + length <= PIPE_BYTES && budget.used + length <= INPUT_BYTES {
                budget.used += length;
                drop(budget);
                inbox.buffer.extend(chunk.take().unwrap_or_default());
                if let Some(producer) = inbox.producer.take() {
                    producer.wake();
                }
                return Poll::Ready(Ok(Vec::new()));
            }
            budget.wait(context.waker());
            drop(budget);
            inbox.writer = Some(context.waker().clone());
            Poll::Pending
        })
        .await
    }
}

impl Drop for Inbox {
    fn drop(&mut self) {
        if !self.buffer.is_empty() {
            self.discard();
        }
    }
}

impl Drop for Writing {
    fn drop(&mut self) {
        self.0.lock().expect("inbox").writing = false;
    }
}

/// Wait for the encoded future value.
pub(super) async fn read_future(pending: Shared<Pending>) -> Result<Vec<u8>, String> {
    std::future::poll_fn(|context| {
        let mut pending = pending.lock().expect("future");
        match pending.value.take() {
            Some(value) => Poll::Ready(value),
            None => {
                pending.reader = Some(context.waker().clone());
                Poll::Pending
            }
        }
    })
    .await
}
