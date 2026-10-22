//! Implementation-owned preparation with the ordinary hook runners as execution authority.
use std::panic::AssertUnwindSafe;

use futures::FutureExt;
use tembra_core::{Context, Declaration, RegisteredOperation, Result, WeakImplementationOwner};

use crate::{
    BailOutcome, HookManager, HookRun, HookTerminal, RustHookDefinition, Strategy, invalid,
};

struct Metadata<A, R> {
    manager: HookManager,
    definition: RustHookDefinition<A, R>,
}

struct Prepared<A: 'static, R: 'static> {
    owner: WeakImplementationOwner<Metadata<A, R>>,
}

impl<A: 'static, R: 'static> Clone for Prepared<A, R> {
    fn clone(&self) -> Self {
        Self {
            owner: self.owner.clone(),
        }
    }
}

impl<A: 'static, R: 'static> Prepared<A, R> {
    fn run<T: 'static>(
        &self,
        context: &Context,
        build: impl FnOnce(&Metadata<A, R>, &Context) -> HookRun<T> + 'static,
    ) -> HookRun<T> {
        let owner = self.owner.clone();
        let context = context.clone();
        HookRun::deferred(
            async move {
                let metadata = owner.get(&context)?;
                let result = build(&metadata, &context).await;
                drop(metadata);
                result
            }
            .boxed_local(),
        )
    }
}

/// Weak access to one implementation's eagerly prepared series hook.
///
/// Clones retain no Host, receiver, implementation or metadata. Each polled run checks
/// the actual Context and exact installed owner before invoking the ordinary runner.
/// Current contributions, value checks, cancellation and physical settlement remain
/// governed by that runner; abandoning its observer does not prove termination.
pub struct PreparedSeries<A: Clone + 'static> {
    inner: Prepared<A, ()>,
}

impl<A: Clone + 'static> Clone for PreparedSeries<A> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
        }
    }
}

/// Weak access to one implementation's eagerly prepared bail hook.
///
/// Clones retain no Host, receiver, implementation or metadata. Each polled run checks
/// the actual Context and exact installed owner before invoking the ordinary runner.
/// Current contributions, value checks, cancellation and physical settlement remain
/// governed by that runner; abandoning its observer does not prove termination.
pub struct PreparedBail<A: Clone + 'static, R: 'static> {
    inner: Prepared<A, R>,
}

impl<A: Clone + 'static, R: 'static> Clone for PreparedBail<A, R> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
        }
    }
}

/// Weak access to one implementation's eagerly prepared waterfall hook.
///
/// Clones retain no Host, receiver, implementation or metadata. Each polled run checks
/// the actual Context and exact installed owner before invoking the ordinary runner.
/// Current contributions, value checks, cancellation and physical settlement remain
/// governed by that runner; abandoning its observer does not prove termination.
///
/// ```
/// use std::{cell::RefCell, collections::BTreeMap, rc::Rc};
/// use futures::FutureExt;
/// use serde_json::json;
/// use tembra_core::{contracts::Contract, *};
/// use tembra_hooks::*;
/// let driver=ManualDriver::new();
/// let host=Host::new(HostOptions::new("declared", json!(null)),driver.clone());
/// let saved=Rc::new(RefCell::new(None)); let target=saved.clone();
/// let facet=NativeFacet::new("scope",vec![],move|scope,_| { *target.borrow_mut()=Some(scope.clone()); Ok(()) })?;
/// driver.run_until(host.install(facet,LoadOptions::default())?)?;
/// let manager=HookManager::new(saved.borrow_mut().take().unwrap(),driver.clone(),HookManagerOptions::default())?;
/// let token=Rc::new(Contract::local("consumer",1,json!({}),BTreeMap::new())?);
/// let handle=Rc::new(RefCell::new(None)); let target=handle.clone();
/// host.serve(token.clone(),Rc::new(move|_,_,declaration| {
///     *target.borrow_mut()=Some(manager.declare_waterfall(declaration,"before",|| {
///         let input=HookType::<i32>::serde(json!({"type":"integer","minimum":0}))?;
///         let descriptor=HookDescriptor::new("before",1,Strategy::Waterfall,input.profile().clone(),input.profile().clone(),None)?;
///         RustHookDefinition::from_descriptor(descriptor,input.clone(),input)
///     })?);
///     Ok(Implementation::new())
/// }),ServeOptions::default())?;
/// let reference=driver.run_until(host.get(token,json!({}),GetOptions::default()))?;
/// let prepared=handle.borrow_mut().take().unwrap(); let current=prepared.clone();
/// driver.run_until(host.root(move|context| async move {
///     assert_eq!(current.run(7,&context).await?,7); // Empty waterfall preserves input.
///     assert_eq!(current.run(-1,&context).await.unwrap_err().reason,"hook_value");
///     Ok(())
/// }.boxed_local()))?;
/// reference.release(); driver.run_until_stalled();
/// let refusal=driver.run_until(host.root(move|context|prepared.run(0,&context).boxed_local())).unwrap_err();
/// assert_eq!(refusal.code,ErrorCode::Gone);
/// driver.run_until(host.close())?;
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
pub struct PreparedWaterfall<A: 'static> {
    inner: Prepared<A, A>,
}

