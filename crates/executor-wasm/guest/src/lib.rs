//! One serialized interpreter retains continuations; the host waits outside its Wasm stack.
//!
//! The module is a WASI preview 1 reactor with host-imported linear memory. Its whole host
//! interface is the private fixed-buffer ABI in `src/abi.rs`: two exports, two imports.
/// The fixed-buffer layout is shared byte-for-byte with the native and browser hosts.
#[path = "../../src/abi.rs"]
#[allow(dead_code)]
mod abi;
mod io;
/// The private host-operation protocol is shared with the native executor.
#[path = "../../src/protocol.rs"]
#[allow(dead_code)]
mod protocol;
mod vm;
use abi::{Completion, Operation, ReplyKind};
use std::{
    cell::{Cell, OnceCell},
    rc::Rc,
};
use vm::Vm;

#[cfg(target_feature = "atomics")]
compile_error!("The interpreter requires the single-threaded Wasm target");

/// Host functions; the literal names must equal `abi::IMPORT_MODULE` and its import names,
/// which the engine build verifies against the actual import section.
mod host {
    #[link(wasm_import_module = "tembra-interpreter-v2")]
    unsafe extern "C" {
        pub fn sync(packet: *const u8, length: usize) -> u32;
        pub fn sync_read(destination: *mut u8, length: usize);
    }
}

/// Instance-wide state; the module is single-threaded and every access is non-reentrant.
struct Instance {
    busy: Cell<bool>,
    /// The one host-writable input buffer reserved by `tembra_alloc`.
    input: Cell<Option<*mut [u8]>>,
    /// The reply returned by the latest call; the host reads it before calling again.
    reply: Cell<Option<Box<[u8]>>>,
}
unsafe impl Sync for Instance {}
static INSTANCE: Instance = Instance {
    busy: Cell::new(false),
    input: Cell::new(None),
    reply: Cell::new(None),
};

struct ExportGuard;
impl ExportGuard {
    /// Refuse reentry while a synchronous step owns the stack.
    fn enter() -> Result<Self, String> {
        if INSTANCE.busy.replace(true) {
            return Err("interpreter exports must be serialized".into());
        }
        Ok(Self)
    }
}
impl Drop for ExportGuard {
    /// Reopen the interpreter only after the exported frame physically returned.
    fn drop(&mut self) {
        INSTANCE.busy.set(false);
    }
}
thread_local! {
    /// The one source revision of this module instance; it is written once and never replaced.
    static VM: OnceCell<Rc<Vm>> = const { OnceCell::new() };
}
/// Share the published revision without holding a cell borrow across JavaScript calls.
fn instance_vm() -> Option<Rc<Vm>> {
    VM.with(|vm| vm.get().cloned())
}
/// Publish one source revision for this module instance.
fn publish_vm(vm: Vm) -> Result<(), String> {
    VM.with(|slot| slot.set(Rc::new(vm)))
        .map_err(|_| "the instance already owns an Orb revision".into())
}
/// Preserve host failures as JavaScript errors without capturing host authority.
fn error(message: String) -> rquickjs::Error {
    rquickjs::Error::new_from_js_message("host", "operation", message)
}

/// Perform one synchronous host request through the two-phase import.
pub(crate) fn host_sync(packet: &str) -> Result<String, String> {
    let header = unsafe { host::sync(packet.as_ptr(), packet.len()) };
    let length = (header & !abi::SYNC_ERROR) as usize;
    let mut reply = vec![0u8; length];
    unsafe { host::sync_read(reply.as_mut_ptr(), length) };
    let reply = String::from_utf8(reply).map_err(|_| "host reply is not UTF-8".to_owned())?;
    if header & abi::SYNC_ERROR == 0 {
        Ok(reply)
    } else {
        Err(reply)
    }
}

