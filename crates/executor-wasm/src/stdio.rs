//! Standard streams serviced inside the Store's own polls.
//!
//! Upstream's `AsyncStdinStream`/`AsyncStdoutStream` forward through p2 pipes that spawn
//! detached reader/writer tasks. These adapters implement the same public `StdinStream` /
//! `StdoutStream` traits by polling the configured reader or writer directly, so the bytes of
//! a revision's stdio are physically owned by its Store and settle (or stay counted) with it.
//!
//! **One parked poller per source.** Every `read-via-stream`/`write-via-stream` of a Store gets
//! its own view of the same configured reader or writer. A typical `AsyncRead`/`AsyncWrite`
//! keeps a single waker slot, so two views parked on it at once would lose a wakeup. Each view
//! therefore takes a turn (a FIFO [`tokio::sync::Mutex`]) for one operation, keeps it across
//! `Pending` so only its waker is parked on the source, and releases it on `Ready` or drop; the
//! next queued view is then woken. Turns are per operation, never per stream, so an idle
//! stream blocks nobody. `AsyncRead`/`AsyncWrite` carry no cancellation signal: if the guest
//! cancels a pending operation, that view keeps its turn until the same stream is polled again
//! (and completes) or is dropped.
//!
//! **Regular files only.** `Input::File` and `Output::File` must name regular files (an absent
//! output path is created as one). FIFOs, devices, terminals, sockets and directories are refused
//! with an `invalid` error before any blocking open; on Unix the open itself is non-blocking and
//! the opened descriptor is checked again, so a path swapped after the first check is refused
//! too. Regular-file reads and writes stay inline, bounded byte work on the Store's thread like
//! the filesystem adapters: they are not preemptible and can still stall for as long as the
//! underlying filesystem (for example a network mount) takes to answer one bounded call.
use crate::install::{Input, Output};
use std::{
    future::Future,
    io::{Read, Write},
    path::Path,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll, ready},
};
use tembra::Result;
use tokio::{
    io::{AsyncRead, AsyncWrite, ReadBuf},
    sync::{Mutex, OwnedMutexGuard},
};
use wasmtime_wasi::cli::{IsTerminal, StdinStream, StdoutStream};

type Reader = Box<dyn AsyncRead + Send + Sync + Unpin>;
type Writer = Box<dyn AsyncWrite + Send + Sync + Unpin>;
type Acquire<T> = Pin<Box<dyn Future<Output = OwnedMutexGuard<T>> + Send + Sync>>;

/// A view's position for its current operation on the shared source.
enum Turn<T> {
    Idle,
    Queued(Acquire<T>),
    Held(OwnedMutexGuard<T>),
}

/// One view of the standard stream shared by every `read-via-stream`/`write-via-stream` of a
/// Store. Dropping it releases its turn or leaves the queue.
struct Shared<T> {
    source: Arc<Mutex<T>>,
    turn: Turn<T>,
}

impl<T: Send + Sync + 'static> Shared<T> {
    fn new(source: &Arc<Mutex<T>>) -> Self {
        Self {
            source: Arc::clone(source),
            turn: Turn::Idle,
        }
    }

    /// Run one operation on the source while holding this view's turn. The turn is kept while
    /// the operation is `Pending` and released as soon as it is `Ready`.
    fn poll_turn<R>(
        &mut self,
        context: &mut Context<'_>,
        operation: impl FnOnce(&mut T, &mut Context<'_>) -> Poll<R>,
    ) -> Poll<R> {
        loop {
            match &mut self.turn {
                Turn::Held(_) => break,
                Turn::Idle => {
                    // `try_lock_owned` never overtakes a queued view: tokio hands a released
                    // turn directly to the oldest waiter.
                    self.turn = match Arc::clone(&self.source).try_lock_owned() {
                        Ok(guard) => Turn::Held(guard),
                        Err(_) => Turn::Queued(Box::pin(Arc::clone(&self.source).lock_owned())),
                    };
                }
                Turn::Queued(acquire) => {
                    let guard = ready!(acquire.as_mut().poll(context));
                    self.turn = Turn::Held(guard);
                }
            }
        }
        let Turn::Held(guard) = &mut self.turn else {
            unreachable!("the turn is held")
        };
        let result = ready!(operation(guard, context));
        self.turn = Turn::Idle;
        Poll::Ready(result)
    }
}