impl<A: 'static> Clone for PreparedWaterfall<A> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
        }
    }
}

/// Weak access to one implementation's eagerly prepared middleware hook.
///
/// Clones retain no Host, receiver, implementation or metadata. Each polled run checks
/// the actual Context and exact installed owner before invoking the ordinary runner.
/// Current contributions, value checks, cancellation and physical settlement remain
/// governed by that runner; abandoning its observer does not prove termination.
pub struct PreparedMiddleware<A: 'static, R: 'static> {
    inner: Prepared<A, R>,
}

impl<A: 'static, R: 'static> Clone for PreparedMiddleware<A, R> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
        }
    }
}

/// Weak access to one implementation's eagerly prepared parallel hook.
///
/// Clones retain no Host, receiver, implementation or metadata. Each polled run checks
/// the actual Context and exact installed owner before invoking the ordinary runner.
/// Current contributions, value checks, cancellation and physical settlement remain
/// governed by that runner; abandoning its observer does not prove termination.
pub struct PreparedParallel<A: Clone + 'static, R: 'static> {
    inner: Prepared<A, R>,
}

impl<A: Clone + 'static, R: 'static> Clone for PreparedParallel<A, R> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
        }
    }
}

impl HookManager {
    /// Prepares a series definition once during this exact factory pass.
    ///
    /// The producer runs eagerly outside runtime borrows. Labels are diagnostic text,
    /// never cache identities; duplicate labels prepare independent owners.
    /// Only the successful implementation retains the result, not this weak handle.
    ///
    /// # Errors
    /// Refuses a foreign/expired declaration before the producer, wrong strategy,
    /// producer error/panic (`hook_panic`), or ended pass after producer effects.
    /// The returned metadata is not adopted on refusal. Successful nested declarations
    /// and external effects are not rolled back.
    pub fn declare_series<A: Clone + 'static>(
        &self,
        declaration: &mut Declaration,
        label: &str,
        build: impl FnOnce() -> Result<RustHookDefinition<A, ()>>,
    ) -> Result<PreparedSeries<A>> {
        let owner = declaration.prepare_implementation_owner(&self.scope, || {
            let definition =
                std::panic::catch_unwind(AssertUnwindSafe(build)).unwrap_or_else(|_| {
                    Err(invalid(
                        "hook_panic",
                        format!("hook producer panicked: {label}"),
                    ))
                })?;
            if definition.descriptor().strategy() != Strategy::Series {
                return Err(invalid("hook_strategy", "wrong declaration strategy"));
            }
            Ok(Metadata {
                manager: self.clone(),
                definition,
            })
        })?;
        Ok(PreparedSeries {
            inner: Prepared { owner },
        })
    }

    /// Prepares a bail definition once during this exact factory pass.
    ///
    /// The producer runs eagerly outside runtime borrows. Labels are diagnostic text,
    /// never cache identities; duplicate labels prepare independent owners.
    /// Only the successful implementation retains the result, not this weak handle.
    ///
    /// # Errors
    /// Refuses a foreign/expired declaration before the producer, wrong strategy,
    /// producer error/panic (`hook_panic`), or ended pass after producer effects.
    /// The returned metadata is not adopted on refusal. Successful nested declarations
    /// and external effects are not rolled back.
    pub fn declare_bail<A: Clone + 'static, R: 'static>(
        &self,
        declaration: &mut Declaration,
        label: &str,
        build: impl FnOnce() -> Result<RustHookDefinition<A, R>>,
    ) -> Result<PreparedBail<A, R>> {
        let owner = declaration.prepare_implementation_owner(&self.scope, || {
            let definition =
                std::panic::catch_unwind(AssertUnwindSafe(build)).unwrap_or_else(|_| {
                    Err(invalid(
                        "hook_panic",
                        format!("hook producer panicked: {label}"),
                    ))
                })?;
            if definition.descriptor().strategy() != Strategy::Bail {
                return Err(invalid("hook_strategy", "wrong declaration strategy"));
            }
            Ok(Metadata {
                manager: self.clone(),
                definition,
            })
        })?;
        Ok(PreparedBail {
            inner: Prepared { owner },
        })
    }

    /// Prepares a waterfall definition once during this exact factory pass.
    ///
    /// The producer runs eagerly outside runtime borrows. Labels are diagnostic text,
    /// never cache identities; duplicate labels prepare independent owners.
    /// Only the successful implementation retains the result, not this weak handle.
    ///
    /// # Errors
    /// Refuses a foreign/expired declaration before the producer, wrong strategy,
    /// producer error/panic (`hook_panic`), or ended pass after producer effects.
    /// The returned metadata is not adopted on refusal. Successful nested declarations
    /// and external effects are not rolled back.
    pub fn declare_waterfall<A: 'static>(
        &self,
        declaration: &mut Declaration,
        label: &str,
        build: impl FnOnce() -> Result<RustHookDefinition<A, A>>,
    ) -> Result<PreparedWaterfall<A>> {
        let owner = declaration.prepare_implementation_owner(&self.scope, || {
            let definition =
                std::panic::catch_unwind(AssertUnwindSafe(build)).unwrap_or_else(|_| {
                    Err(invalid(
                        "hook_panic",
                        format!("hook producer panicked: {label}"),
                    ))
                })?;
            if definition.descriptor().strategy() != Strategy::Waterfall {
                return Err(invalid("hook_strategy", "wrong declaration strategy"));
            }
            Ok(Metadata {
                manager: self.clone(),
                definition,
            })
        })?;
        Ok(PreparedWaterfall {
            inner: Prepared { owner },
        })
    }

    /// Prepares a middleware definition once during this exact factory pass.
    ///
    /// The producer runs eagerly outside runtime borrows. Labels are diagnostic text,
    /// never cache identities; duplicate labels prepare independent owners.
    /// Only the successful implementation retains the result, not this weak handle.
    ///
    /// # Errors
    /// Refuses a foreign/expired declaration before the producer, wrong strategy,
    /// producer error/panic (`hook_panic`), or ended pass after producer effects.
    /// The returned metadata is not adopted on refusal. Successful nested declarations
    /// and external effects are not rolled back.
    pub fn declare_middleware<A: 'static, R: 'static>(
        &self,
        declaration: &mut Declaration,
        label: &str,
        build: impl FnOnce() -> Result<RustHookDefinition<A, R>>,
    ) -> Result<PreparedMiddleware<A, R>> {
        let owner = declaration.prepare_implementation_owner(&self.scope, || {
            let definition =
                std::panic::catch_unwind(AssertUnwindSafe(build)).unwrap_or_else(|_| {
                    Err(invalid(
                        "hook_panic",
                        format!("hook producer panicked: {label}"),
                    ))
                })?;
            if definition.descriptor().strategy() != Strategy::Middleware {
                return Err(invalid("hook_strategy", "wrong declaration strategy"));
            }
            Ok(Metadata {
                manager: self.clone(),
                definition,
            })
        })?;
        Ok(PreparedMiddleware {
            inner: Prepared { owner },
        })
    }

    /// Prepares a parallel definition once during this exact factory pass.
    ///
    /// The producer runs eagerly outside runtime borrows. Labels are diagnostic text,
    /// never cache identities; duplicate labels prepare independent owners.
    /// Only the successful implementation retains the result, not this weak handle.
    ///
    /// # Errors
    /// Refuses a foreign/expired declaration before the producer, wrong strategy,
    /// producer error/panic (`hook_panic`), or ended pass after producer effects.
    /// The returned metadata is not adopted on refusal. Successful nested declarations
    /// and external effects are not rolled back.
    pub fn declare_parallel<A: Clone + 'static, R: 'static>(
        &self,
        declaration: &mut Declaration,
        label: &str,
        build: impl FnOnce() -> Result<RustHookDefinition<A, R>>,
    ) -> Result<PreparedParallel<A, R>> {
        let owner = declaration.prepare_implementation_owner(&self.scope, || {
            let definition =
                std::panic::catch_unwind(AssertUnwindSafe(build)).unwrap_or_else(|_| {
                    Err(invalid(
                        "hook_panic",
                        format!("hook producer panicked: {label}"),
                    ))
                })?;
            if definition.descriptor().strategy() != Strategy::Parallel {
                return Err(invalid("hook_strategy", "wrong declaration strategy"));
            }
            Ok(Metadata {
                manager: self.clone(),
                definition,
            })
        })?;
        Ok(PreparedParallel {
            inner: Prepared { owner },
        })
    }
}

