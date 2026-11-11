//! A host-issued byte stream end at the native portable boundary.
//!
//! An issuing executor wraps its bounded producer in a [`ByteSource`] and returns the end as a
//! payload attachment, together with the call's physical work in an [`super::Outcome`]. The
//! receiving caller decodes one [`ByteStream`]. The end belongs to the root that received it:
//! the root's end closes it and makes it refuse reads, and the root's cancellation fails it
//! with that cancellation at once, including a read that is already waiting. It never carries
//! the producer's physical work: dropping or closing it only asks the producer to stop, and the
//! producer's call keeps settling under its own invocation.
use crate::{Error, Result, core, core::Reason};
use std::{
    cell::RefCell,
    pin::Pin,
    rc::Rc,
    task::{Context, Poll, Waker},
};

/// The producing side of one byte stream, supplied by its issuer.
///
/// A writer learns that the consumer stopped only at its next stream operation; the source
/// cannot cancel an idle producer, which ends at its call's own timeout.
///
/// # Issuer obligations
///
/// - **Termination.** Whenever the issuer's physical work ends, for any reason (return, error,
///   panic, or the work being dropped), the source must resolve both sides: a waiting or later
///   [`poll_next`](ByteSource::poll_next) yields `Err` unless the producer had already ended
///   cleanly, and [`poll_closed`](ByteSource::poll_closed) becomes `Ready`. The framework
///   cannot do this itself: it has no link from the physical work to the source. The issuer
///   needs a drop guard on its physical side for this.
/// - **Failure is never a clean end.** A failed, trapped or timed-out producer yields `Err`,
///   never `None`. The `Err` item is terminal; reads after it yield `None`.
/// - **A clean end is only the body's end.** It does not mean the producer's call succeeded; a
///   later failure reaches `Host::errors` and the result of [`ByteStream::close`]. A close the
///   consumer asked for should settle as `Ok` unless the producer itself failed, since every
///   recorded failure also fails `Host::close`.
/// - **No re-entry.** Every method runs while the stream end is borrowed. A source must not call
///   back into the host synchronously (for example through actions that end or cancel a
///   root); it may only store wakers and buffer state, and wake them.
pub trait ByteSource: 'static {
    /// The next chunk, `None` once the producer ended cleanly, or the producer's failure.
    /// Returns `Pending` with the waker stored when no chunk is buffered.
    fn poll_next(&mut self, context: &mut Context<'_>) -> Poll<Option<Result<Vec<u8>>>>;

    /// The consumer will read no more. Called at most once.
    fn close(&mut self);

    /// Ready once the producer has physically stopped after [`ByteSource::close`] or its own
    /// end, with its final outcome (an error or a timeout of the producer's call, for example).
    fn poll_closed(&mut self, context: &mut Context<'_>) -> Poll<Result<()>>;
}

enum State {
    Open,
    /// The producer ended cleanly or failed; the error was reported once.
    Finished,
    /// The receiving root ended: the end is unusable.
    Expired,
    /// The receiving root was cancelled.
    Cancelled,
}

struct Shared {
    source: Box<dyn ByteSource>,
    state: State,
    /// The close request was given to the source.
    closed: bool,
    reader: Option<Waker>,
}

impl Shared {
    fn close_once(&mut self) {
        if !self.closed {
            self.closed = true;
            self.source.close();
        }
    }
}

/// One end of a host-issued byte stream. It is linear: not `Clone`, and dropping it asks the
/// producer to stop exactly once.
pub struct ByteStream(Rc<RefCell<Shared>>);

impl ByteStream {
    /// Issue a stream end over `source`. Only its first decoded holder can read it.
    pub fn new(source: impl ByteSource) -> Self {
        Self(Rc::new(RefCell::new(Shared {
            source: Box::new(source),
            state: State::Open,
            closed: false,
            reader: None,
        })))
    }

    /// Stop reading and wait until the producer has physically stopped, returning its final
    /// outcome. Nothing is read after the close request.
    pub async fn close(self) -> Result<()> {
        self.0.borrow_mut().close_once();
        let shared = self.0.clone();
        futures::future::poll_fn(move |context| shared.borrow_mut().source.poll_closed(context))
            .await
    }

    pub(crate) fn root_stream(&self) -> std::rc::Weak<dyn RootStream> {
        let shared: Rc<dyn RootStream> = self.0.clone();
        Rc::downgrade(&shared)
    }
}

impl futures::Stream for ByteStream {
    type Item = Result<Vec<u8>>;

    /// Chunks in order, then `None` after a clean end. A cancelled or expired end yields an
    /// error, never a clean end.
    fn poll_next(self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let mut shared = self.0.borrow_mut();
        match shared.state {
            State::Open => {}
            State::Finished => return Poll::Ready(None),
            State::Expired => return Poll::Ready(Some(Err(expired()))),
            State::Cancelled => return Poll::Ready(Some(Err(cancelled()))),
        }
        shared.reader = Some(context.waker().clone());
        let next = shared.source.poll_next(context);
        if matches!(next, Poll::Ready(None | Some(Err(_)))) {
            shared.state = State::Finished;
        }
        next
    }
}

impl Drop for ByteStream {
    fn drop(&mut self) {
        self.0.borrow_mut().close_once();
    }
}

/// Root events reaching a delivered stream end through the host's root registry.
pub(crate) trait RootStream {
    /// The root ended: close the producer and refuse every later read.
    fn end_root(&self);
    /// The root was cancelled: close the producer and fail reads with the cancellation.
    fn cancel_root(&self);
}

impl RootStream for RefCell<Shared> {
    fn end_root(&self) {
        settle(self, State::Expired);
    }

    fn cancel_root(&self) {
        settle(self, State::Cancelled);
    }
}

/// Move an open end into a terminal root state, close its producer and wake a waiting read.
fn settle(cell: &RefCell<Shared>, state: State) {
    let reader = {
        let mut shared = cell.borrow_mut();
        if matches!(shared.state, State::Open) {
            shared.state = state;
        }
        shared.close_once();
        shared.reader.take()
    };
    if let Some(reader) = reader {
        reader.wake();
    }
}

fn expired() -> Error {
    Error::new(
        core::Code::Expired,
        Reason::Root,
        "the stream's root has ended",
    )
}

fn cancelled() -> Error {
    Error::new(
        core::Code::Cancelled,
        Reason::Root,
        "the stream's root was cancelled",
    )
}
