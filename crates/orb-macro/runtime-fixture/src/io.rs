/// Reaches the host's configured providers through WASI: the filesystem's preopened directories, a
/// TCP connection and an HTTP request. The attribute imports `wasi:filesystem/preopens` and
/// `wasi:sockets/types`, whose packages both hold an interface named `types`: the module
/// re-exports the sockets' interface it imports as `types`, and reaches the filesystem's, which the
/// preopens bring, by the bindings' own path, as it does the HTTP types the client brings. An
/// installation grants a filesystem, a network destination and an HTTP origin.
#[cfg_attr(
    feature = "no-std",
    tembra::orb(
        package = "example:io@1.0.0",
        import = "wasi:filesystem/preopens@0.3.0",
        import = "wasi:sockets/types@0.3.0",
        import = "wasi:http/client@0.3.0",
        no_std
    )
)]
#[cfg_attr(
    not(feature = "no-std"),
    tembra::orb(
        package = "example:io@1.0.0",
        import = "wasi:filesystem/preopens@0.3.0",
        import = "wasi:sockets/types@0.3.0",
        import = "wasi:http/client@0.3.0"
    )
)]
pub mod io {
    pub mod outside {
        use super::{client, preopens, types, wasi, wit_future};
        use alloc::{format, string::String, vec::Vec};

        /// Reaches outside the component.
        #[service]
        pub struct Outside;

        impl Outside {
            /// The path and the type of each preopened directory, a type of the filesystem's types.
            pub async fn directories() -> Vec<String> {
                let mut directories = Vec::new();
                for (descriptor, path) in preopens::get_directories() {
                    let kind = match descriptor.get_type().await {
                        Ok(wasi::filesystem::types::DescriptorType::Directory) => "directory",
                        Ok(_) => "other",
                        Err(_) => "unknown",
                    };
                    directories.push(format!("{path}: {kind}"));
                }
                directories
            }

            /// Connect to `port` of the loopback address over TCP, through a socket of the sockets'
            /// types.
            pub async fn connect(port: u16) -> Result<(), String> {
                let socket = types::TcpSocket::create(types::IpAddressFamily::Ipv4)
                    .map_err(|error| format!("create: {error:?}"))?;
                let address = types::IpSocketAddress::Ipv4(types::Ipv4SocketAddress {
                    port,
                    address: (127, 0, 0, 1),
                });
                socket
                    .connect(address)
                    .await
                    .map_err(|error| format!("connect: {error:?}"))
            }

            /// The status and the body of a GET of `path` from `authority`, over HTTP through the
            /// host's client.
            pub async fn fetch(authority: String, path: String) -> Result<String, String> {
                use wasi::http::types::{ErrorCode, Fields, Request, Response, Scheme, Trailers};
                let (_, trailers) =
                    wit_future::new::<Result<Option<Trailers>, ErrorCode>>(|| Ok(None));
                let (request, _sent) = Request::new(Fields::new(), None, trailers, None);
                request
                    .set_scheme(Some(&Scheme::Http))
                    .map_err(|()| "scheme")?;
                request
                    .set_authority(Some(&authority))
                    .map_err(|()| "authority")?;
                request
                    .set_path_with_query(Some(&path))
                    .map_err(|()| "path")?;
                let response = client::send(request)
                    .await
                    .map_err(|error| format!("send: {error:?}"))?;
                let status = response.get_status_code();
                let (_, consumed) = wit_future::new::<Result<(), ErrorCode>>(|| Ok(()));
                let (body, _trailers) = Response::consume_body(response, consumed);
                let body: Vec<u8> = body.collect().await;
                Ok(format!("{status} {}", String::from_utf8_lossy(&body)))
            }
        }
    }
}

#[cfg(all(test, feature = "native"))]
mod tests {
    /// Natively, the module compiles and `orb()` answers why a host cannot install it: the native
    /// side does not serve a WASI import yet.
    #[test]
    fn a_wasi_import_is_refused_natively() {
        assert_eq!(
            crate::native::refused(super::io::orb()),
            "the import of wasi:filesystem/preopens@0.3.0 is not supported natively yet"
        );
    }
}