impl<A: Clone + 'static> PreparedSeries<A> {
    /// Runs this call's current value through the existing series pipeline.
    /// # Errors
    /// Owner/Context admission refuses before pipeline entry; subsequent runner errors
    /// retain their ordinary order and may follow handler or codec effects.
    pub fn run(&self, input: A, context: &Context) -> HookRun<()> {
        self.inner.run(context, move |metadata, context| {
            metadata
                .manager
                .run_dynamic_series(&metadata.definition, input, context)
        })
    }
    /// Runs the ordinary series pipeline followed by the supplied current terminal.
    /// # Errors
    /// Refuses owner, Context, terminal or plan mismatch before handler entry;
    /// later value/handler failures may follow effects and skip the terminal.
    pub fn run_then<T: 'static>(
        &self,
        input: A,
        context: &Context,
        terminal: HookTerminal<A, T>,
    ) -> HookRun<T> {
        self.inner.run(context, move |metadata, context| {
            metadata
                .manager
                .run_dynamic_series_then(&metadata.definition, input, context, terminal)
        })
    }
}

impl<A: Clone + 'static, R: 'static> PreparedBail<A, R> {
    /// Runs this call's current value through the existing bail pipeline.
    /// # Errors
    /// Owner/Context admission refuses before pipeline entry; subsequent runner errors
    /// retain their ordinary order and may follow handler or codec effects.
    pub fn run(
        &self,
        input: A,
        context: &Context,
        terminal: Option<HookTerminal<A, R>>,
    ) -> HookRun<BailOutcome<R>> {
        self.inner.run(context, move |metadata, context| {
            metadata
                .manager
                .run_dynamic_bail(&metadata.definition, input, context, terminal)
        })
    }
}