impl AsyncRead for Shared<Reader> {
    fn poll_read(
        self: Pin<&mut Self>,
        context: &mut Context<'_>,
        buffer: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        self.get_mut().poll_turn(context, |reader, context| {
            Pin::new(&mut **reader).poll_read(context, buffer)
        })
    }
}

impl AsyncWrite for Shared<Writer> {
    fn poll_write(
        self: Pin<&mut Self>,
        context: &mut Context<'_>,
        bytes: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        self.get_mut().poll_turn(context, |writer, context| {
            Pin::new(&mut **writer).poll_write(context, bytes)
        })
    }
    fn poll_flush(self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        self.get_mut().poll_turn(context, |writer, context| {
            Pin::new(&mut **writer).poll_flush(context)
        })
    }
    fn poll_shutdown(self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        self.get_mut().poll_turn(context, |writer, context| {
            Pin::new(&mut **writer).poll_shutdown(context)
        })
    }
}

/// A configured input; never a terminal.
pub(crate) struct Stdin(Arc<Mutex<Reader>>);
/// A configured output; never a terminal.
pub(crate) struct Stdout(Arc<Mutex<Writer>>);

impl IsTerminal for Stdin {
    fn is_terminal(&self) -> bool {
        false
    }
}
impl IsTerminal for Stdout {
    fn is_terminal(&self) -> bool {
        false
    }
}
impl StdinStream for Stdin {
    fn async_stream(&self) -> Box<dyn AsyncRead + Send + Sync> {
        Box::new(Shared::new(&self.0))
    }
}
impl StdoutStream for Stdout {
    fn async_stream(&self) -> Box<dyn AsyncWrite + Send + Sync> {
        Box::new(Shared::new(&self.0))
    }
}

/// A regular host file read or appended inline, one bounded buffer per poll.
struct File(std::fs::File);

impl AsyncRead for File {
    fn poll_read(
        mut self: Pin<&mut Self>,
        _: &mut Context<'_>,
        buffer: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let read = self.0.read(buffer.initialize_unfilled())?;
        buffer.advance(read);
        Poll::Ready(Ok(()))
    }
}

impl AsyncWrite for File {
    fn poll_write(
        mut self: Pin<&mut Self>,
        _: &mut Context<'_>,
        bytes: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        Poll::Ready(self.0.write(bytes))
    }
    fn poll_flush(mut self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Poll::Ready(self.0.flush())
    }
    fn poll_shutdown(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}

fn not_regular(path: &Path) -> tembra::Error {
    tembra::Error::invalid(
        "cli_stdio",
        format!(
            "{}: standard stream files must be regular files (not FIFOs, devices, terminals, sockets or directories)",
            path.display()
        ),
    )
}

/// Whether metadata describes a regular file. Windows: std reports neither named pipes nor
/// character devices separately, so the device attribute is checked as well.
fn regular(metadata: &std::fs::Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        const FILE_ATTRIBUTE_DEVICE: u32 = 0x40;
        if metadata.file_attributes() & FILE_ATTRIBUTE_DEVICE != 0 {
            return false;
        }
    }
    metadata.is_file()
}

/// Open a regular file for one Store without ever blocking on a non-regular one.
///
/// 1. An existing path (symlinks followed) must already be a regular file, so a FIFO is never
///    opened and a device open never has side effects.
/// 2. Unix: the open is `O_NONBLOCK | O_NOCTTY`, so even a FIFO or terminal swapped in after the
///    check cannot block it or become a controlling terminal.
/// 3. The opened descriptor itself must be a regular file (closes the swap race).
/// 4. Unix: `O_NONBLOCK` is cleared again so regular-file I/O keeps ordinary semantics.
///
/// Windows has no public safe non-blocking open: a path swapped to a non-regular object between
/// steps 1 and 3 is still opened (once) before step 3 refuses it.
fn open(path: &Path, output: bool) -> Result<std::fs::File> {
    match std::fs::metadata(path) {
        Ok(metadata) if !regular(&metadata) => return Err(not_regular(path)),
        Ok(_) => {}
        Err(error) if output && error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(crate::failure(error)),
    }
    let mut options = std::fs::OpenOptions::new();
    if output {
        options.create(true).append(true);
    } else {
        options.read(true);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NONBLOCK | libc::O_NOCTTY);
    }
    let file = options.open(path).map_err(crate::failure)?;
    if !regular(&file.metadata().map_err(crate::failure)?) {
        return Err(not_regular(path));
    }
    #[cfg(unix)]
    blocking(&file).map_err(crate::failure)?;
    Ok(file)
}

