//! One serialized interpreter retains continuations; the host waits outside its Wasm stack.
mod io;
/// The private host-operation protocol is shared with the native executor.
#[path = "../../src/protocol.rs"]
#[allow(dead_code)]
mod protocol;
mod vm;
use std::{
    cell::{Cell, UnsafeCell},
    rc::Rc,
};
use vm::Vm;

#[cfg(target_feature = "atomics")]
compile_error!("The interpreter requires the single-threaded Wasm target");
wit_bindgen::generate!({path:"../wit",world:"interpreter"});
use tembra::runtime::execution::Effect;
struct Component;
struct InstanceVm(UnsafeCell<Option<Rc<Vm>>>, Cell<bool>);
unsafe impl Sync for InstanceVm {}
static VM: InstanceVm = InstanceVm(UnsafeCell::new(None), Cell::new(false));
struct ExportGuard;
impl ExportGuard {
    /// Refuse reentry while a synchronous step or its WASI I/O still owns the stack.
    fn enter() -> Result<Self, String> {
        if VM.1.replace(true) {
            return Err("interpreter exports must be serialized".into());
        }
        Ok(Self)
    }
}
impl Drop for ExportGuard {
    /// Reopen the interpreter only after the exported frame physically returned.
    fn drop(&mut self) {
        VM.1.set(false);
    }
}
/// Retain VM storage independently of platform task-local state.
fn instance_vm() -> Option<Rc<Vm>> {
    unsafe { (&*VM.0.get()).clone() }
}
/// Publish one source revision for this component instance.
fn publish_vm(vm: Vm) {
    unsafe {
        *VM.0.get() = Some(Rc::new(vm));
    }
}
/// Preserve host failures as JavaScript errors without capturing host authority.
fn error(message: String) -> rquickjs::Error {
    rquickjs::Error::new_from_js_message("host", "operation", message)
}
/// Report cleanup and instruction exhaustion with the original callback result.
fn outcome(vm: &Vm, result: Result<String, String>) -> Outcome {
    let value = vm.finish(result);
    Outcome {
        value,
        interrupted: vm.interrupted.get(),
    }
}
/// A failed driver transition closes admission before returning to the host.
fn failed(message: String) -> Progress {
    let result = if VM.1.get() {
        Outcome {
            value: Err(message),
            interrupted: true,
        }
    } else {
        instance_vm().map_or_else(
            || Outcome {
                value: Err(message.clone()),
                interrupted: true,
            },
            |vm| {
                vm.interrupted.set(true);
                outcome(&vm, Err(message.clone()))
            },
        )
    };
    Progress {
        outcome: Some(result),
        effects: Vec::new(),
    }
}
/// Drain ready jobs once; pending host effects do not retain a native stack.
fn advance(vm: &Vm) -> Result<Progress, String> {
    vm.pump()?;
    if let Some(result) = vm.outcome()? {
        return Ok(Progress {
            outcome: Some(outcome(vm, result)),
            effects: Vec::new(),
        });
    }
    let effects = vm.effects()?;
    if effects.len() > 128 {
        return Err("host operation queue is full".into());
    }
    Ok(Progress {
        outcome: None,
        effects: effects
            .into_iter()
            .map(|effect| Effect {
                id: effect.id,
                packet: effect.packet,
            })
            .collect(),
    })
}
impl Guest for Component {
    /// Load one source revision; module initialization receives no execution scope.
    fn load(packet: String) -> Outcome {
        let result = (|| {
            let _guard = ExportGuard::enter()?;
            if instance_vm().is_some() {
                return Err("the component already owns an Orb revision".into());
            }
            std::panic::set_hook(Box::new(|information| {
                let _ = io::log(
                    protocol::LogLevel::Error,
                    format!("interpreter panic: {information}"),
                );
            }));
            let vm = Vm::load(&packet)?;
            let description = vm.helper("__tembraDescribe", None)?;
            publish_vm(vm);
            Ok(description)
        })();
        Outcome {
            interrupted: result.is_err(),
            value: result,
        }
    }
    /// Factories and validators remain logically synchronous under the current host scope.
    fn declare(packet: String) -> Outcome {
        let result = (|| {
            let _guard = ExportGuard::enter()?;
            let vm = instance_vm().ok_or("Orb source is not loaded")?;
            Ok(Outcome {
                value: vm.declare(packet),
                interrupted: vm.interrupted.get(),
            })
        })();
        result.unwrap_or_else(|message: String| Outcome {
            value: Err(message),
            interrupted: true,
        })
    }
    /// Start a callback without resetting its budget when subsequent effects complete.
    fn start(packet: String) -> Progress {
        (|| {
            let _guard = ExportGuard::enter()?;
            let vm = instance_vm().ok_or("Orb source is not loaded")?;
            if vm.accepting.get() {
                return Err("interpreter already owns a callback".into());
            }
            vm.start(packet)?;
            advance(&vm)
        })()
        .unwrap_or_else(failed)
    }
    /// Complete only effects still retained by this callback's heap.
    fn resume(completed: Vec<Completion>) -> Progress {
        (|| {
            let _guard = ExportGuard::enter()?;
            let vm = instance_vm().ok_or("Orb source is not loaded")?;
            if !vm.accepting.get() {
                return Err("callback scope is closed".into());
            }
            if completed.is_empty() || completed.len() > 128 {
                return Err("invalid completion batch".into());
            }
            for completion in completed {
                vm.complete(completion.id, completion.value)?;
            }
            advance(&vm)
        })()
        .unwrap_or_else(failed)
    }
    /// Revoke an idle callback after host cancellation; active frames must return first.
    fn cancel() -> Outcome {
        let result = (|| {
            let _guard = ExportGuard::enter()?;
            let vm = instance_vm().ok_or("Orb source is not loaded")?;
            vm.interrupted.set(true);
            Ok(outcome(&vm, Err("callback interrupted".into())))
        })();
        result.unwrap_or_else(|message: String| Outcome {
            value: Err(message),
            interrupted: true,
        })
    }
    /// Capture only idle heap state with no platform stack or retained authority.
    fn capture() -> Result<Vec<u8>, String> {
        let _guard = ExportGuard::enter()?;
        instance_vm().ok_or("Orb source is not loaded")?.capture()
    }
}
export!(Component);
