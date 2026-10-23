//! The five hook strategies in one story: a mailer runs one hook of each kind while sending.
//!
//! 1. Waterfall  `mail.before-send`   plugins TRANSFORM the text, each receiving the last result
//! 2. Bail       `mail.spam-check`    the first plugin that says Stop DECIDES: a veto
//! 3. Parallel   `mail.suggest-tags`  every plugin ANSWERS and the owner collects the answers
//! 4. Middleware `mail.deliver`       plugins WRAP the real delivery: before, after, or skip it
//! 5. Series     `mail.sent`          every plugin is NOTIFIED; nothing is returned

#[path = "../helpers/hooks.rs"]
mod helpers;

use helpers::{
    define, middleware, ready, register_plugin, register_terminal, select_terminal,
    terminal_registry, text, waterfall,
};
use tembra::FacetDeclaration;
use tembra::core::{FacetScope, Registry};
use tembra::futures::FutureExt;
use tembra::prelude::*;
use tembra::serde_json::json;
use tembra_hooks::{
    BailDecision, BailOutcome, HookManager, HookType, Next, PreparedBail, PreparedMiddleware,
    PreparedParallel, PreparedSeries, PreparedWaterfall, RustHookDefinition, Strategy,
};

// 1. The definitions: a name and a strategy each. Every hook carries a string.
fn before_send() -> Result<RustHookDefinition<String, String>> {
    waterfall("mail.before-send")
}

fn spam_check() -> Result<RustHookDefinition<String, bool>> {
    let flag = HookType::serde(json!({ "type": "boolean" }))?;
    define("mail.spam-check", Strategy::Bail, flag, None)
}

fn suggest_tags() -> Result<RustHookDefinition<String, String>> {
    define("mail.suggest-tags", Strategy::Parallel, text()?, None)
}

fn deliver() -> Result<RustHookDefinition<String, String>> {
    middleware("mail.deliver")
}

fn sent() -> Result<RustHookDefinition<String, ()>> {
    define("mail.sent", Strategy::Series, HookType::absent(), None)
}

/// The registry in which the transport publishes the real delivery: the middleware terminal.
fn transport_registry() -> Result<Registry> {
    terminal_registry("mail.transport")
}

// 2. The owner runs the five hooks, in order, inside `send`.
#[tembra::contract(local, name = "example.mailer", version = 1)]
trait Mailer {
    async fn send(&self, subject: String) -> Result<String>;
}

struct MailerService {
    scope: FacetScope,
    before_send: PreparedWaterfall<String>,
    spam_check: PreparedBail<String, bool>,
    suggest_tags: PreparedParallel<String, String>,
    deliver: PreparedMiddleware<String, String>,
    sent: PreparedSeries<String>,
}

impl MailerProvider for MailerService {
    async fn send(&self, subject: String, context: Context) -> Result<String> {
        let context = context.core();

        println!("  1. WATERFALL mail.before-send");
        let subject = self.before_send.run(subject, context).await?;
        println!("     -> final text {subject:?}");

        println!("  2. BAIL mail.spam-check");
        match self.spam_check.run(subject.clone(), context, None).await? {
            BailOutcome::Resolved(true) => {
                println!("     -> BLOCKED by a plugin");
                return Err(RuntimeError::invalid("spam", "blocked by a plugin"));
            }
            BailOutcome::Resolved(false) => println!("     -> explicitly approved"),
            BailOutcome::Unresolved => println!("     -> nobody objected"),
        }

        println!("  3. PARALLEL mail.suggest-tags");
        let tags = self.suggest_tags.run(subject.clone(), context).await?;
        println!("     -> collected tags {tags:?}");

        println!("  4. MIDDLEWARE mail.deliver");
        // The terminal is selected now, under the current context. It is never cached.
        let transport = select_terminal(&self.scope, &transport_registry()?, "default", context)?;
        let receipt = self
            .deliver
            .run_native(subject.clone(), context, transport)
            .await?;
        println!("     -> {receipt:?}");

        println!("  5. SERIES mail.sent");
        self.sent.run(subject, context).await?;
        Ok(receipt)
    }
}

