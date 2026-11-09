//! Capabilityless WASI preview 1 functions for the interpreter's exact import allowlist.
//!
//! The source VM receives an empty environment, real clocks and standard output/error that
//! forward to attributed diagnostics. There are no preopened directories, no readable input and
//! no other descriptors, so filesystem and socket access stay scoped host effects. Each function
//! works on a plain memory slice; the linker wrappers only borrow memory and state.
use crate::abi;
use crate::protocol::LogLevel;
use std::time::{Instant, SystemTime, UNIX_EPOCH};
use wasmtime::{Caller, Linker};

/// Per-instance standard descriptor state and monotonic clock origin.
pub(super) struct Wasi {
    open: [bool; abi::STDIO_DESCRIPTORS as usize],
    origin: Instant,
}

impl Default for Wasi {
    /// Every instance starts with its three standard descriptors open.
    fn default() -> Self {
        Self {
            open: [true; abi::STDIO_DESCRIPTORS as usize],
            origin: Instant::now(),
        }
    }
}

/// Borrow a guest range or report `EFAULT`.
fn range(memory: &[u8], pointer: u32, length: u32) -> Result<std::ops::Range<usize>, u32> {
    let start = pointer as usize;
    let end = start
        .checked_add(length as usize)
        .filter(|end| *end <= memory.len())
        .ok_or(abi::ERRNO_FAULT)?;
    Ok(start..end)
}

/// Store little-endian bytes at a checked guest address.
fn store(memory: &mut [u8], pointer: u32, bytes: &[u8]) -> Result<(), u32> {
    let range = range(memory, pointer, bytes.len() as u32)?;
    memory[range].copy_from_slice(bytes);
    Ok(())
}

/// Collapse a result into the WASI error number.
fn errno(result: Result<(), u32>) -> u32 {
    result.err().unwrap_or(abi::ERRNO_SUCCESS)
}

impl Wasi {
    /// Recognize only standard descriptors that the guest has not closed.
    fn is_open(&self, descriptor: u32) -> bool {
        self.open.get(descriptor as usize).copied().unwrap_or(false)
    }

    /// Real wall-clock and monotonic time in nanoseconds; CPU-time clocks are unsupported.
    pub fn clock_time_get(&self, memory: &mut [u8], clock: u32, out: u32) -> u32 {
        let nanoseconds = match clock {
            abi::CLOCK_REALTIME => SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|elapsed| elapsed.as_nanos())
                .unwrap_or(0),
            abi::CLOCK_MONOTONIC => self.origin.elapsed().as_nanos(),
            _ => return abi::ERRNO_INVAL,
        };
        let nanoseconds = u64::try_from(nanoseconds).unwrap_or(u64::MAX);
        errno(store(memory, out, &nanoseconds.to_le_bytes()))
    }

    /// The environment is empty: zero variables occupying zero bytes.
    pub fn environ_sizes_get(&self, memory: &mut [u8], count: u32, size: u32) -> u32 {
        errno(store(memory, count, &0u32.to_le_bytes()).and(store(
            memory,
            size,
            &0u32.to_le_bytes(),
        )))
    }

    /// Writing an empty environment touches no guest memory.
    pub fn environ_get(&self, _memory: &mut [u8], _environ: u32, _buffer: u32) -> u32 {
        abi::ERRNO_SUCCESS
    }

    /// Close one standard descriptor; later operations on it report `EBADF`.
    pub fn fd_close(&mut self, descriptor: u32) -> u32 {
        if !self.is_open(descriptor) {
            return abi::ERRNO_BADF;
        }
        self.open[descriptor as usize] = false;
        abi::ERRNO_SUCCESS
    }

    /// Standard descriptors are character devices; only output and error may be written.
    pub fn fd_fdstat_get(&self, memory: &mut [u8], descriptor: u32, out: u32) -> u32 {
        if !self.is_open(descriptor) {
            return abi::ERRNO_BADF;
        }
        let mut stat = [0u8; abi::FDSTAT_BYTES as usize];
        stat[0] = abi::FILETYPE_CHARACTER_DEVICE as u8;
        let rights = if descriptor == 0 {
            0
        } else {
            u64::from(abi::RIGHT_FD_WRITE)
        };
        stat[8..16].copy_from_slice(&rights.to_le_bytes());
        errno(store(memory, out, &stat))
    }

    /// There are no preopened directories.
    pub fn fd_prestat_get(&self, _descriptor: u32) -> u32 {
        abi::ERRNO_BADF
    }

    /// Character devices cannot seek.
    pub fn fd_seek(&self, descriptor: u32) -> u32 {
        if self.is_open(descriptor) {
            abi::ERRNO_SPIPE
        } else {
            abi::ERRNO_BADF
        }
    }

    /// Open standard descriptors are not directories; every other descriptor is absent.
    pub fn path_open(&self, descriptor: u32) -> u32 {
        if self.is_open(descriptor) {
            abi::ERRNO_NOTDIR
        } else {
            abi::ERRNO_BADF
        }
    }

    /// Gather one write to standard output or error. The whole write is consumed; its first
    /// [`abi::STDIO_LIMIT`] bytes are returned for attributed diagnostics.
    pub fn fd_write(
        &self,
        memory: &mut [u8],
        descriptor: u32,
        vectors: u32,
        count: u32,
        written: u32,
    ) -> Result<(LogLevel, Vec<u8>), u32> {
        let level = match descriptor {
            1 if self.is_open(1) => LogLevel::Log,
            2 if self.is_open(2) => LogLevel::Error,
            _ => return Err(abi::ERRNO_BADF),
        };
        let table = range(
            memory,
            vectors,
            count.checked_mul(8).ok_or(abi::ERRNO_INVAL)?,
        )?;
        let mut total = 0u32;
        let mut text = Vec::new();
        for vector in memory[table].as_chunks::<8>().0 {
            let pointer = u32::from_le_bytes([vector[0], vector[1], vector[2], vector[3]]);
            let length = u32::from_le_bytes([vector[4], vector[5], vector[6], vector[7]]);
            let bytes = &memory[range(memory, pointer, length)?];
            total = total.checked_add(length).ok_or(abi::ERRNO_INVAL)?;
            let room = (abi::STDIO_LIMIT as usize).saturating_sub(text.len());
            text.extend_from_slice(&bytes[..bytes.len().min(room)]);
        }
        store(memory, written, &total.to_le_bytes())?;
        Ok((level, text))
    }
}

