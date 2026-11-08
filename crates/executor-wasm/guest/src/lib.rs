//! One persistent synchronous interpreter per component; the outer driver owns asynchronous effects.
mod io;
mod vm;
use futures::{
    FutureExt, StreamExt,
    future::{AbortHandle, Abortable},
    stream::FuturesUnordered,
};
use std::{
    cell::{Cell, UnsafeCell},
    rc::Rc,
};
use vm::Vm;

#[cfg(target_feature = "atomics")]
compile_error!("The interpreter component requires the single-threaded Wasm target");
wit_bindgen::generate!({path:"../wit",world:"interpreter"});
struct Component;
// WASIp3 TLS belongs to individual tasks, so the host serializes access to instance storage.
struct InstanceVm(UnsafeCell<Option<Rc<Vm>>>, Cell<bool>);
unsafe impl Sync for InstanceVm {}
static VM: InstanceVm = InstanceVm(UnsafeCell::new(None), Cell::new(false));
struct ExportGuard;
impl ExportGuard {
    fn enter() -> Result<Self, String> {
        if VM.1.replace(true) {
            return Err("interpreter exports must be serialized".into());
        }
        Ok(Self)
    }
}
impl Drop for ExportGuard {
    fn drop(&mut self) {
        VM.1.set(false);
    }
}
fn instance_vm() -> Option<Rc<Vm>> {
    unsafe { (&*VM.0.get()).clone() }
}
fn publish_vm(vm: Vm) {
    unsafe {
        *VM.0.get() = Some(Rc::new(vm));
    }
}
fn error(message: String) -> rquickjs::Error {
    rquickjs::Error::new_from_js_message("host", "operation", message)
}

/// Drive asynchronous effects outside the JavaScript heap, preserving the existing host transport.
async fn drive(vm: &Vm, packet: String) -> Result<String, String> {
    vm.start(packet)?;
    let mut pending = FuturesUnordered::new();
    loop {
        vm.pump()?;
        if let Some(result) = vm.outcome()? {
            return result;
        }
        for effect in vm.effects()? {
            pending.push(
                async move {
                    let ticket = host_async(effect.packet).await;
                    (effect.id, tembra::runtime::replies::take(ticket))
                }
                .boxed_local(),
            );
        }
        if pending.is_empty() {
            futures::future::pending::<()>().await;
        }
        if let Some((id, result)) = pending.next().await {
            vm.complete(id, result)?;
        }
    }
}
impl Guest for Component {
    fn has_interrupted() -> bool {
        instance_vm().is_some_and(|vm| vm.interrupted.get())
    }
    fn interrupt() {
        if let Some(vm) = instance_vm() {
            vm.interrupted.set(true);
            if let Some(handle) = vm.interrupt.borrow().as_ref() {
                handle.abort();
            }
        }
    }
    fn declare(packet: String) -> Result<String, String> {
        let _guard = ExportGuard::enter()?;
        instance_vm()
            .ok_or("Orb source is not loaded")?
            .declare(packet)
    }
    async fn load(packet: String) -> Result<String, String> {
        let _guard = ExportGuard::enter()?;
        if instance_vm().is_some() {
            return Err("the component already owns an Orb revision".into());
        }
        std::panic::set_hook(Box::new(|information| {
            let _=host_sync(&serde_json::json!({"operation":"console","level":"error","message":format!("interpreter panic: {information}")}).to_string());
        }));
        let vm = Vm::load(&packet)?;
        let description = vm.helper("__tembraDescribe", None)?;
        publish_vm(vm);
        Ok(description)
    }
    async fn dispatch(packet: String) -> Result<String, String> {
        let _guard = ExportGuard::enter()?;
        let vm = instance_vm().ok_or("Orb source is not loaded")?;
        let (handle, registration) = AbortHandle::new_pair();
        *vm.interrupt.borrow_mut() = Some(handle);
        let outcome = Abortable::new(drive(&vm, packet), registration)
            .await
            .unwrap_or_else(|_| Err("callback interrupted".into()));
        vm.interrupt.borrow_mut().take();
        vm.finish(outcome)
    }
    fn capture() -> Result<Vec<u8>, String> {
        let _guard = ExportGuard::enter()?;
        instance_vm().ok_or("Orb source is not loaded")?.capture()
    }
}
export!(Component);