// 3. The plugins, grouped by strategy. Each handler receives the input and, for middleware,
//    the `next` continuation, and returns the type its strategy requires.
fn install_plugins(setup: &mut FacetDeclaration<'_, '_>) -> Result<()> {
    // Waterfall: return the NEW text. One plugin's output is the next plugin's input.
    register_plugin(setup, before_send()?, "trim", 0, |text, _| {
        ready(Ok(text.trim().to_string()))
    })?;
    register_plugin(setup, before_send()?, "prefix", 10, |text, _| {
        ready(Ok(format!("[app] {text}")))
    })?;

    // Bail: return Continue ("not my business") or Stop(value) ("my decision, stop here").
    register_plugin(setup, spam_check()?, "blocklist", 0, |text, _| {
        let decision = if text.contains("lottery") {
            BailDecision::Stop(true)
        } else {
            BailDecision::Continue
        };
        ready(Ok(decision))
    })?;
    register_plugin(setup, spam_check()?, "length", 5, |text, _| {
        let decision = if text.len() > 40 {
            BailDecision::Stop(true)
        } else {
            BailDecision::Continue
        };
        ready(Ok(decision))
    })?;

    // Parallel: every plugin returns its own answer; the owner receives all of them.
    register_plugin(setup, suggest_tags()?, "by-length", 0, |text, _| {
        let tag = if text.len() > 12 { "long" } else { "short" };
        ready(Ok(tag.to_string()))
    })?;
    register_plugin(setup, suggest_tags()?, "by-keyword", 1, |text, _| {
        let tag = if text.contains("discount") {
            "promo"
        } else {
            "regular"
        };
        ready(Ok(tag.to_string()))
    })?;

    // Middleware: the handler also receives `next`. It can act BEFORE, call next, act AFTER,
    // or answer by itself without calling next. A lower priority is an outer layer.
    register_plugin(
        setup,
        deliver()?,
        "audit",
        0,
        |text, next: Option<Next<String, String>>| {
            async move {
                println!("      [audit] before delivering {text:?}");
                let result = next
                    .expect("middleware handlers receive next")
                    .call(text)
                    .await?;
                println!("      [audit] after delivery: {result:?}");
                Ok(result)
            }
            .boxed_local()
        },
    )?;
    register_plugin(
        setup,
        deliver()?,
        "dry-run",
        10,
        |text, next: Option<Next<String, String>>| {
            async move {
                if text.contains("test") {
                    println!("      [dry-run] a test: NOT calling next, answering myself");
                    return Ok("not sent (dry run)".to_string());
                }
                println!("      [dry-run] not a test: calling next");
                next.expect("middleware handlers receive next")
                    .call(text)
                    .await
            }
            .boxed_local()
        },
    )?;

    // Series: return nothing. Side effects only: logs, metrics, notifications.
    register_plugin(setup, sent()?, "log", 0, |text, _| {
        println!("      [log] sent: {text:?}");
        ready(Ok(()))
    })?;
    register_plugin(setup, sent()?, "metrics", 1, |_, _| {
        println!("      [metrics] mail counter +1");
        ready(Ok(()))
    })
}

/// The real delivery: the terminal at the centre of the middleware.
fn deliver_for_real(subject: String) -> String {
    println!("      [transport] actually delivering {subject:?}");
    format!("delivered: {subject}")
}

fn main() -> Result<()> {
    // The hook manager needs the scheduler, so the driver is created here and shared with the
    // Host instead of letting `tembra::native::run` create one privately.
    let driver = tembra::ManualDriver::new();
    let host = Host::with_driver("hook-strategies-example", (), driver.clone())?;
    let scheduler = driver.clone();
    let facet = NativeFacet::new("mail", vec![], move |scope, setup| {
        let manager = HookManager::new(scope.clone(), scheduler.clone(), Default::default())?;
        let scope = scope.clone();
        setup.serve(Mailer, move |_, _, declaration| {
            let core = declaration.core_mut();
            Ok(MailerService {
                scope: scope.clone(),
                before_send: manager.declare_waterfall(core, "before-send", before_send)?,
                spam_check: manager.declare_bail(core, "spam-check", spam_check)?,
                suggest_tags: manager.declare_parallel(core, "suggest-tags", suggest_tags)?,
                deliver: manager.declare_middleware(core, "deliver", deliver)?,
                sent: manager.declare_series(core, "sent", sent)?,
            })
        })?;
        register_terminal(setup, &transport_registry()?, "default", deliver_for_real)?;
        install_plugins(setup)
    })?;

    driver.run_until(async move {
        host.install(facet, Default::default())?.await?;
        let mailer = host.get(Mailer, MailerInput {}).await?;
        for subject in ["   hello   ", "you won the lottery", "   test discount   "] {
            println!("\n=== send({subject:?}) ===");
            match mailer.send(subject.to_string()).await {
                Ok(receipt) => println!("=== outcome: Ok({receipt:?}) ==="),
                Err(error) => {
                    println!("=== outcome: Err({}: {}) ===", error.reason, error.message);
                }
            }
        }
        drop(mailer);
        host.close().await
    })
}