impl<A: 'static> PreparedWaterfall<A> {
    /// Runs this call's current value through the existing waterfall pipeline.
    /// # Errors
    /// Owner/Context admission refuses before pipeline entry; subsequent runner errors
    /// retain their ordinary order and may follow handler or codec effects.
    pub fn run(&self, input: A, context: &Context) -> HookRun<A> {
        self.inner.run(context, move |metadata, context| {
            metadata
                .manager
                .run_dynamic_waterfall(&metadata.definition, input, context)
        })
    }
    /// Runs the ordinary waterfall pipeline followed by the supplied current terminal.
    /// # Errors
    /// Refuses owner, Context, terminal or plan mismatch before handler entry;
    /// later value/handler failures may follow effects and skip the terminal.
    pub fn run_then<T: 'static>(
        &self,
        input: A,
        context: &Context,
        terminal: HookTerminal<A, T>,
    ) -> HookRun<T> {
        self.inner.run(context, move |metadata, context| {
            metadata.manager.run_dynamic_waterfall_then(
                &metadata.definition,
                input,
                context,
                terminal,
            )
        })
    }
}

impl<A: 'static, R: 'static> PreparedMiddleware<A, R> {
    /// Runs this call's current value through the existing middleware pipeline.
    /// # Errors
    /// Owner/Context admission refuses before pipeline entry; subsequent runner errors
    /// retain their ordinary order and may follow handler or codec effects.
    pub fn run(&self, input: A, context: &Context, terminal: HookTerminal<A, R>) -> HookRun<R> {
        self.inner.run(context, move |metadata, context| {
            metadata
                .manager
                .run_dynamic_middleware(&metadata.definition, input, context, terminal)
        })
    }
    /// Adapts the supplied actual native operation using retained checked types.
    /// # Errors
    /// Owner admission precedes adapter construction. Signature/admission/runner errors
    /// are returned when polled; the operation is never cached between calls.
    pub fn run_native(
        &self,
        input: A,
        context: &Context,
        operation: RegisteredOperation,
    ) -> HookRun<R> {
        self.inner.run(context, move |metadata, context| {
            let terminal = HookTerminal::new(
                operation,
                metadata.definition.input.clone(),
                metadata.definition.result.clone(),
            );
            match terminal {
                Ok(terminal) => metadata.manager.run_dynamic_middleware(
                    &metadata.definition,
                    input,
                    context,
                    terminal,
                ),
                Err(error) => HookRun::deferred(async move { Err(error) }.boxed_local()),
            }
        })
    }

