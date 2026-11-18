/// Contributes to the waterfall `guard` of the package in `admission/`, which another Orb declares.
/// Natively, the contribution needs the runtime's `hooks`: without it the module compiles all the
/// same, and its `orb(..)` answers an error naming the feature.
#[cfg_attr(
    feature = "no-std",
    tembra::orb(package = "example:contributor@1.0.0", wit = "admission", no_std)
)]
#[cfg_attr(
    not(feature = "no-std"),
    tembra::orb(package = "example:contributor@1.0.0", wit = "admission")
)]
pub mod contributor {
    /// Raise each admitted value by one.
    #[hook("example:admission/guard@1.0.0")]
    pub fn raise(input: guard::Admission) -> guard::Admission {
        guard::Admission {
            value: input.value + 1,
            blocked: input.blocked,
        }
    }
}

#[cfg(all(test, feature = "native"))]
mod tests {
    use super::contributor::{Providers, orb};
    use crate::native::facade::Provider;

    /// The Orb that declares the hook, for `orb(..)`.
    fn providers() -> Providers {
        Providers {
            guard: Provider::new("gate", 1),
        }
    }

    /// Without the runtime's `hooks`, the module compiles, and `orb(..)` answers why a host cannot
    /// install it, naming the feature.
    #[cfg(not(feature = "native-hooks"))]
    #[test]
    fn a_contribution_without_hooks_is_refused_natively() {
        assert_eq!(
            crate::native::refused(orb(providers())),
            "the contribution of `raise` to the hook example:admission/guard@1.0.0 needs the runtime's `hooks`: enable the facade's `hooks` feature in the host that installs this Orb"
        );
    }

    /// With the runtime's `hooks`, the contribution runs in the plan of a gate written by hand,
    /// which declares the hook with the signature of its WIT.
    #[cfg(feature = "native-hooks")]
    #[test]
    fn a_contribution_with_hooks_joins_the_hook_natively() {
        use crate::native::facade::{Hook, Host, Orb, Service, contracts::Contract};
        use futures::{FutureExt, executor::LocalPool};
        use serde_json::{Value, json};

        const GUARD: &str = "example:admission/guard@1.0.0";
        let source = format!(
            "{}\nworld gate {{ import guard; export guard; }}\n",
            include_str!("../admission/admission.wit")
        );
        let signature = Contract::from_source("gate.wit", &source, Some("gate"))
            .unwrap()
            .function_signature(GUARD, "apply", true)
            .unwrap();
        let hook = Hook::<Value, Value>::waterfall(GUARD).signature(signature);
        let declared = hook.clone();
        let gate = Orb::new("gate", 1).hook(hook).service(
            Service::new("gate::example:gate/admissions@1.0.0", move |declaration| {
                declaration.hook(&declared)
            })
            .method("admit", |hook, context, (value,): (i32,)| async move {
                hook.run(&context, json!({ "value": value, "blocked": false }))
                    .await
            }),
        );
        let mut pool = LocalPool::new();
        let host = Host::new(pool.spawner());
        host.install(vec![gate, orb(providers()).unwrap()]).unwrap();
        let admissions = std::rc::Rc::new(
            host.get("gate::example:gate/admissions@1.0.0", vec!["main".into()], "alice")
                .unwrap(),
        );
        pool.run_until(admissions.ready()).unwrap();
        let called = admissions.clone();
        let admitted = pool
            .run_until(host.root("alice", move |context| {
                async move { called.call::<_, Value>(&context, "admit", (5,)).await }.boxed_local()
            }))
            .unwrap();
        assert_eq!(admitted, json!({ "value": 6, "blocked": false }));
        admissions.release().unwrap();
        pool.run_until(host.close()).unwrap();
    }
}
