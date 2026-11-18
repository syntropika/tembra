//! A guest whose service sits in a private module and whose WIT resource, with a constructor, sits
//! in another module, which cannot name the service's native client. A component needs no client,
//! so the guest builds for wasm32; natively its `orb()` answers why a host cannot install it.

#[tembra::orb(package = "example:sett@1.0.0")]
pub mod sett {
    pub mod den {
        mod nest {
            use super::super::tags::Token;

            /// Hands out tokens, from a module only `den` can name.
            #[service]
            pub struct Tokens;

            impl Tokens {
                /// A token numbered `n`.
                pub fn open(n: u32) -> Token {
                    Token::new(n)
                }
            }
        }
    }

    pub mod tags {
        /// A numbered token, which `Tokens` hands out.
        #[wit_resource(service = Tokens)]
        pub struct Token {
            n: u32,
        }

        impl Token {
            /// A token numbered `n`: the resource's constructor.
            #[constructor]
            pub fn new(n: u32) -> Token {
                Token { n }
            }

            /// The token's number.
            pub fn number(&self) -> u32 {
                self.n
            }
        }
    }
}
