# Releases

The [release workflow](../.github/workflows/release.yml) builds and publishes matching
npm and crates.io versions when a `vMAJOR.MINOR.PATCH` tag is pushed. Tags must point
to a commit reachable from `main`. A prerelease such as `v0.2.0-rc.1` publishes to
npm's `next` channel; stable versions use `latest`. Build metadata suffixes are not accepted.

## Packages and build gates

The workflow publishes these npm packages in dependency order:

- `@syntropika/tembra-engine-node`: native binaries for Linux, macOS and Windows, on x64 and arm64.
  Linux binaries target glibc and are built on Ubuntu 22.04; musl is not included.
- `@syntropika/tembra-engine-wasm`: the JavaScript loader, declarations and WASM binary.
- `@syntropika/tembra-sdk`: source entrypoints, built JavaScript, declarations and build tooling.
- `@syntropika/tembra-react-native`: Swift sources and an XCFramework for iOS, simulators and macOS;
  an Android Maven package containing arm64-v8a, armeabi-v7a, x86 and x86_64 libraries.

The publishable Rust workspace crates are `tembra-core`, `tembra-bridge`, `tembra`,
`tembra-browser-wasm`, `tembra-contracts` and `tembra-mobile`. `tembra-node-binding` is private implementation
code distributed through npm. `tembra-executor-wasm` and `tembra-contracts-wasm` are not published. All local Rust dependencies
have exact registry versions as well as local paths.

Every platform builds its Node addon and loads it with Node. The workflow also runs
the repository quality checks, isolated orb acceptance, Swift tests and Android unit
tests. Packaging requires all six Node binaries, the Swift distribution and every
Android ABI. Each standalone crate archive is compiled with all features. Staging
replaces each package's previous staged copy, so removed build outputs cannot ship.

The packed SDK and engine archives are installed in an empty project with the
workspace's TypeScript and Node type versions. A consumer compiled with `NodeNext`
must typecheck against every published SDK entry. It then runs with Node and Bun on
native and WASM hosts, including error class identity across entrypoints. The
installed build tooling, run with Bun, builds a source orb and packages the compiled
Rust provider. The Node host executes both through the common component backend. Source components already contain their runtime.
Publication starts only after every build and packaging check passes.

Select **Actions → Release → Run workflow** for a build rehearsal on the chosen ref.
Manual build runs use the checked-in version and upload artifacts. They do not publish or
request registry credentials. Select `check_npm_auth` to check each npm trusted publisher
instead of building. This check requests short-lived registry credentials and discards
them. It does not publish packages or print credentials. The workflow must first exist on the default branch
to appear in the Actions interface.

## Registry setup

The npm packages use the `@syntropika` organization scope. The publishing account
must have permission to publish these four Tembra packages in that organization.

Configure trusted publishing for each package and crate with this GitHub repository
and workflow filename `release.yml`. The publish job has `id-token: write`; build jobs
have read-only repository access. For npm, allow direct `npm publish` and ensure the
package's repository metadata matches the publishing repository. The pinned Node
version includes an npm CLI compatible with trusted publishing.

For initial publication or registries without trusted publisher configuration, add
repository secrets `NPM_TOKEN` and `CARGO_REGISTRY_TOKEN` with the necessary package
publication permissions. Tokens are available only to their publication steps.
Limit the npm token to the `@syntropika` Tembra packages, including permission to create
them for the first release. The crates.io OIDC action is used when its fallback token is absent. npm automatically
selects OIDC or the supplied token. Use npm and crates.io account settings to configure
publishers; do not put credentials in a manifest or tag.

See the official [npm trusted publishing guide](https://docs.npmjs.com/trusted-publishers/)
and [crates.io trusted publishing guide](https://crates.io/docs/trusted-publishing).

## Create a version

Before creating a tag, update the shared Cargo version, exact internal Cargo dependency
versions, the root npm version, all four npm package versions, internal npm requirements
and the React Native SDK peer version. Kotlin artifacts and the Android adapter derive
their version from the React Native manifest; the podspec uses the same version with a
`v` tag prefix. Refresh both lockfiles, commit the version changes to `main` and push
`main` before pushing the tag.

For example, after setting all versions to `0.2.0`:

```sh
mise exec -- bun install --lockfile-only
mise exec -- cargo check --workspace
mise exec -- bun scripts/release.ts validate v0.2.0
mise exec -- just check-all
git tag -a v0.2.0 -m "release: 0.2.0"
git push origin v0.2.0
```

The workflow does not modify versions or create tags. It rejects a mismatched tag
before building or requesting publication credentials. The root npm manifest is a private workspace manifest; `@syntropika/tembra-sdk` itself is public.
All registry packages use Apache-2.0.
The published SDK is staged from `packages/ts/sdk`, with workspace requirements resolved
to the release version and development dependencies omitted. Generated artifacts never
need to be committed.

## Partial publication and retries

The registries do not provide a transaction spanning all packages. A failure can leave
some versions published. Rerun the failed workflow rather than moving or recreating the
tag. An existing npm version is skipped only when its integrity matches the prepared
archive; crates.io versions must match the verified archive checksum. Publication also
checks that repackaged crate bytes match the verified build artifacts. Conflicting
contents stop the workflow instead of silently treating an unrelated version as success.

The publish step does not rebuild Rust libraries: it uploads packages from the same
checkout after archive verification has passed. Cargo handles dependency ordering and
registry index availability. npm packages publish in engine, SDK, then React Native order.
The workflow uploads `npm-release` and `crate-release` artifacts for inspection.