    /// Adapts the supplied actual portable operation with current presence-preserving codecs.
    ///
    /// Callers select this operation from their current Context's registry. Neither the
    /// operation nor codec results are retained across calls. Codecs and value validators
    /// preserve their existing effects and failure ordering.
    /// # Errors
    /// Refuses owner, Context or profile mismatch before terminal entry; later codec,
    /// handler or value errors can follow effects. Errors arrive when polled.
    pub fn run_portable(
        &self,
        input: A,
        context: &Context,
        operation: RegisteredOperation,
        encode: impl Fn(A) -> Result<Option<serde_json::Value>> + 'static,
        decode: impl Fn(Option<serde_json::Value>) -> Result<R> + 'static,
    ) -> HookRun<R> {
        self.inner.run(context, move |metadata, context| {
            let terminal = HookTerminal::portable(
                operation,
                metadata.definition.input.clone(),
                metadata.definition.result.clone(),
                encode,
                decode,
            );
            match terminal {
                Ok(terminal) => metadata.manager.run_dynamic_middleware(
                    &metadata.definition,
                    input,
                    context,
                    terminal,
                ),
                Err(error) => HookRun::deferred(async move { Err(error) }.boxed_local()),
            }
        })
    }
}

impl<A: Clone + 'static, R: 'static> PreparedParallel<A, R> {
    /// Runs this call's current value through the existing parallel pipeline.
    /// # Errors
    /// Owner/Context admission refuses before pipeline entry; subsequent runner errors
    /// retain their ordinary order and may follow handler or codec effects.
    pub fn run(&self, input: A, context: &Context) -> HookRun<Vec<R>> {
        self.inner.run(context, move |metadata, context| {
            metadata
                .manager
                .run_dynamic_parallel(&metadata.definition, input, context)
        })
    }
    /// Runs the ordinary parallel pipeline followed by the supplied current terminal.
    /// # Errors
    /// Refuses owner, Context, terminal or plan mismatch before handler entry;
    /// later value/handler failures may follow effects and skip the terminal.
    pub fn run_then<T: 'static>(
        &self,
        input: A,
        context: &Context,
        terminal: HookTerminal<Vec<R>, T>,
    ) -> HookRun<T> {
        self.inner.run(context, move |metadata, context| {
            metadata.manager.run_dynamic_parallel_then(
                &metadata.definition,
                input,
                context,
                terminal,
            )
        })
    }
}