/// Link exactly the allowlisted functions over the instance's imported memory.
pub(super) fn add(linker: &mut Linker<super::runtime::State>) -> wasmtime::Result<()> {
    use super::runtime::State;
    /// Borrow guest memory and host state together for one call.
    fn parts<'a>(caller: &'a mut Caller<'_, State>) -> (&'a mut [u8], &'a mut State) {
        let memory = caller
            .data()
            .memory
            .expect("memory is defined before instantiation");
        memory.data_and_store_mut(caller)
    }
    let module = abi::WASI_MODULE;
    linker.func_wrap(
        module,
        "clock_time_get",
        |mut caller: Caller<'_, State>, clock: u32, _precision: u64, out: u32| {
            let (memory, state) = parts(&mut caller);
            state.wasi.clock_time_get(memory, clock, out)
        },
    )?;
    linker.func_wrap(
        module,
        "environ_get",
        |mut caller: Caller<'_, State>, environ: u32, buffer: u32| {
            let (memory, state) = parts(&mut caller);
            state.wasi.environ_get(memory, environ, buffer)
        },
    )?;
    linker.func_wrap(
        module,
        "environ_sizes_get",
        |mut caller: Caller<'_, State>, count: u32, size: u32| {
            let (memory, state) = parts(&mut caller);
            state.wasi.environ_sizes_get(memory, count, size)
        },
    )?;
    linker.func_wrap(
        module,
        "fd_close",
        |mut caller: Caller<'_, State>, descriptor: u32| {
            caller.data_mut().wasi.fd_close(descriptor)
        },
    )?;
    linker.func_wrap(
        module,
        "fd_fdstat_get",
        |mut caller: Caller<'_, State>, descriptor: u32, out: u32| {
            let (memory, state) = parts(&mut caller);
            state.wasi.fd_fdstat_get(memory, descriptor, out)
        },
    )?;
    linker.func_wrap(
        module,
        "fd_prestat_get",
        |caller: Caller<'_, State>, descriptor: u32, _out: u32| {
            caller.data().wasi.fd_prestat_get(descriptor)
        },
    )?;
    linker.func_wrap(
        module,
        "fd_prestat_dir_name",
        |caller: Caller<'_, State>, descriptor: u32, _path: u32, _length: u32| {
            caller.data().wasi.fd_prestat_get(descriptor)
        },
    )?;
    linker.func_wrap(
        module,
        "fd_seek",
        |caller: Caller<'_, State>, descriptor: u32, _offset: i64, _whence: u32, _out: u32| {
            caller.data().wasi.fd_seek(descriptor)
        },
    )?;
    linker.func_wrap(
        module,
        "fd_write",
        |mut caller: Caller<'_, State>, descriptor: u32, vectors: u32, count: u32, written: u32| {
            let (memory, state) = parts(&mut caller);
            match state
                .wasi
                .fd_write(memory, descriptor, vectors, count, written)
            {
                Ok((level, text)) => {
                    let text = String::from_utf8_lossy(&text);
                    let text = text.trim_end_matches('\n');
                    if !text.is_empty() {
                        let _ = state.diagnostics.emit(level, text.to_owned());
                    }
                    abi::ERRNO_SUCCESS
                }
                Err(errno) => errno,
            }
        },
    )?;
    linker.func_wrap(
        module,
        "path_open",
        |caller: Caller<'_, State>,
         descriptor: u32,
         _flags: u32,
         _path: u32,
         _length: u32,
         _open: u32,
         _base: u64,
         _inheriting: u64,
         _descriptor_flags: u32,
         _out: u32| caller.data().wasi.path_open(descriptor),
    )?;
    linker.func_wrap(
        module,
        "proc_exit",
        |_: Caller<'_, State>, code: u32| -> wasmtime::Result<()> {
            Err(wasmtime::Error::msg(format!(
                "interpreter exited with status {code}"
            )))
        },
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clocks_environment_and_descriptors_have_capabilityless_behavior() {
        let mut wasi = Wasi::default();
        let mut memory = vec![0xffu8; 128];
        assert_eq!(wasi.clock_time_get(&mut memory, abi::CLOCK_REALTIME, 0), 0);
        let realtime = u64::from_le_bytes(memory[0..8].try_into().unwrap());
        assert!(
            realtime > 1_600_000_000_000_000_000,
            "realtime is a real wall clock"
        );
        assert_eq!(wasi.clock_time_get(&mut memory, abi::CLOCK_MONOTONIC, 8), 0);
        assert_eq!(wasi.clock_time_get(&mut memory, 2, 8), abi::ERRNO_INVAL);
        assert_eq!(
            wasi.clock_time_get(&mut memory, abi::CLOCK_REALTIME, 124),
            abi::ERRNO_FAULT
        );
        assert_eq!(wasi.environ_sizes_get(&mut memory, 16, 20), 0);
        assert_eq!(&memory[16..24], &[0; 8]);
        // Standard descriptors are character devices; there are no preopens or other files.
        assert_eq!(wasi.fd_fdstat_get(&mut memory, 1, 32), 0);
        assert_eq!(memory[32], abi::FILETYPE_CHARACTER_DEVICE as u8);
        assert_eq!(
            u64::from_le_bytes(memory[40..48].try_into().unwrap()),
            u64::from(abi::RIGHT_FD_WRITE)
        );
        assert_eq!(wasi.fd_fdstat_get(&mut memory, 3, 32), abi::ERRNO_BADF);
        for descriptor in 0..5 {
            assert_eq!(wasi.fd_prestat_get(descriptor), abi::ERRNO_BADF);
        }
        assert_eq!(wasi.path_open(3), abi::ERRNO_BADF);
        assert_eq!(wasi.path_open(1), abi::ERRNO_NOTDIR);
        assert_eq!(wasi.fd_seek(1), abi::ERRNO_SPIPE);
        assert_eq!(wasi.fd_seek(9), abi::ERRNO_BADF);
        // Writes gather every vector, report the full count and bound the diagnostic text.
        memory[64..72].copy_from_slice(&[96, 0, 0, 0, 3, 0, 0, 0]);
        memory[72..80].copy_from_slice(&[99, 0, 0, 0, 2, 0, 0, 0]);
        memory[96..101].copy_from_slice(b"abcde");
        let (level, text) = wasi.fd_write(&mut memory, 2, 64, 2, 80).unwrap();
        assert_eq!((level, text.as_slice()), (LogLevel::Error, &b"abcde"[..]));
        assert_eq!(u32::from_le_bytes(memory[80..84].try_into().unwrap()), 5);
        assert_eq!(
            wasi.fd_write(&mut memory, 0, 64, 2, 80).unwrap_err(),
            abi::ERRNO_BADF
        );
        memory[72..80].copy_from_slice(&[120, 0, 0, 0, 64, 0, 0, 0]);
        assert_eq!(
            wasi.fd_write(&mut memory, 1, 64, 2, 80).unwrap_err(),
            abi::ERRNO_FAULT
        );
        assert_eq!(wasi.fd_close(1), 0);
        assert_eq!(wasi.fd_close(1), abi::ERRNO_BADF);
        assert_eq!(
            wasi.fd_write(&mut memory, 1, 64, 1, 80).unwrap_err(),
            abi::ERRNO_BADF
        );
    }
}
