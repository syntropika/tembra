//! Source-Orb I/O uses host-owned directory capabilities and exact destination grants.
use crate::{failure, protocol::AsyncRequest};
#[cfg(unix)]
use cap_std::fs::OpenOptionsExt;
use cap_std::fs::{Dir, OpenOptions};
use serde_json::Value;
use std::{
    collections::BTreeSet,
    io::{Read, Write},
    net::{Shutdown, SocketAddr, TcpStream},
    sync::Arc,
    time::{Duration, Instant},
};
use tembra::Result;

mod pool;

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
            timeout,
        })
    }
    pub(crate) fn operation(&self, packet: &str) -> Result<IoOperation> {
        let request = match AsyncRequest::decode(packet).map_err(failure)? {
            AsyncRequest::ReadText { path } => {
                let (directory, path) = self.file(&path, false)?;
                Operation::Read {
                    directory: directory.clone(),
                    path,
                }
            }
            AsyncRequest::WriteText { path, value } => {
                bounded(&value)?;
                let (directory, path) = self.file(&path, true)?;
                Operation::Write {
                    directory: directory.clone(),
                    path,
                    value,
                }
            }
            AsyncRequest::Exchange { ip, port, request } => {
                bounded(&request)?;
                let address = SocketAddr::new(
                    ip.parse()
                        .map_err(|_| failure("transport requires a literal IP address"))?,
                    port,
                );
                if !self.destinations.contains(&address) {
                    return Err(failure("transport destination is not granted to this Orb"));
                }
                Operation::Exchange { address, request }
            }
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
    Read {
        directory: Arc<Dir>,
        path: String,
    },
    Write {
        directory: Arc<Dir>,
        path: String,
        value: String,
    },
    Exchange {
        address: SocketAddr,
        request: String,
    },
}

pub struct IoOperation {
    request: Operation,
    deadline: Instant,
}
impl IoOperation {
    /// Execute on a host worker, never on the VM or JavaScript event-loop thread.
    fn run(self) -> Result<Value> {
        let deadline = self.deadline;
        remaining(deadline)?;
        match self.request {
            Operation::Read { directory, path } => {
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
            Operation::Write {
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
            Operation::Exchange { address, request } => {
                let mut socket =
                    TcpStream::connect_timeout(&address, remaining(deadline)?).map_err(failure)?;
                let mut sent = request.as_bytes();
                while !sent.is_empty() {
                    socket
                        .set_write_timeout(Some(remaining(deadline)?))
                        .map_err(failure)?;
                    let count = socket.write(sent).map_err(failure)?;
                    if count == 0 {
                        return Err(failure("transport closed while writing"));
                    }
                    sent = &sent[count..];
                }
                socket.shutdown(Shutdown::Write).map_err(failure)?;
                let mut response = Vec::new();
                let mut buffer = [0; 8192];
                loop {
                    socket
                        .set_read_timeout(Some(remaining(deadline)?))
                        .map_err(failure)?;
                    let count = socket.read(&mut buffer).map_err(failure)?;
                    if count == 0 {
                        break;
                    }
                    if response.len() + count > TEXT_LIMIT {
                        return Err(failure("transport response exceeds the text limit"));
                    }
                    response.extend_from_slice(&buffer[..count]);
                }
                Ok(Value::String(String::from_utf8(response).map_err(failure)?))
            }
        }
    }
    /// Submit bounded host work without occupying the embedding runtime's worker pool.
    pub fn submit(self, complete: impl FnOnce(Result<Value>) + Send + 'static) {
        pool::submit(self, complete);
    }
    pub(crate) async fn execute(self) -> Result<Value> {
        let (send, receive) = futures::channel::oneshot::channel();
        self.submit(move |outcome| {
            let _ = send.send(outcome);
        });
        receive.await.map_err(failure)?
    }
}
fn bounded(text: &str) -> Result<()> {
    if text.len() > TEXT_LIMIT {
        Err(failure("text exceeds the one mebibyte limit"))
    } else {
        Ok(())
    }
}
fn remaining(deadline: Instant) -> Result<Duration> {
    let duration = deadline.saturating_duration_since(Instant::now());
    if duration.is_zero() {
        Err(failure("host I/O deadline exceeded"))
    } else {
        Ok(duration)
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
mod tests {
    use super::*;
    fn io(path: &std::path::Path, writable: bool) -> SourceIo {
        let mut providers = crate::providers::Providers::default();
        providers.filesystem.insert("files".into(), path.into());
        SourceIo::new(
            &providers,
            &[tembra_contracts::HostImport {
                binding: "filesystem".into(),
                contract: "wasi:filesystem/preopens@0.3.0".into(),
                provider: "files".into(),
            }],
            &crate::Grants {
                filesystem: vec![crate::FilesystemGrant {
                    provider: "files".into(),
                    mount: "/data".into(),
                    writable,
                }],
                ..Default::default()
            },
            Duration::from_secs(1),
        )
        .unwrap()
    }
    fn run(io: &SourceIo, packet: &str) -> Result<Value> {
        io.operation(packet).and_then(IoOperation::run)
    }

    #[test]
    fn source_io_enforces_mounts_permissions_and_text_bounds() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(directory.path().join("input"), "original").unwrap();
        let reader = io(directory.path(), false);
        let writer = io(directory.path(), true);
        assert_eq!(
            run(
                &reader,
                r#"{"operation":"filesystem.read-text","path":"/data/input"}"#
            )
            .unwrap(),
            "original"
        );
        assert!(
            run(
                &reader,
                r#"{"operation":"filesystem.write-text","path":"/data/input","value":"bad"}"#
            )
            .is_err()
        );
        for path in [
            "/data/../input",
            "/database/input",
            "/data//input",
            "/data/./input",
            "/input",
            "/data/a\\b",
        ] {
            let packet =
                serde_json::json!({"operation":"filesystem.read-text","path":path}).to_string();
            assert!(run(&reader, &packet).is_err(), "{path}");
        }
        run(
            &writer,
            r#"{"operation":"filesystem.write-text","path":"/data/input","value":"updated"}"#,
        )
        .unwrap();
        assert_eq!(
            std::fs::read_to_string(directory.path().join("input")).unwrap(),
            "updated"
        );
        let packet = serde_json::json!({"operation":"filesystem.write-text","path":"/data/input","value":"x".repeat(TEXT_LIMIT+1)}).to_string();
        assert!(run(&writer, &packet).is_err());
        assert!(
            run(
                &writer,
                r#"{"operation":"transport.exchange","ip":"127.0.0.1","port":80,"request":"x"}"#
            )
            .is_err()
        );
    }
    #[test]
    fn expired_work_cannot_start_a_write() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("input");
        std::fs::write(&path, "original").unwrap();
        let mut operation = io(directory.path(), true)
            .operation(
                r#"{"operation":"filesystem.write-text","path":"/data/input","value":"late"}"#,
            )
            .unwrap();
        operation.deadline = Instant::now() - Duration::from_secs(1);
        assert!(operation.run().is_err());
        assert_eq!(std::fs::read_to_string(path).unwrap(), "original");
    }
    #[cfg(unix)]
    #[test]
    fn directory_capability_rejects_symlink_escape_and_fifo() {
        use std::os::unix::fs::symlink;
        let directory = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("secret"), "private").unwrap();
        symlink(outside.path(), directory.path().join("escape")).unwrap();
        let io = io(directory.path(), true);
        for (operation, path) in [
            ("filesystem.read-text", "/data/escape/secret"),
            ("filesystem.write-text", "/data/escape/secret"),
        ] {
            let mut packet = serde_json::json!({"operation":operation,"path":path});
            if operation.ends_with("write-text") {
                packet["value"] = Value::String("bad".into());
            }
            assert!(run(&io, &packet.to_string()).is_err());
        }
        assert_eq!(
            std::fs::read_to_string(outside.path().join("secret")).unwrap(),
            "private"
        );
        let fifo =
            std::ffi::CString::new(directory.path().join("fifo").as_os_str().as_encoded_bytes())
                .unwrap();
        // This test owns the newly created temporary directory and named pipe.
        assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
        assert!(
            run(
                &io,
                r#"{"operation":"filesystem.read-text","path":"/data/fifo"}"#
            )
            .is_err()
        );
        assert!(
            run(
                &io,
                r#"{"operation":"filesystem.write-text","path":"/data/fifo","value":"x"}"#
            )
            .is_err()
        );
    }
}
