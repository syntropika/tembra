//! Private fixed-buffer ABI between Tembra hosts and the reusable interpreter core module.
//!
//! This file is the single definition of the transport. The guest includes it through
//! `#[path]`, the native host compiles it directly, and the browser host uses TypeScript
//! constants generated from it (`packages/ts/sdk/src/interpreter/abi.ts`). It depends only on
//! `core`/`alloc` so both sides share the exact encoder and decoder.
//!
//! Buffers contain no pointers. Every record is little-endian `u32` fields followed by the
//! concatenated byte bodies in record order, so a decoder checks one total length and never
//! dereferences guest-selected addresses.
//!
//! Reply (guest-owned, returned by [`CALL_EXPORT`], valid until the next call):
//! `total, kind, interrupted, count, value_len`, then `count` effect records
//! `id_low, id_high, packet_len`, then the value bytes, then every effect packet.
//!
//! Completion batch (host-written into a buffer obtained from [`ALLOC_EXPORT`]):
//! `count`, then `count` records `id_low, id_high, status, value_len`, then every value.

/// Import namespace carries the ABI version and the generated WASI schema digest.
pub const IMPORT_MODULE: &str = env!("TEMBRA_INTERPRETER_IMPORT_MODULE");
/// First byte of a binary standard WASI packet; JSON packets never start with NUL.
pub const STANDARD_PACKET: u32 = 0;
/// Synchronous host request: `(packet_ptr, packet_len) -> header`. The header holds the reply
/// length; [`SYNC_ERROR`] marks an error message instead of a JSON value.
pub const SYNC_IMPORT: &str = "sync";
/// Copy the pending synchronous reply: `(destination_ptr, length)`. The length must equal the
/// header length; any other use traps.
pub const SYNC_READ_IMPORT: &str = "sync_read";
/// Flag bit of the synchronous reply header; the remaining bits are the reply length.
pub const SYNC_ERROR: u32 = 0x8000_0000;
/// WebAssembly linear memory is supplied by the host so it can set a per-instance maximum.
pub const MEMORY_MODULE: &str = "env";
pub const MEMORY_IMPORT: &str = "memory";
/// The guest links with exactly this initial memory; hosts supply it and their configured maximum.
pub const MEMORY_INITIAL_PAGES: u32 = 32;
pub const PAGE_BYTES: u32 = 65_536;
/// Reactor constructor export, called once before any other export.
pub const INITIALIZE_EXPORT: &str = "_initialize";
/// `(length) -> pointer`: reserve the single host-written input buffer, or return 0.
pub const ALLOC_EXPORT: &str = "tembra_alloc";
/// `(operation, input_ptr, input_len) -> reply_ptr`: run one serialized logical operation.
pub const CALL_EXPORT: &str = "tembra_call";
/// Largest length of any packet, value or snapshot crossing this ABI.
pub const LENGTH_LIMIT: u32 = 0x7fff_ffff;
/// Largest number of host effects outstanding for one callback.
pub const EFFECT_LIMIT: u32 = 128;
/// Effect identities are positive integers exactly representable as JavaScript numbers.
pub const IDENTITY_LIMIT: u64 = (1 << 53) - 1;
pub const REPLY_HEADER_BYTES: u32 = 20;
pub const EFFECT_RECORD_BYTES: u32 = 12;
pub const COMPLETION_HEADER_BYTES: u32 = 4;
pub const COMPLETION_RECORD_BYTES: u32 = 16;

