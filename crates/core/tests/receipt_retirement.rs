use futures::{
    FutureExt,
    channel::oneshot,
    future::{Either, select},
};
use serde_json::json;
use std::{
    cell::{Cell, RefCell},
    collections::BTreeMap,
    rc::Rc,
};
use tembra_core::{
    contracts::{Contract, Member},
    *,
};
struct SelectedCapture {
    context: Rc<RefCell<Option<Context>>>,
    dropped: Rc<Cell<bool>>,
}
impl Drop for SelectedCapture {
    fn drop(&mut self) {
        let context = self.context.borrow().clone().unwrap();
        assert_eq!(
            context.check(context.host_id()).unwrap_err().reason,
            "expired"
        );
        self.dropped.set(true);
    }
}
#[test]
fn final_selected_adapter_capture_retires_after_scope_expiry_before_receipt_wakes() {
    let driver = ManualDriver::new();
    let host = Host::new(HostOptions::new("receipt", json!(null)), driver.clone());
    let contract = |version| {
        Rc::new(
            Contract::local(
                "adapted",
                version,
                json!({}),
                BTreeMap::from([(
                    "run".into(),
                    Member::Method {
                        args: None,
                        result: None,
                    },
                )]),
            )
            .unwrap(),
        )
    };
    let consumer = contract(1);
    let provider = contract(2);
    let captured_context = Rc::new(RefCell::new(None));
    let dropped = Rc::new(Cell::new(false));
    let context = captured_context.clone();
    let observed = dropped.clone();
    let (send_entered, entered) = oneshot::channel();
    let send_entered = Rc::new(RefCell::new(Some(send_entered)));
    let (send_return, returning) = oneshot::channel();
    let returning = Rc::new(RefCell::new(Some(returning)));
    host.serve(
        provider,
        Rc::new(|_, _, _| {
            Ok(Implementation::new().method("run", |(): (), _| async { Ok(()) }.boxed_local()))
        }),
        ServeOptions::default().with_adapter(
            1,
            Rc::new(move |_| {
                let capture = Rc::new(SelectedCapture {
                    context: context.clone(),
                    dropped: observed.clone(),
                });
                let (context, entered, returning) =
                    (context.clone(), send_entered.clone(), returning.clone());
                Ok(Implementation::new()
                    .retain_owner(capture)
                    .method("run", move |(): (), ctx| {
                        *context.borrow_mut() = Some(ctx);
                        entered.borrow_mut().take().unwrap().send(()).unwrap();
                        let returning = returning.borrow_mut().take().unwrap();
                        async move {
                            returning.await.unwrap();
                            Ok(())
                        }
                        .boxed_local()
                    }))
            }),
        ),
    )
    .unwrap();
    let reference = driver
        .run_until(host.get(consumer, json!({}), GetOptions::default()))
        .unwrap();
    let call = reference.call_tracked::<(), ()>("run", (), None);
    let receipt = call.receipt();
    let Either::Right((_, call)) = driver.run_until(select(call, entered)) else {
        panic!("handler must await external settlement")
    };
    reference.release();
    assert!(!dropped.get());
    send_return.send(()).unwrap();
    driver.run_until(call).unwrap();
    driver.run_until(receipt.settled());
    assert!(dropped.get());
    driver.run_until(host.close()).unwrap();
}
