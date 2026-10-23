//! Hooks are extension points identified by name. Two owners each run their own hook; a facet
//! installed later contributes plugins to both without either owner changing.

#[path = "../helpers/hooks.rs"]
mod helpers;

use helpers::{register_transform, waterfall};
use tembra::prelude::*;
use tembra_hooks::{HookManager, PreparedWaterfall, RustHookDefinition};

// 1. Two hook definitions. Only the name differs, and the name selects the registry.
fn mail_hook() -> Result<RustHookDefinition<String, String>> {
    waterfall("mail.before-send")
}

fn sms_hook() -> Result<RustHookDefinition<String, String>> {
    waterfall("sms.before-send")
}

// 2. Two owners. Each prepares its own pipeline in its factory and runs it in `send`.
#[tembra::contract(local, name = "example.mailer", version = 1)]
trait Mailer {
    async fn send(&self, subject: String) -> Result<String>;
}

struct MailerService {
    before_send: PreparedWaterfall<String>,
}

impl MailerProvider for MailerService {
    async fn send(&self, subject: String, context: Context) -> Result<String> {
        let subject = self.before_send.run(subject, context.core()).await?;
        println!("  [mailer] sending {subject:?}");
        Ok(subject)
    }
}

#[tembra::contract(local, name = "example.sms", version = 1)]
trait Sms {
    async fn send(&self, text: String) -> Result<String>;
}

struct SmsService {
    before_send: PreparedWaterfall<String>,
}

impl SmsProvider for SmsService {
    async fn send(&self, text: String, context: Context) -> Result<String> {
        let text = self.before_send.run(text, context.core()).await?;
        println!("  [sms] sending {text:?}");
        Ok(text)
    }
}

// 3. Plugin logic: plain string transformations.
fn trim(text: String) -> String {
    text.trim().to_string()
}

fn add_prefix(text: String) -> String {
    format!("[app] {text}")
}

fn shout(text: String) -> String {
    text.to_uppercase()
}

fn censor(text: String) -> String {
    text.replace("secret", "******")
}

fn main() -> Result<()> {
    // The hook manager needs the scheduler, so the driver is created here and shared with the
    // Host instead of letting `tembra::native::run` create one privately.
    let driver = tembra::ManualDriver::new();
    let host = Host::with_driver("hooks-example", (), driver.clone())?;

    // 4. The "mail" facet: the mailer and its built-in plugins. Registration order does not
    //    matter; priority does.
    let scheduler = driver.clone();
    let mail_facet = NativeFacet::new("mail", vec![], move |scope, setup| {
        let manager = HookManager::new(scope.clone(), scheduler.clone(), Default::default())?;
        setup.serve(Mailer, move |_, _, declaration| {
            let before_send =
                manager.declare_waterfall(declaration.core_mut(), "before-send", mail_hook)?;
            Ok(MailerService { before_send })
        })?;
        register_transform(setup, mail_hook()?, "prefix", 10, add_prefix)?;
        register_transform(setup, mail_hook()?, "trim", 0, trim)
    })?;

    // 5. The "sms" facet: its own hook, no built-in plugins.
    let scheduler = driver.clone();
    let sms_facet = NativeFacet::new("sms", vec![], move |scope, setup| {
        let manager = HookManager::new(scope.clone(), scheduler.clone(), Default::default())?;
        setup.serve(Sms, move |_, _, declaration| {
            let before_send =
                manager.declare_waterfall(declaration.core_mut(), "before-send", sms_hook)?;
            Ok(SmsService { before_send })
        })
    })?;

    // 6. An external facet. It knows nothing about the owners, only the hook definitions.
    let plugin_pack = NativeFacet::new("plugin-pack", vec![], |_, setup| {
        register_transform(setup, mail_hook()?, "shout", 20, shout)?;
        register_transform(setup, sms_hook()?, "censor", 0, censor)
    })?;

    driver.run_until(async move {
        host.install(mail_facet, Default::default())?.await?;
        host.install(sms_facet, Default::default())?.await?;
        let mailer = host.get(Mailer, MailerInput {}).await?;
        let sms = host.get(Sms, SmsInput {}).await?;

        println!("=== before plugin-pack ===");
        mailer.send("   hello   ".to_string()).await?;
        sms.send("a secret".to_string()).await?;

        println!("=== install plugin-pack while the Host is running ===");
        host.install(plugin_pack, Default::default())?.await?;

        println!("=== after plugin-pack: same references, owners unchanged ===");
        mailer.send("   hello   ".to_string()).await?;
        sms.send("a secret".to_string()).await?;

        drop((mailer, sms));
        host.close().await
    })
}
