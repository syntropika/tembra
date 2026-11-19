/// Types that stay the author's: a record declared in a sub-module with a private field, a field
/// `r#type` and a case `HTTP`, which the bindings spell `type_` and `Http`, and a function's error
/// enum with a hand `impl Display`, which the bindings implement for their own error type too. The
/// macro converts each to and from the bindings' type at the boundary, in the type's module.
#[cfg_attr(
    feature = "no-std",
    tembra::orb(package = "example:authored@1.0.0", no_std)
)]
#[cfg_attr(
    not(feature = "no-std"),
    tembra::orb(package = "example:authored@1.0.0")
)]
pub mod requests {
    pub mod model {
        use alloc::string::String;

        /// A request: its path, its scheme and how often it was routed, which only this module
        /// counts.
        #[derive(Debug, Clone, PartialEq, Eq)]
        pub struct Request {
            pub path: String,
            pub r#type: Scheme,
            seen: u32,
        }

        impl Request {
            /// A request of `path` over `r#type`, never routed.
            pub fn new(path: String, r#type: Scheme) -> Self {
                Request {
                    path,
                    r#type,
                    seen: 0,
                }
            }

            /// How often the request was routed.
            pub fn seen(&self) -> u32 {
                self.seen
            }

            /// The request, routed once more, over `r#type`.
            pub fn routed(mut self, r#type: Scheme) -> Self {
                self.r#type = r#type;
                self.seen += 1;
                self
            }
        }

        /// The scheme of a request.
        #[derive(Debug, Clone, Copy, PartialEq, Eq)]
        pub enum Scheme {
            HTTP,
            Https,
        }
    }

    pub mod router {
        use super::model;
        use alloc::{format, string::String};

        /// Why the router refuses a request.
        #[derive(Debug, Clone, PartialEq, Eq)]
        pub enum Refusal {
            /// The path is empty.
            Empty,
            /// The path is longer than the router accepts.
            TooLong { length: u32 },
            /// The request was routed as often as the router allows.
            Stale { seen: u32 },
        }

        impl core::fmt::Display for Refusal {
            fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
                match self {
                    Refusal::Empty => f.write_str("the path is empty"),
                    Refusal::TooLong { length } => write!(f, "the path has {length} characters"),
                    Refusal::Stale { seen } => write!(f, "the request was routed {seen} times"),
                }
            }
        }

        /// Routes requests.
        #[service]
        pub struct Router;

        impl Router {
            /// A request of `path` over `HTTP`, never routed.
            pub fn fresh(path: String) -> model::Request {
                model::Request::new(path, model::Scheme::HTTP)
            }

            /// Route `request`: refused when its path is empty or longer than eight characters, or
            /// when it was routed three times already, otherwise routed once more, over `Https`
            /// when the path starts with `secure`.
            pub fn route(request: model::Request) -> Result<model::Request, Refusal> {
                let length = request.path.chars().count() as u32;
                if length == 0 {
                    return Err(Refusal::Empty);
                }
                if length > 8 {
                    return Err(Refusal::TooLong { length });
                }
                if request.seen() >= 3 {
                    return Err(Refusal::Stale {
                        seen: request.seen(),
                    });
                }
                let r#type = if request.path.starts_with("secure") {
                    model::Scheme::Https
                } else {
                    request.r#type
                };
                Ok(request.routed(r#type))
            }

            /// The words of a refusal, as its `Display` writes them.
            pub fn explain(refusal: Refusal) -> String {
                format!("{refusal}")
            }
        }
    }
}

#[cfg(all(test, feature = "native"))]
mod tests {
    use super::requests::{
        self,
        model::{Request, Scheme},
        router::Refusal,
    };
    use crate::native::{call, hosting, ready};
    use futures::FutureExt;
    use serde_json::json;

    /// The Orb routes the author's types natively, through the client and portably: the record's
    /// private field crosses as `seen`, its field `r#type` as `type` and the case `HTTP` as `http`,
    /// and the refusal's words come from the hand `impl Display`.
    #[test]
    fn the_authors_types_answer_natively() {
        let (mut pool, host) = hosting(requests::orb().unwrap());
        let reference = ready(&mut pool, &host, "example:authored/router@1.0.0");
        let client = requests::router::RouterClient::new(reference.clone());
        let request = Request::new("secure".into(), Scheme::HTTP);
        let routed = pool
            .run_until(host.root("alice", move |context| {
                async move { client.route(&context, request).await }.boxed_local()
            }))
            .unwrap()
            .unwrap();
        assert_eq!(
            (routed.path.as_str(), routed.r#type, routed.seen()),
            ("secure", Scheme::Https, 1)
        );
        assert_eq!(
            call(
                &mut pool,
                &host,
                &reference,
                "alice",
                "route",
                json!([{"path": "plain", "type": "http", "seen": 2}])
            ),
            json!({"tag": "ok", "val": {"path": "plain", "type": "http", "seen": 3}})
        );
        assert_eq!(
            call(
                &mut pool,
                &host,
                &reference,
                "alice",
                "route",
                json!([{"path": "", "type": "https", "seen": 0}])
            ),
            json!({"tag": "err", "val": {"tag": "empty"}})
        );
        assert_eq!(
            call(
                &mut pool,
                &host,
                &reference,
                "alice",
                "route",
                json!([{"path": "much-too-long", "type": "https", "seen": 0}])
            ),
            json!({"tag": "err", "val": {"tag": "too-long", "val": {"length": 13}}})
        );
        assert_eq!(
            call(
                &mut pool,
                &host,
                &reference,
                "alice",
                "explain",
                json!([{"tag": "too-long", "val": {"length": 13}}])
            ),
            json!("the path has 13 characters")
        );
        assert_eq!(
            Refusal::TooLong { length: 13 }.to_string(),
            "the path has 13 characters"
        );
        reference.release().unwrap();
        pool.run_until(host.close()).unwrap();
    }
}