/// One logical result before it is laid out for the host.
struct Reply {
    kind: ReplyKind,
    interrupted: bool,
    value: Vec<u8>,
    effects: Vec<vm::Effect>,
}
impl Reply {
    /// An outcome closes the callback; `interrupted` asks the host to quarantine the instance.
    fn outcome(value: Result<Vec<u8>, String>, interrupted: bool) -> Self {
        let (kind, value) = match value {
            Ok(value) => (ReplyKind::Ok, value),
            Err(message) => (ReplyKind::Error, message.into_bytes()),
        };
        Self {
            kind,
            interrupted,
            value,
            effects: Vec::new(),
        }
    }
    /// Quarantine the instance without running VM cleanup, which this frame may not own.
    fn quarantine(message: String) -> Self {
        Self::outcome(Err(message), true)
    }
}

/// Report cleanup and instruction exhaustion with the original callback result.
fn finished(vm: &Vm, result: Result<String, String>) -> Reply {
    let value = vm.finish(result).map(String::into_bytes);
    Reply::outcome(value, vm.interrupted.get())
}
/// Close admission with bounded cleanup and quarantine the instance after a failed transition.
fn abort(message: String) -> Reply {
    let Some(vm) = instance_vm() else {
        return Reply::quarantine(message);
    };
    // Cleanup never clears this flag, so the reply stays interrupted.
    vm.interrupted.set(true);
    finished(&vm, Err(message))
}
/// Drain ready jobs once; pending host effects do not retain a native stack.
fn advance(vm: &Vm) -> Result<Reply, String> {
    vm.pump()?;
    if let Some(result) = vm.outcome()? {
        return Ok(finished(vm, result));
    }
    // `Vm::effects` bounds the queue by `abi::EFFECT_LIMIT` before reading it.
    Ok(Reply {
        kind: ReplyKind::Pending,
        interrupted: false,
        value: Vec::new(),
        effects: vm.effects()?,
    })
}
/// Text inputs must be valid UTF-8 before any engine sees them.
fn text(input: Box<[u8]>) -> Result<String, String> {
    String::from_utf8(input.into_vec()).map_err(|_| "interpreter input is not UTF-8".into())
}

/// Load one source revision; module initialization receives no execution scope.
fn load(input: Box<[u8]>) -> Reply {
    let result = (|| {
        if instance_vm().is_some() {
            return Err("the instance already owns an Orb revision".into());
        }
        std::panic::set_hook(Box::new(|information| {
            let _ = io::log(
                protocol::LogLevel::Error,
                format!("interpreter panic: {information}"),
            );
        }));
        let vm = Vm::load(&text(input)?)?;
        let description = vm.helper("__tembraDescribe", None)?;
        publish_vm(vm)?;
        Ok(description.into_bytes())
    })();
    let interrupted = result.is_err();
    Reply::outcome(result, interrupted)
}
/// Factories and validators remain logically synchronous under the current host scope.
fn declare(input: Box<[u8]>) -> Reply {
    let Some(vm) = instance_vm() else {
        return Reply::quarantine("Orb source is not loaded".into());
    };
    match text(input) {
        Ok(packet) => {
            let value = vm.declare(packet).map(String::into_bytes);
            Reply::outcome(value, vm.interrupted.get())
        }
        Err(message) => abort(message),
    }
}
/// Start a callback without resetting its budget when subsequent effects complete.
fn start(input: Box<[u8]>) -> Reply {
    (|| {
        let vm = instance_vm().ok_or("Orb source is not loaded")?;
        if vm.accepting.get() {
            return Err("interpreter already owns a callback".into());
        }
        vm.start(text(input)?)?;
        advance(&vm)
    })()
    .unwrap_or_else(abort)
}
/// Complete only effects still retained by this callback's heap.
fn resume(input: Box<[u8]>) -> Reply {
    (|| {
        let vm = instance_vm().ok_or("Orb source is not loaded")?;
        if !vm.accepting.get() {
            return Err("callback scope is closed".into());
        }
        for completion in Completion::decode_batch(&input)? {
            let value = std::str::from_utf8(completion.value)
                .map_err(|_| "completion value is not UTF-8")?
                .to_owned();
            vm.complete(
                completion.id,
                if completion.ok { Ok(value) } else { Err(value) },
            )?;
        }
        advance(&vm)
    })()
    .unwrap_or_else(abort)
}
/// Revoke an idle callback after host cancellation; active frames must return first.
fn cancel() -> Reply {
    if instance_vm().is_none() {
        return Reply::quarantine("Orb source is not loaded".into());
    }
    abort("callback interrupted".into())
}
/// Capture only idle heap state with no platform stack or retained authority.
fn capture() -> Reply {
    let result = instance_vm()
        .ok_or_else(|| "Orb source is not loaded".to_owned())
        .and_then(|vm| vm.capture());
    Reply::outcome(result, false)
}