/// Exact standard imports linked into the interpreter by Rust std and wasi-libc. Each has a
/// capabilityless host implementation: an empty environment, real clocks, standard output and
/// error forwarded to attributed diagnostics, no preopened directories and no other
/// descriptors. Any import outside this list is refused before instantiation.
pub const WASI_MODULE: &str = "wasi_snapshot_preview1";
pub const WASI_IMPORTS: &[&str] = &[
    "clock_time_get",
    "environ_get",
    "environ_sizes_get",
    "fd_close",
    "fd_fdstat_get",
    "fd_prestat_dir_name",
    "fd_prestat_get",
    "fd_seek",
    "fd_write",
    "path_open",
    "proc_exit",
];
/// WASI error numbers returned by the capabilityless implementations.
pub const ERRNO_SUCCESS: u32 = 0;
pub const ERRNO_BADF: u32 = 8;
pub const ERRNO_FAULT: u32 = 21;
pub const ERRNO_INVAL: u32 = 28;
pub const ERRNO_NOTDIR: u32 = 54;
pub const ERRNO_SPIPE: u32 = 70;
/// WASI clock identifiers with real implementations; CPU-time clocks are unsupported.
pub const CLOCK_REALTIME: u32 = 0;
pub const CLOCK_MONOTONIC: u32 = 1;
/// Standard descriptors 0..=2 are character devices; only 1 and 2 accept writes.
pub const STDIO_DESCRIPTORS: u32 = 3;
pub const FILETYPE_CHARACTER_DEVICE: u32 = 2;
pub const RIGHT_FD_WRITE: u32 = 64;
/// `fdstat` layout: filetype u8 at 0, flags u16 at 2, base rights u64 at 8, inheriting at 16.
pub const FDSTAT_BYTES: u32 = 24;
/// Largest standard-stream write forwarded to the attributed diagnostic sink.
pub const STDIO_LIMIT: u32 = 4096;

/// Logical interpreter operations selected by the host.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u32)]
pub enum Operation {
    /// Input: load JSON. Reply: outcome with the callback description.
    Load = 0,
    /// Input: declaration JSON. Reply: outcome.
    Declare = 1,
    /// Input: dispatch JSON. Reply: progress or outcome.
    Start = 2,
    /// Input: completion batch. Reply: progress or outcome.
    Resume = 3,
    /// No input. Reply: outcome of the revoked callback.
    Cancel = 4,
    /// No input. Reply: snapshot bytes or an error message.
    Capture = 5,
}

