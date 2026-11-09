//! Source-Orb I/O uses host-owned directory capabilities and exact destination grants.
//!
//! The guest supplies only typed requests. Filesystem work runs on a few bounded blocking
//! workers; sockets are incremental, nonblocking streams on one shared reactor, so a slow reader
//! never occupies a filesystem worker or the embedding runtime's thread pool.
use crate::{failure, protocol::AsyncRequest, protocol::TRANSPORT_CHUNK_BYTES};
#[cfg(unix)]
use cap_std::fs::OpenOptionsExt;
use cap_std::fs::{Dir, OpenOptions};
use serde_json::Value;
use std::{
    collections::BTreeSet,
    io::{Read, Write},
    net::SocketAddr,
    sync::Arc,
    time::{Duration, Instant},
};
use tembra::Result;

mod reactor;
mod streams;

pub(crate) use streams::Streams;

const TEXT_LIMIT: usize = 1 << 20;

struct Mount {
    path: String,
    directory: Arc<Dir>,
    writable: bool,
}

/// Bound once for an installation; no directory handle or network authority enters the VM.
#[derive(Clone)]
pub(crate) struct SourceIo {
    mounts: Arc<Vec<Mount>>,
    destinations: Arc<BTreeSet<SocketAddr>>,
    streams: Streams,
    timeout: Duration,
}
impl SourceIo {
    pub(crate) fn new(
        providers: &crate::providers::Providers,
        imports: &[tembra_contracts::HostImport],
        grants: &crate::Grants,
        timeout: Duration,
    ) -> Result<Self> {
        grants.check(
            imports,
            &providers.filesystem.keys().cloned().collect(),
            &providers.network,
        )?;
        let mut mounts = grants
            .filesystem
            .iter()
            .map(|grant| {
                let directory = Dir::open_ambient_dir(
                    &providers.filesystem[&grant.provider],
                    cap_std::ambient_authority(),
                )
                .map_err(failure)?;
                Ok(Mount {
                    path: grant.mount.clone(),
                    directory: Arc::new(directory),
                    writable: grant.writable,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        mounts.sort_by_key(|mount| std::cmp::Reverse(mount.path.len()));
        Ok(Self {
            mounts: Arc::new(mounts),
            destinations: Arc::new(
                grants
                    .network
                    .iter()
                    .flat_map(|grant| grant.connect.iter().copied())
                    .collect(),
            ),
            streams: Streams::default(),
            timeout,
        })
    }

    /// The transport table whose epochs follow this Orb's guest callbacks.
    pub(crate) fn streams(&self) -> &Streams {
        &self.streams
    }

    /// Check one request against this installation and capture its deadline at admission.
    pub(crate) fn operation(&self, packet: &str) -> Result<IoOperation> {
        let request = match AsyncRequest::decode(packet).map_err(failure)? {
            AsyncRequest::ReadText { path } => {
                let (directory, path) = self.file(&path, false)?;
                Operation::ReadText {
                    directory: directory.clone(),
                    path,
                }
            }
            AsyncRequest::WriteText { path, value } => {
                bounded(&value)?;
                let (directory, path) = self.file(&path, true)?;
                Operation::WriteText {
                    directory: directory.clone(),
                    path,
                    value,
                }
            }
            AsyncRequest::TransportConnect { ip, port } => {
                let address = SocketAddr::new(
                    ip.parse()
                        .map_err(|_| failure("transport requires a literal IP address"))?,
                    port,
                );
                if !self.destinations.contains(&address) {
                    return Err(failure("transport destination is not granted to this Orb"));
                }
                Operation::Connect {
                    reservation: self.streams.reserve()?,
                    address,
                }
            }
            AsyncRequest::TransportRead { id } => {
                let stream = self.streams.stream(id)?;
                let reader = stream.reader()?;
                Operation::Read { stream, reader }
            }
            AsyncRequest::TransportWrite { id, bytes } => {
                if bytes.len() > TRANSPORT_CHUNK_BYTES {
                    return Err(failure("transport write exceeds the chunk limit"));
                }
                let stream = self.streams.stream(id)?;
                let writer = stream.writer()?;
                Operation::Write {
                    stream,
                    writer,
                    bytes,
                }
            }
            AsyncRequest::TransportClose { id } => Operation::Close {
                streams: self.streams.clone(),
                id,
            },
            _ => return Err(failure("expected an I/O operation")),
        };
        Ok(IoOperation {
            request,
            deadline: Instant::now() + self.timeout,
        })
    }

    fn file(&self, path: &str, write: bool) -> Result<(&Arc<Dir>, String)> {
        bounded(path)?;
        if !path.starts_with('/')
            || path.contains(['\\', '\0'])
            || path
                .split('/')
                .skip(1)
                .any(|part| part.is_empty() || part == "." || part == "..")
        {
            return Err(failure(
                "filesystem paths must be normalized absolute paths",
            ));
        }
        for mount in self.mounts.iter() {
            if let Some(relative) = path
                .strip_prefix(&mount.path)
                .and_then(|suffix| suffix.strip_prefix('/'))
            {
                if write && !mount.writable {
                    return Err(failure("filesystem mount is read-only"));
                }
                return Ok((&mount.directory, relative.to_owned()));
            }
        }
        Err(failure("filesystem path is outside the granted mounts"))
    }
}

/// A Send operation owns its original installation's capabilities until physical completion.
enum Operation {
    ReadText {
        directory: Arc<Dir>,
        path: String,
    },
    WriteText {
        directory: Arc<Dir>,
        path: String,
        value: String,
    },
    Connect {
        reservation: streams::Reservation,
        address: SocketAddr,
    },
    Read {
        stream: Arc<streams::Stream>,
        reader: tokio::sync::OwnedMutexGuard<tokio::net::tcp::OwnedReadHalf>,
    },
    Write {
        stream: Arc<streams::Stream>,
        writer: tokio::sync::OwnedMutexGuard<tokio::net::tcp::OwnedWriteHalf>,
        bytes: Vec<u8>,
    },
    Close {
        streams: Streams,
        id: u32,
    },
}

pub struct IoOperation {
    request: Operation,
    deadline: Instant,
}
impl IoOperation {
    /// Submit admitted host work without occupying the embedding runtime's worker pool.
    ///
    /// `complete` runs exactly once, on the submitting thread when admission fails and on a
    /// host I/O thread otherwise.
    pub fn submit(self, complete: impl FnOnce(Result<Value>) + Send + 'static) {
        let reactor = match reactor::reactor() {
            Ok(reactor) => reactor,
            Err(error) => return complete(Err(error)),
        };
        let deadline = self.deadline;
        match self.request {
            request @ (Operation::ReadText { .. } | Operation::WriteText { .. }) => {
                match reactor.admit_file() {
                    Ok(permit) => reactor.run_file(permit, move || {
                        let outcome =
                            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                                run_file(request, deadline)
                            }))
                            .unwrap_or_else(|_| Err(failure("host I/O worker panicked")));
                        complete(outcome);
                    }),
                    Err(error) => complete(Err(error)),
                }
            }
            Operation::Connect {
                reservation,
                address,
            } => reactor.spawn(async move {
                complete(reservation.connect(address, deadline).await);
            }),
            Operation::Read { stream, reader } => reactor.spawn(async move {
                complete(stream.read(reader, deadline).await);
            }),
            Operation::Write {
                stream,
                writer,
                bytes,
            } => reactor.spawn(async move {
                complete(stream.write(writer, bytes, deadline).await);
            }),
            Operation::Close { streams, id } => complete(streams.close(id)),
        }
    }

    pub(crate) async fn execute(self) -> Result<Value> {
        let (send, receive) = futures::channel::oneshot::channel();
        self.submit(move |outcome| {
            let _ = send.send(outcome);
        });
        receive.await.map_err(failure)?
    }
}

/// Execute one blocking filesystem operation; expired work never starts a write.
fn run_file(request: Operation, deadline: Instant) -> Result<Value> {
    if Instant::now() >= deadline {
        return Err(failure("host I/O deadline exceeded"));
    }
    match request {
        Operation::ReadText { directory, path } => {
            let file = directory
                .open_with(path, &options(false))
                .map_err(failure)?;
            if !file.metadata().map_err(failure)?.is_file() {
                return Err(failure("text reads require a regular file"));
            }
            let mut text = String::new();
            file.take((TEXT_LIMIT + 1) as u64)
                .read_to_string(&mut text)
                .map_err(failure)?;
            bounded(&text)?;
            Ok(Value::String(text))
        }
        Operation::WriteText {
            directory,
            path,
            value,
        } => {
            let mut file = directory.open_with(path, &options(true)).map_err(failure)?;
            if !file.metadata().map_err(failure)?.is_file() {
                return Err(failure("text writes require a regular file"));
            }
            file.set_len(0).map_err(failure)?;
            file.write_all(value.as_bytes()).map_err(failure)?;
            Ok(Value::Null)
        }
        _ => Err(failure("expected a filesystem operation")),
    }
}

fn bounded(text: &str) -> Result<()> {
    if text.len() > TEXT_LIMIT {
        Err(failure("text exceeds the one mebibyte limit"))
    } else {
        Ok(())
    }
}

fn options(write: bool) -> OpenOptions {
    let mut options = OpenOptions::new();
    options.read(!write).write(write).create(write);
    #[cfg(unix)]
    options.custom_flags(libc::O_NONBLOCK);
    options
}

#[cfg(test)]
mod tests;