/// Take ownership of the reserved input buffer; only the exact reserved range is accepted.
fn take_input(
    operation: Operation,
    pointer: usize,
    length: usize,
) -> Result<Option<Box<[u8]>>, String> {
    let reserved = INSTANCE.input.take();
    if !operation.has_input() {
        if pointer != 0 || length != 0 || reserved.is_some() {
            if let Some(buffer) = reserved {
                drop(unsafe { Box::from_raw(buffer) });
            }
            return Err("operation takes no input buffer".into());
        }
        return Ok(None);
    }
    let buffer = reserved.ok_or("input buffer was not reserved")?;
    let input = unsafe { Box::from_raw(buffer) };
    if input.as_ptr() as usize != pointer || input.len() != length {
        return Err("input buffer does not match its reservation".into());
    }
    Ok(Some(input))
}

/// Dispatch one logical operation under the export guard.
fn call(operation: u32, pointer: usize, length: usize) -> Reply {
    let _guard = match ExportGuard::enter() {
        Ok(guard) => guard,
        Err(message) => return Reply::quarantine(message),
    };
    let Some(operation) = Operation::decode(operation) else {
        return Reply::quarantine("unknown interpreter operation".into());
    };
    let input = match take_input(operation, pointer, length) {
        Ok(input) => input,
        Err(message) => return abort(message),
    };
    match (operation, input) {
        (Operation::Load, Some(input)) => load(input),
        (Operation::Declare, Some(input)) => declare(input),
        (Operation::Start, Some(input)) => start(input),
        (Operation::Resume, Some(input)) => resume(input),
        (Operation::Cancel, None) => cancel(),
        (Operation::Capture, None) => capture(),
        _ => abort("operation input mismatch".into()),
    }
}

/// Reserve the single host-written input buffer for the next call; 0 refuses the request.
#[unsafe(no_mangle)]
pub extern "C" fn tembra_alloc(length: usize) -> *mut u8 {
    if INSTANCE.busy.get() || length > abi::LENGTH_LIMIT as usize {
        return std::ptr::null_mut();
    }
    if let Some(previous) = INSTANCE.input.take() {
        // A second reservation without a call would leak ownership; refuse and release both.
        drop(unsafe { Box::from_raw(previous) });
        return std::ptr::null_mut();
    }
    // A zero-length reservation is a dangling but nonnull address that is never dereferenced.
    let buffer = Box::into_raw(vec![0u8; length].into_boxed_slice());
    INSTANCE.input.set(Some(buffer));
    buffer.cast::<u8>()
}

/// Run one operation and return the guest-owned reply, valid until the next call.
#[unsafe(no_mangle)]
pub extern "C" fn tembra_call(operation: u32, pointer: usize, length: usize) -> *const u8 {
    let reply = call(operation, pointer, length);
    let effects: Vec<(u64, &[u8])> = reply
        .effects
        .iter()
        .map(|effect| (effect.id, effect.packet.as_bytes()))
        .collect();
    let bytes = abi::encode_reply(reply.kind, reply.interrupted, &reply.value, &effects)
        .or_else(|message| abi::encode_reply(ReplyKind::Error, true, message.as_bytes(), &[]))
        .expect("bounded failure reply")
        .into_boxed_slice();
    let address = bytes.as_ptr();
    // Dropping the previous reply here releases memory the host has finished reading.
    INSTANCE.reply.set(Some(bytes));
    address
}