/// Clear `O_NONBLOCK` on a validated regular file.
#[cfg(unix)]
fn blocking(file: &std::fs::File) -> std::io::Result<()> {
    use std::os::fd::AsRawFd;
    let descriptor = file.as_raw_fd();
    // SAFETY: `descriptor` is owned by `file`, which outlives both calls; F_GETFL/F_SETFL only
    // read and set its status flags.
    let flags = unsafe { libc::fcntl(descriptor, libc::F_GETFL) };
    if flags < 0
        // SAFETY: as above.
        || unsafe { libc::fcntl(descriptor, libc::F_SETFL, flags & !libc::O_NONBLOCK) } < 0
    {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

/// A fresh standard input for one Store; `None` keeps upstream's empty default.
pub(crate) fn stdin(input: &Input) -> Result<Option<Stdin>> {
    let reader: Reader = match input {
        Input::Empty => return Ok(None),
        Input::Bytes(bytes) => Box::new(std::io::Cursor::new(bytes.clone())),
        Input::File(path) => Box::new(File(open(path, false)?)),
        Input::Reader(factory) => factory(),
    };
    Ok(Some(Stdin(Arc::new(Mutex::new(reader)))))
}

/// A fresh standard output or error for one Store; `None` keeps upstream's discarding sink.
pub(crate) fn stdout(output: &Output) -> Result<Option<Stdout>> {
    let writer: Writer = match output {
        Output::Discard => return Ok(None),
        Output::File(path) => Box::new(File(open(path, true)?)),
        Output::Writer(factory) => factory(),
    };
    Ok(Some(Stdout(Arc::new(Mutex::new(writer)))))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        collections::VecDeque,
        sync::atomic::{AtomicUsize, Ordering},
        task::{Wake, Waker},
    };

    /// A waker that counts its wakes.
    #[derive(Default)]
    struct Count(AtomicUsize);
    impl Wake for Count {
        fn wake(self: Arc<Self>) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }
    fn waker() -> (Arc<Count>, Waker) {
        let count = Arc::new(Count::default());
        (count.clone(), Waker::from(count))
    }
    fn wakes(count: &Count) -> usize {
        count.0.load(Ordering::SeqCst)
    }

    /// A deliberately one-waker source and sink: a second parked poller overwrites the first.
    #[derive(Default)]
    struct Gate {
        chunks: VecDeque<Vec<u8>>,
        capacity: usize,
        accepted: Vec<u8>,
        waker: Option<Waker>,
        polls: usize,
    }
    #[derive(Clone, Default)]
    struct OneWaker(Arc<std::sync::Mutex<Gate>>);
    impl OneWaker {
        fn gate(&self) -> std::sync::MutexGuard<'_, Gate> {
            self.0.lock().unwrap()
        }
        /// External progress: wake whichever single waker is parked.
        fn release(&self, change: impl FnOnce(&mut Gate)) {
            let waker = {
                let mut gate = self.gate();
                change(&mut gate);
                gate.waker.take()
            };
            waker.into_iter().for_each(Waker::wake);
        }
    }
    impl AsyncRead for OneWaker {
        fn poll_read(
            self: Pin<&mut Self>,
            context: &mut Context<'_>,
            buffer: &mut ReadBuf<'_>,
        ) -> Poll<std::io::Result<()>> {
            let mut gate = self.gate();
            gate.polls += 1;
            match gate.chunks.pop_front() {
                Some(chunk) => {
                    buffer.put_slice(&chunk);
                    Poll::Ready(Ok(()))
                }
                None => {
                    gate.waker = Some(context.waker().clone());
                    Poll::Pending
                }
            }
        }
    }
    impl AsyncWrite for OneWaker {
        fn poll_write(
            self: Pin<&mut Self>,
            context: &mut Context<'_>,
            bytes: &[u8],
        ) -> Poll<std::io::Result<usize>> {
            let mut gate = self.gate();
            gate.polls += 1;
            if gate.capacity == 0 {
                gate.waker = Some(context.waker().clone());
                return Poll::Pending;
            }
            let accepted = bytes.len().min(gate.capacity);
            gate.capacity -= accepted;
            gate.accepted.extend_from_slice(&bytes[..accepted]);
            Poll::Ready(Ok(accepted))
        }
        fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            Poll::Ready(Ok(()))
        }
        fn poll_shutdown(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    fn input(source: &OneWaker) -> Stdin {
        let source = source.clone();
        stdin(&Input::Reader(Arc::new(move || Box::new(source.clone()))))
            .unwrap()
            .unwrap()
    }
    fn output(sink: &OneWaker) -> Stdout {
        let sink = sink.clone();
        stdout(&Output::Writer(Arc::new(move || Box::new(sink.clone()))))
            .unwrap()
            .unwrap()
    }

    type ReadView = Pin<Box<dyn AsyncRead + Send + Sync>>;
    type WriteView = Pin<Box<dyn AsyncWrite + Send + Sync>>;
    fn reader(stdin: &Stdin) -> ReadView {
        Box::into_pin(stdin.async_stream())
    }
    fn writer(stdout: &Stdout) -> WriteView {
        Box::into_pin(stdout.async_stream())
    }

    /// One read; `Ready` carries the bytes produced.
    fn read(view: &mut ReadView, waker: &Waker) -> Poll<Vec<u8>> {
        let mut bytes = [0; 16];
        let mut buffer = ReadBuf::new(&mut bytes);
        view.as_mut()
            .poll_read(&mut Context::from_waker(waker), &mut buffer)
            .map(|result| {
                result.unwrap();
                buffer.filled().to_vec()
            })
    }
    /// One write; `Ready` carries the accepted length.
    fn write(view: &mut WriteView, waker: &Waker, bytes: &[u8]) -> Poll<usize> {
        view.as_mut()
            .poll_write(&mut Context::from_waker(waker), bytes)
            .map(|result| result.unwrap())
    }

    #[test]
    fn concurrent_input_views_take_turns_without_losing_a_wakeup() {
        let source = OneWaker::default();
        let stdin = input(&source);
        let (mut first, mut second) = (reader(&stdin), reader(&stdin));
        let ((first_wakes, first_waker), (second_wakes, second_waker)) = (waker(), waker());

        assert!(read(&mut first, &first_waker).is_pending());
        // The second view queues for the turn instead of overwriting the parked waker.
        assert!(read(&mut second, &second_waker).is_pending());
        assert!(
            read(&mut second, &second_waker).is_pending(),
            "re-polls stay queued"
        );
        assert_eq!(
            source.gate().polls,
            1,
            "only the turn holder polls the source"
        );

        source.release(|gate| gate.chunks.push_back(b"one".to_vec()));
        assert_eq!((wakes(&first_wakes), wakes(&second_wakes)), (1, 0));
        assert_eq!(read(&mut first, &first_waker), Poll::Ready(b"one".to_vec()));
        // Completing the operation hands the turn to the queued view and wakes it once.
        assert_eq!(wakes(&second_wakes), 1);
        assert!(read(&mut second, &second_waker).is_pending());
        source.release(|gate| gate.chunks.push_back(b"two".to_vec()));
        assert_eq!((wakes(&first_wakes), wakes(&second_wakes)), (1, 2));
        assert_eq!(
            read(&mut second, &second_waker),
            Poll::Ready(b"two".to_vec())
        );
        assert_eq!(source.gate().polls, 4, "no busy polling");

        // An idle view holds no turn: the other view reads without waiting for it.
        source.release(|gate| gate.chunks.push_back(b"three".to_vec()));
        assert_eq!(
            read(&mut first, &first_waker),
            Poll::Ready(b"three".to_vec())
        );
    }

    #[test]
    fn dropping_the_pending_owner_or_a_queued_view_passes_the_turn_on() {
        let source = OneWaker::default();
        let stdin = input(&source);
        let mut views: Vec<_> = (0..3).map(|_| reader(&stdin)).collect();
        let wakers: Vec<_> = (0..3).map(|_| waker()).collect();
        for (view, (_, waker)) in views.iter_mut().zip(&wakers) {
            assert!(read(view, waker).is_pending());
        }
        let mut third = views.pop().unwrap();
        // A queued view leaves the queue without disturbing the order.
        drop(views.pop());
        // Dropping the pending owner (its cancellation) releases the turn to the next waiter.
        drop(views.pop());
        assert_eq!(wakes(&wakers[2].0), 1);
        source.release(|gate| gate.chunks.push_back(b"kept".to_vec()));
        assert_eq!(
            read(&mut third, &wakers[2].1),
            Poll::Ready(b"kept".to_vec()),
            "no accepted data is lost"
        );
    }

    #[test]
    fn concurrent_output_views_take_turns_and_keep_every_accepted_byte() {
        let sink = OneWaker::default();
        let stdout = output(&sink);
        let (mut first, mut second) = (writer(&stdout), writer(&stdout));
        let ((first_wakes, first_waker), (second_wakes, second_waker)) = (waker(), waker());

        assert!(write(&mut first, &first_waker, b"first").is_pending());
        assert!(write(&mut second, &second_waker, b"second").is_pending());
        assert_eq!(sink.gate().polls, 1);

        sink.release(|gate| gate.capacity = 5);
        assert_eq!((wakes(&first_wakes), wakes(&second_wakes)), (1, 0));
        assert_eq!(write(&mut first, &first_waker, b"first"), Poll::Ready(5));
        assert_eq!(wakes(&second_wakes), 1);
        assert!(write(&mut second, &second_waker, b"second").is_pending());
        sink.release(|gate| gate.capacity = 6);
        assert_eq!(wakes(&second_wakes), 2);
        assert_eq!(write(&mut second, &second_waker, b"second"), Poll::Ready(6));
        assert_eq!(sink.gate().accepted, b"firstsecond");
        assert_eq!(sink.gate().polls, 4, "no busy polling");

        // Dropping the pending writer releases the turn; a later writer proceeds.
        assert!(write(&mut first, &first_waker, b"x").is_pending());
        assert!(write(&mut second, &second_waker, b"y").is_pending());
        drop(first);
        assert_eq!(wakes(&second_wakes), 3);
        sink.release(|gate| gate.capacity = 1);
        assert_eq!(write(&mut second, &second_waker, b"y"), Poll::Ready(1));
        assert_eq!(sink.gate().accepted, b"firstsecondy");
    }

    fn refused(result: Result<()>) {
        let error = result.unwrap_err();
        assert_eq!(error.code, tembra::core::Code::Invalid, "{error}");
        assert!(error.to_string().contains("regular files"), "{error}");
    }
    fn configure(path: &Path) -> [Result<()>; 2] {
        [
            stdin(&Input::File(path.into())).map(drop),
            stdout(&Output::File(path.into())).map(drop),
        ]
    }

    #[test]
    fn regular_files_symlinks_to_them_and_new_outputs_are_accepted() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("data");
        std::fs::write(&path, b"bytes").unwrap();
        configure(&path).into_iter().for_each(Result::unwrap);
        let created = directory.path().join("created");
        stdout(&Output::File(created.clone())).unwrap();
        assert!(std::fs::metadata(&created).unwrap().is_file());
        assert!(stdin(&Input::File(directory.path().join("absent"))).is_err());
        #[cfg(unix)]
        {
            let link = directory.path().join("link");
            std::os::unix::fs::symlink(&path, &link).unwrap();
            configure(&link).into_iter().for_each(Result::unwrap);
        }
    }

    #[test]
    fn directories_are_refused() {
        let directory = tempfile::tempdir().unwrap();
        configure(directory.path()).into_iter().for_each(refused);
    }

    /// Linux/Unix only: a FIFO whose ordinary open would block until a peer appears, a
    /// character device, and a socket are all refused without blocking.
    #[cfg(unix)]
    #[test]
    fn fifos_devices_and_sockets_are_refused_without_blocking() {
        use std::{ffi::CString, os::unix::ffi::OsStrExt, time::Duration};
        let directory = tempfile::tempdir().unwrap();
        let fifo = directory.path().join("fifo");
        let name = CString::new(fifo.as_os_str().as_bytes()).unwrap();
        // SAFETY: `name` is a valid NUL-terminated path for the duration of the call.
        assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);

        // An ordinary open really blocks on this FIFO while no writer exists.
        let (opened, receive) = std::sync::mpsc::channel();
        let path = fifo.clone();
        let blocked = std::thread::spawn(move || {
            let file = std::fs::File::open(path);
            opened.send(()).unwrap();
            file
        });
        assert!(
            receive.recv_timeout(Duration::from_millis(200)).is_err(),
            "open blocks"
        );

        // The configured stream refuses it immediately, for input and output.
        let started = std::time::Instant::now();
        configure(&fifo).into_iter().for_each(refused);
        assert!(started.elapsed() < Duration::from_secs(1));

        // Release the blocked plain open with a writer.
        let _writer = std::fs::OpenOptions::new().write(true).open(&fifo).unwrap();
        blocked.join().unwrap().unwrap();

        // A character device is never opened, so no device open side effect occurs.
        configure(Path::new("/dev/null"))
            .into_iter()
            .for_each(refused);
        let socket = directory.path().join("socket");
        let _listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
        configure(&socket).into_iter().for_each(refused);
    }
}
