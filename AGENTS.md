# Working in this repository

Read these before you change code:

- [CONTRIBUTING.md](CONTRIBUTING.md) states the code structure, the comment style and the [invariants](CONTRIBUTING.md#invariants) a change must keep. Each invariant names the tests that guard it.
- [DEVELOP.md](DEVELOP.md) lists the tools and the [checks for one change](DEVELOP.md#checks-for-one-change) in each area. `just ci` runs the full verification, the parts that CI runs in a job each. CI also runs `just test-wasm-acceptance` in its `acceptance` job, which `just ci` does not run.

Run the checks of the area you change before you commit.
