use crate::{HookArgs, invalid};
use futures::{FutureExt, future::LocalBoxFuture};
use std::rc::Rc;
use tembra_core::{Context, Result};

pub type MethodHandler<A, H> = Rc<dyn Fn(A, Context) -> LocalBoxFuture<'static, Result<H>>>;
pub type MethodWrapper<A, H> =
    Rc<dyn Fn(A, Context, MethodHandler<A, H>) -> LocalBoxFuture<'static, Result<H>>>;
/// Wraps a typed managed handler, joining its continuations before receiver scope expiry.
pub fn hook_handler<A: 'static, H: 'static>(
    handler: MethodHandler<HookArgs<A, H>, H>,
) -> MethodHandler<HookArgs<A, H>, H> {
    let wrapper = hook_wrapper();
    Rc::new(move |args, context| wrapper(args, context, handler.clone()))
}
/// Supplies the same explicit typed wrapper to a foreign whole-method adapter.
pub fn hook_wrapper<A: 'static, H: 'static>() -> MethodWrapper<HookArgs<A, H>, H> {
    Rc::new(|args, context, method| {
        async move {
            let frame = args
                .frame
                .upgrade()
                .ok_or_else(|| invalid("hook_next_scope", "handler frame ended"))?;
            frame.bind(&context)?;
            let result = method(args, context).await;
            frame.seal(result.as_ref().err());
            let joined = frame.join().await;
            let value = result?;
            joined?;
            frame.result.check(&value)?;
            Ok(value)
        }
        .boxed_local()
    })
}
