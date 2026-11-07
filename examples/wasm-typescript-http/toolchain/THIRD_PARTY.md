# Third-party source and prepared component

The prepared example component contains QuickJS and Rust code built by componentize-qjs with the two local source patches in this directory. The patches modify upstream files; they are not an upstream release.

| Project                             | Source identity                                                                              | License                                                                      |
| ----------------------------------- | -------------------------------------------------------------------------------------------- | ---------------------------------------------------------------------------- |
| componentize-qjs                    | `66b3fafcfd7588e155edc0bec4839e5de7298072` at https://github.com/andreiltd/componentize-qjs  | Apache-2.0                                                                   |
| wasm-tools and wit-dylib            | `e22db44da4eaf17a0e46ec55b911c7539589a3cc` at https://github.com/bytecodealliance/wasm-tools | Apache-2.0 with LLVM exception, Apache-2.0 or MIT as specified by each crate |
| QuickJS through rquickjs-sys 0.14.0 | Upstream runtime Cargo.lock                                                                  | MIT                                                                          |

The [license directory](licenses/) preserves the upstream componentize-qjs, wasm-tools and QuickJS license texts, including QuickJS copyright notices. The pinned upstream Cargo.lock resolves the runtime dependencies. The build script fetches the exact source revisions and applies the included patches.

JavaScript dependencies are installed from the example's pinned package manifest and Bun lockfile. They are not bundled into committed JavaScript. Their package license files remain in the installation. The generated engine world in `assets/engine.wit` comes from Jco 1.41.0 and is retained for inspection and native binding generation.
