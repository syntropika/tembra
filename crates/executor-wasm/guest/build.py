"""Build the reusable interpreter with an explicitly supplied WASI SDK and cache."""
import argparse
import hashlib
import io
import json
import os
from pathlib import Path
import shutil
import subprocess
import tarfile

REVISION = "b51e92c0c7ca3b0ae7afff1a1f2de8baf2e7a34a"
ROOT = Path(__file__).resolve().parent


def verify_source(source, archive):
    """Reject modified cached inputs instead of attributing them to the pinned revision."""
    expected = set()
    digest = hashlib.sha256()
    with tarfile.open(fileobj=io.BytesIO(archive)) as contents:
        for member in contents.getmembers():
            if member.isdir():
                continue
            expected.add(member.name)
            target = source / member.name
            if member.issym():
                if not target.is_symlink() or os.readlink(target) != member.linkname:
                    raise RuntimeError("Cached source link differs: " + member.name)
                data = member.linkname.encode()
            elif member.isfile():
                data = contents.extractfile(member).read()
                if member.name == "dtoa.c":
                    data = data.replace(b"#include <setjmp.h>", b"#if !defined(__wasi__)\n#include <setjmp.h>\n#endif")
                if target.is_symlink() or not target.is_file() or target.read_bytes() != data:
                    raise RuntimeError("Cached source file differs: " + member.name)
            else:
                raise RuntimeError("Unsupported archive entry: " + member.name)
            digest.update(json.dumps([member.name, len(data)]).encode())
            digest.update(data)
    actual = {str(path.relative_to(source)) for path in source.rglob("*") if path.is_file() or path.is_symlink()}
    if actual != expected:
        raise RuntimeError("Cached source contains unexpected files")
    return digest.hexdigest()


def main():
    """Prepare a task-owned source copy, apply the WASI include fix, and build."""
    parser = argparse.ArgumentParser()
    parser.add_argument("--sdk", type=Path, required=True)
    parser.add_argument("--cache", type=Path, required=True)
    parser.add_argument("--source", type=Path)
    parser.add_argument("--toolchain", default="nightly")
    args = parser.parse_args()
    cache = args.cache.resolve()
    cache.mkdir(parents=True, exist_ok=True)
    checkout = args.source
    if checkout is None:
        checkout = cache / "checkout"
        if not checkout.exists():
            subprocess.run(["git", "clone", "https://github.com/stencil-hq/dquickts", str(checkout)], check=True)
    archive = subprocess.check_output(["git", "-C", str(checkout), "archive", REVISION])
    source = cache / ("dquickts-" + REVISION)
    if not source.exists():
        source.mkdir()
        with tarfile.open(fileobj=io.BytesIO(archive)) as contents:
            contents.extractall(source, filter="data")
        dtoa = source / "dtoa.c"
        text = dtoa.read_text()
        if text.count("#include <setjmp.h>") != 1:
            raise RuntimeError("Pinned dtoa include changed")
        dtoa.write_text(text.replace("#include <setjmp.h>", "#if !defined(__wasi__)\n#include <setjmp.h>\n#endif"))
    source_sha256 = verify_source(source, archive)
    sdk = args.sdk.resolve()
    sysroot = sdk / "share/wasi-sysroot"
    env = os.environ.copy()
    env.update({
        "WASI_SDK": str(sdk), "RQUICKJS_SYS_NO_WASI_SDK": "1",
        "CC_wasm32_wasip3": str(sdk / "bin/clang"),
        "AR_wasm32_wasip3": str(sdk / "bin/llvm-ar"),
        "CFLAGS_wasm32_wasip3": "--sysroot=" + str(sysroot),
        "BINDGEN_EXTRA_CLANG_ARGS_wasm32_wasip3": "--sysroot=" + str(sysroot) + " -isystem " + str(sysroot / "include/wasm32-wasip3"),
        "CARGO_TARGET_DIR": str(cache / "target"),
    })
    patch = 'patch."https://github.com/stencil-hq/dquickts".rquickjs.path=' + json.dumps(str(source / "rquickjs"))
    subprocess.run(["cargo", "+" + args.toolchain, "build", "--locked", "--release", "--target", "wasm32-wasip3", "--manifest-path", str(ROOT / "Cargo.toml"), "--config", patch], env=env, check=True)
    component = cache / "target/wasm32-wasip3/release/tembra_dquickts.wasm"
    destination = cache / "engine.wasm"
    shutil.copyfile(component, destination)
    manifest = {"engine": "dquickts", "protocol": "tembra:runtime/interpreter@0.3.0", "revision": REVISION, "source_sha256": source_sha256, "wasm_sha256": hashlib.sha256(destination.read_bytes()).hexdigest(), "bytes": destination.stat().st_size,
                "rust": subprocess.check_output(["rustc", "+" + args.toolchain, "-Vv"], text=True), "sdk": subprocess.check_output([str(sdk / "bin/clang"), "--version"], text=True)}
    (cache / "engine.json").write_text(json.dumps(manifest, indent=2) + "\n")
    print(json.dumps({"component": str(destination), **manifest}))


if __name__ == "__main__":
    main()