impl Operation {
    pub const ALL: [(&'static str, Self); 6] = [
        ("Load", Self::Load),
        ("Declare", Self::Declare),
        ("Start", Self::Start),
        ("Resume", Self::Resume),
        ("Cancel", Self::Cancel),
        ("Capture", Self::Capture),
    ];

    /// Accept only defined operation numbers.
    pub fn decode(value: u32) -> Option<Self> {
        Self::ALL
            .iter()
            .find(|(_, operation)| *operation as u32 == value)
            .map(|(_, operation)| *operation)
    }

    /// Operations without input require a null buffer.
    pub fn has_input(self) -> bool {
        !matches!(self, Self::Cancel | Self::Capture)
    }
}

/// Reply kinds. `Pending` carries effects; outcomes carry a value or an error message.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u32)]
pub enum ReplyKind {
    Pending = 0,
    Ok = 1,
    Error = 2,
}

impl ReplyKind {
    pub const ALL: [(&'static str, Self); 3] = [
        ("Pending", Self::Pending),
        ("Ok", Self::Ok),
        ("Error", Self::Error),
    ];
}

/// Completion status written by the host for each settled effect.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u32)]
pub enum Status {
    Ok = 0,
    Error = 1,
}

impl Status {
    pub const ALL: [(&'static str, Self); 2] = [("Ok", Self::Ok), ("Error", Self::Error)];
}

/// One decoded guest reply borrowing the copied reply buffer.
#[derive(Debug, PartialEq, Eq)]
pub struct Reply<'a> {
    pub kind: ReplyKind,
    pub interrupted: bool,
    pub value: &'a [u8],
    pub effects: Vec<(u64, &'a [u8])>,
}

/// One decoded host completion borrowing the guest's input buffer.
#[derive(Debug, PartialEq, Eq)]
pub struct Completion<'a> {
    pub id: u64,
    pub ok: bool,
    pub value: &'a [u8],
}

/// Read one little-endian field; callers have already checked the bound.
fn field(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(bytes[offset..offset + 4].try_into().expect("checked field"))
}

/// Split an identity into JavaScript-safe halves after checking its range.
fn identity(id: u64) -> Result<(u32, u32), String> {
    if id == 0 || id > IDENTITY_LIMIT {
        return Err("invalid effect identity".into());
    }
    Ok((id as u32, (id >> 32) as u32))
}

/// Join and check identity halves received from the other side.
fn join(low: u32, high: u32) -> Result<u64, String> {
    let id = u64::from(high) << 32 | u64::from(low);
    identity(id).map(|_| id)
}

/// Check a body length against the ABI limit before it is encoded.
fn length(bytes: &[u8]) -> Result<u32, String> {
    u32::try_from(bytes.len())
        .ok()
        .filter(|length| *length <= LENGTH_LIMIT)
        .ok_or_else(|| "ABI body exceeds its length limit".into())
}

/// Consume the bodies following the records; the sum must equal the remaining buffer exactly.
fn bodies<'a>(
    bytes: &'a [u8],
    mut offset: usize,
    lengths: &[u32],
) -> Result<Vec<&'a [u8]>, String> {
    let mut out = Vec::with_capacity(lengths.len());
    for length in lengths {
        let end = offset
            .checked_add(*length as usize)
            .filter(|end| *end <= bytes.len())
            .ok_or("ABI body exceeds its buffer")?;
        out.push(&bytes[offset..end]);
        offset = end;
    }
    if offset != bytes.len() {
        return Err("ABI buffer has trailing bytes".into());
    }
    Ok(out)
}

/// Encode a guest reply; the guest returns its address and keeps ownership.
pub fn encode_reply(
    kind: ReplyKind,
    interrupted: bool,
    value: &[u8],
    effects: &[(u64, &[u8])],
) -> Result<Vec<u8>, String> {
    if effects.len() > EFFECT_LIMIT as usize || (kind != ReplyKind::Pending && !effects.is_empty())
    {
        return Err("invalid effect batch".into());
    }
    let records = REPLY_HEADER_BYTES as usize + effects.len() * EFFECT_RECORD_BYTES as usize;
    let total = effects
        .iter()
        .map(|(_, packet)| packet.len())
        .try_fold(records + value.len(), usize::checked_add)
        .and_then(|total| u32::try_from(total).ok())
        .ok_or("ABI reply exceeds its length limit")?;
    let mut out = Vec::with_capacity(total as usize);
    for value in [
        total,
        kind as u32,
        u32::from(interrupted),
        effects.len() as u32,
        length(value)?,
    ] {
        out.extend_from_slice(&value.to_le_bytes());
    }
    for (id, packet) in effects {
        let (low, high) = identity(*id)?;
        for value in [low, high, length(packet)?] {
            out.extend_from_slice(&value.to_le_bytes());
        }
    }
    out.extend_from_slice(value);
    for (_, packet) in effects {
        out.extend_from_slice(packet);
    }
    Ok(out)
}

impl<'a> Reply<'a> {
    /// Decode a reply copied out of guest memory, refusing any inconsistent layout.
    pub fn decode(bytes: &'a [u8]) -> Result<Self, String> {
        if bytes.len() < REPLY_HEADER_BYTES as usize || field(bytes, 0) as usize != bytes.len() {
            return Err("invalid ABI reply length".into());
        }
        let kind = match field(bytes, 4) {
            0 => ReplyKind::Pending,
            1 => ReplyKind::Ok,
            2 => ReplyKind::Error,
            _ => return Err("invalid ABI reply kind".into()),
        };
        let interrupted = match field(bytes, 8) {
            0 => false,
            1 => true,
            _ => return Err("invalid ABI interruption flag".into()),
        };
        let count = field(bytes, 12);
        if count > EFFECT_LIMIT || (kind != ReplyKind::Pending && count != 0) {
            return Err("invalid ABI effect count".into());
        }
        let records = REPLY_HEADER_BYTES as usize + count as usize * EFFECT_RECORD_BYTES as usize;
        if records > bytes.len() {
            return Err("ABI effect records exceed the reply".into());
        }
        let mut lengths = vec![field(bytes, 16)];
        let mut ids = Vec::with_capacity(count as usize);
        for index in 0..count as usize {
            let offset = REPLY_HEADER_BYTES as usize + index * EFFECT_RECORD_BYTES as usize;
            ids.push(join(field(bytes, offset), field(bytes, offset + 4))?);
            lengths.push(field(bytes, offset + 8));
        }
        let bodies = bodies(bytes, records, &lengths)?;
        Ok(Self {
            kind,
            interrupted,
            value: bodies[0],
            effects: ids.into_iter().zip(bodies[1..].iter().copied()).collect(),
        })
    }
}

/// Encode settled host effects into one guest input buffer.
pub fn encode_completions(completions: &[Completion<'_>]) -> Result<Vec<u8>, String> {
    if completions.is_empty() || completions.len() > EFFECT_LIMIT as usize {
        return Err("invalid completion batch".into());
    }
    let mut out = Vec::new();
    out.extend_from_slice(&(completions.len() as u32).to_le_bytes());
    for completion in completions {
        let (low, high) = identity(completion.id)?;
        let status = if completion.ok {
            Status::Ok
        } else {
            Status::Error
        };
        for value in [low, high, status as u32, length(completion.value)?] {
            out.extend_from_slice(&value.to_le_bytes());
        }
    }
    for completion in completions {
        out.extend_from_slice(completion.value);
    }
    length(&out)?;
    Ok(out)
}

impl<'a> Completion<'a> {
    /// Decode one host-written completion batch inside guest memory.
    pub fn decode_batch(bytes: &'a [u8]) -> Result<Vec<Self>, String> {
        if bytes.len() < COMPLETION_HEADER_BYTES as usize {
            return Err("invalid completion batch".into());
        }
        let count = field(bytes, 0);
        if count == 0 || count > EFFECT_LIMIT {
            return Err("invalid completion batch".into());
        }
        let records =
            COMPLETION_HEADER_BYTES as usize + count as usize * COMPLETION_RECORD_BYTES as usize;
        if records > bytes.len() {
            return Err("completion records exceed the batch".into());
        }
        let mut heads = Vec::with_capacity(count as usize);
        let mut lengths = Vec::with_capacity(count as usize);
        for index in 0..count as usize {
            let offset =
                COMPLETION_HEADER_BYTES as usize + index * COMPLETION_RECORD_BYTES as usize;
            let id = join(field(bytes, offset), field(bytes, offset + 4))?;
            let ok = match field(bytes, offset + 8) {
                0 => true,
                1 => false,
                _ => return Err("invalid completion status".into()),
            };
            heads.push((id, ok));
            lengths.push(field(bytes, offset + 12));
        }
        Ok(heads
            .into_iter()
            .zip(bodies(bytes, records, &lengths)?)
            .map(|((id, ok), value)| Self { id, ok, value })
            .collect())
    }
}

/// Render the generated TypeScript constants used by the browser host.
#[cfg(test)]
pub fn typescript() -> String {
    let mut out = String::from(
        "// Generated from crates/executor-wasm/src/abi.rs. Do not edit.\n\
         // Regenerate with: TEMBRA_BLESS=1 cargo test -p tembra-executor-wasm --lib abi\n",
    );
    let strings = [
        ("IMPORT_MODULE", IMPORT_MODULE),
        ("SYNC_IMPORT", SYNC_IMPORT),
        ("SYNC_READ_IMPORT", SYNC_READ_IMPORT),
        ("MEMORY_MODULE", MEMORY_MODULE),
        ("MEMORY_IMPORT", MEMORY_IMPORT),
        ("INITIALIZE_EXPORT", INITIALIZE_EXPORT),
        ("ALLOC_EXPORT", ALLOC_EXPORT),
        ("CALL_EXPORT", CALL_EXPORT),
        ("WASI_MODULE", WASI_MODULE),
    ];
    for (name, value) in strings {
        let declaration = format!("export const {name} = {value:?};");
        if declaration.len() > 100 {
            out += &format!("export const {name} =\n  {value:?};\n");
        } else {
            out += &format!("{declaration}\n");
        }
    }
    let numbers = [
        ("SYNC_ERROR", u64::from(SYNC_ERROR)),
        ("STANDARD_PACKET", u64::from(STANDARD_PACKET)),
        ("MEMORY_INITIAL_PAGES", u64::from(MEMORY_INITIAL_PAGES)),
        ("PAGE_BYTES", u64::from(PAGE_BYTES)),
        ("LENGTH_LIMIT", u64::from(LENGTH_LIMIT)),
        ("EFFECT_LIMIT", u64::from(EFFECT_LIMIT)),
        ("IDENTITY_LIMIT", IDENTITY_LIMIT),
        ("REPLY_HEADER_BYTES", u64::from(REPLY_HEADER_BYTES)),
        ("EFFECT_RECORD_BYTES", u64::from(EFFECT_RECORD_BYTES)),
        (
            "COMPLETION_HEADER_BYTES",
            u64::from(COMPLETION_HEADER_BYTES),
        ),
        (
            "COMPLETION_RECORD_BYTES",
            u64::from(COMPLETION_RECORD_BYTES),
        ),
        ("ERRNO_SUCCESS", u64::from(ERRNO_SUCCESS)),
        ("ERRNO_BADF", u64::from(ERRNO_BADF)),
        ("ERRNO_FAULT", u64::from(ERRNO_FAULT)),
        ("ERRNO_INVAL", u64::from(ERRNO_INVAL)),
        ("ERRNO_NOTDIR", u64::from(ERRNO_NOTDIR)),
        ("ERRNO_SPIPE", u64::from(ERRNO_SPIPE)),
        ("CLOCK_REALTIME", u64::from(CLOCK_REALTIME)),
        ("CLOCK_MONOTONIC", u64::from(CLOCK_MONOTONIC)),
        ("STDIO_DESCRIPTORS", u64::from(STDIO_DESCRIPTORS)),
        (
            "FILETYPE_CHARACTER_DEVICE",
            u64::from(FILETYPE_CHARACTER_DEVICE),
        ),
        ("RIGHT_FD_WRITE", u64::from(RIGHT_FD_WRITE)),
        ("FDSTAT_BYTES", u64::from(FDSTAT_BYTES)),
        ("STDIO_LIMIT", u64::from(STDIO_LIMIT)),
    ];
    for (name, value) in numbers {
        out += &format!("export const {name} = {value};\n");
    }
    out += "export const WASI_IMPORTS = Object.freeze([\n";
    for name in WASI_IMPORTS {
        out += &format!("  {name:?},\n");
    }
    out += "] as const);\n";
    let enums: [(&str, Vec<(&str, u32)>); 3] = [
        (
            "Operation",
            Operation::ALL
                .iter()
                .map(|(n, v)| (*n, *v as u32))
                .collect(),
        ),
        (
            "ReplyKind",
            ReplyKind::ALL
                .iter()
                .map(|(n, v)| (*n, *v as u32))
                .collect(),
        ),
        (
            "Status",
            Status::ALL.iter().map(|(n, v)| (*n, *v as u32)).collect(),
        ),
    ];
    for (name, variants) in enums {
        out += &format!("export const {name} = Object.freeze({{\n");
        for (variant, value) in variants {
            out += &format!("  {variant}: {value},\n");
        }
        out +=
            &format!("}} as const);\nexport type {name} = (typeof {name})[keyof typeof {name}];\n");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_typescript_matches_the_abi() {
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../packages/ts/sdk/src/interpreter/abi.ts"
        );
        let generated = typescript();
        if std::env::var_os("TEMBRA_BLESS").is_some() {
            std::fs::write(path, &generated).unwrap();
        }
        assert!(
            std::fs::read_to_string(path).unwrap_or_default() == generated,
            "{path} is stale; regenerate with TEMBRA_BLESS=1 cargo test -p tembra-executor-wasm --lib abi"
        );
    }

    #[test]
    fn replies_round_trip_and_refuse_inconsistent_layouts() {
        let packet: &[u8] = br#"{"operation":"wait","milliseconds":1}"#;
        let bytes = encode_reply(
            ReplyKind::Pending,
            false,
            b"",
            &[(7, packet), (IDENTITY_LIMIT, b"x")],
        )
        .unwrap();
        let reply = Reply::decode(&bytes).unwrap();
        assert_eq!(reply.kind, ReplyKind::Pending);
        assert_eq!(
            reply.effects,
            vec![(7, packet), (IDENTITY_LIMIT, &b"x"[..])]
        );
        let outcome = encode_reply(ReplyKind::Error, true, b"failed", &[]).unwrap();
        let reply = Reply::decode(&outcome).unwrap();
        assert!(reply.interrupted && reply.value == b"failed");
        // Truncation, trailing bytes, a wrong total and invalid flags are all refused.
        assert!(Reply::decode(&bytes[..bytes.len() - 1]).is_err());
        let mut trailing = bytes.clone();
        trailing.push(0);
        assert!(Reply::decode(&trailing).is_err());
        let mut total = bytes.clone();
        total[0..4].copy_from_slice(&((bytes.len() as u32) + 1).to_le_bytes());
        total.push(0);
        assert!(
            Reply::decode(&total).is_err(),
            "body lengths must cover the buffer"
        );
        for (offset, value) in [(4, 3u32), (8, 2), (12, EFFECT_LIMIT + 1)] {
            let mut broken = bytes.clone();
            broken[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
            assert!(Reply::decode(&broken).is_err(), "offset {offset}");
        }
        let mut overflow = bytes.clone();
        overflow[28..32].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(Reply::decode(&overflow).is_err());
        // Outcomes cannot carry effects; identities stay in the JavaScript-safe range.
        assert!(encode_reply(ReplyKind::Ok, false, b"1", &[(1, b"x")]).is_err());
        assert!(encode_reply(ReplyKind::Pending, false, b"", &[(0, b"x")]).is_err());
        assert!(
            encode_reply(
                ReplyKind::Pending,
                false,
                b"",
                &[(IDENTITY_LIMIT + 1, b"x")]
            )
            .is_err()
        );
        let many: Vec<(u64, &[u8])> = (1..=u64::from(EFFECT_LIMIT) + 1)
            .map(|id| (id, &b""[..]))
            .collect();
        assert!(encode_reply(ReplyKind::Pending, false, b"", &many).is_err());
        assert!(
            encode_reply(
                ReplyKind::Pending,
                false,
                b"",
                &many[..EFFECT_LIMIT as usize]
            )
            .is_ok()
        );
    }

    #[test]
    fn completions_round_trip_and_refuse_inconsistent_layouts() {
        let batch = [
            Completion {
                id: 1,
                ok: true,
                value: b"null",
            },
            Completion {
                id: 1 << 40,
                ok: false,
                value: "\u{e9}".as_bytes(),
            },
        ];
        let bytes = encode_completions(&batch).unwrap();
        assert_eq!(Completion::decode_batch(&bytes).unwrap(), batch);
        assert!(encode_completions(&[]).is_err());
        assert!(Completion::decode_batch(&bytes[..bytes.len() - 1]).is_err());
        let mut status = bytes.clone();
        status[12..16].copy_from_slice(&2u32.to_le_bytes());
        assert!(Completion::decode_batch(&status).is_err());
        let mut unsafe_id = bytes.clone();
        unsafe_id[8..12].copy_from_slice(&(1u32 << 21).to_le_bytes());
        assert!(Completion::decode_batch(&unsafe_id).is_err());
        let mut zero = bytes.clone();
        zero[0..4].copy_from_slice(&0u32.to_le_bytes());
        assert!(Completion::decode_batch(&zero).is_err());
        assert_eq!(Operation::decode(5), Some(Operation::Capture));
        assert_eq!(Operation::decode(6), None);
    }
}
