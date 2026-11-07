"""Build the matched experimental generator and runtime from pinned upstream sources."""
from pathlib import Path
import argparse
import hashlib
import json
import os
import shutil
import subprocess

here = Path(__file__).resolve().parent.parent
parser = argparse.ArgumentParser()
parser.add_argument('--wasi-sdk', type=Path, default=Path(os.environ['WASI_SDK_PATH']) if 'WASI_SDK_PATH' in os.environ else None)
parser.add_argument('--offline', action='store_true', help='Use existing Cargo caches after source checkout')
args = parser.parse_args()
assert args.wasi_sdk, 'Set WASI_SDK_PATH or pass --wasi-sdk to an installed WASI SDK 33'
sdk = args.wasi_sdk.resolve()
assert (sdk/'bin/clang').is_file(), 'WASI SDK clang was not found'
work = here/'.cache/toolchain'
work.mkdir(parents=True, exist_ok=True)
manifest = json.loads((here/'assets/engine.json').read_text())
sha = lambda path: hashlib.sha256(path.read_bytes()).hexdigest()
for name, digest in manifest['patches'].items():
    assert sha(here/'toolchain'/name) == digest, name

def run(command, cwd=work, env=None):
    print(' '.join(str(value) for value in command), flush=True)
    subprocess.run([str(value) for value in command], cwd=cwd, env=env, check=True)

for name, url, revision in [
    ('componentize-qjs', 'https://github.com/andreiltd/componentize-qjs.git', manifest['componentize_qjs_revision']),
    ('wasm-tools', 'https://github.com/bytecodealliance/wasm-tools.git', manifest['wasm_tools_revision']),
]:
    directory = work/name
    if not directory.exists():
        directory.mkdir()
        run(['git', 'init'], directory)
        run(['git', 'remote', 'add', 'origin', url], directory)
        run(['git', 'fetch', '--depth=1', '--no-tags', 'origin', revision], directory)
        run(['git', 'checkout', '--detach', revision], directory)
    actual = subprocess.check_output(['git', 'rev-parse', 'HEAD'], cwd=directory, text=True).strip()
    assert actual == revision, f'Unexpected source revision in {name}'
    patch = here/'toolchain'/(name+'.patch')
    applied = subprocess.run(['git', 'apply', '--check', '--reverse', str(patch)], cwd=directory, capture_output=True)
    if applied.returncode != 0:
        run(['git', 'apply', '--check', patch], directory)
        run(['git', 'apply', patch], directory)

source = work/'componentize-qjs'
target = work/'target'
remap = ' '.join(f'--remap-path-prefix={path}={replacement}' for path, replacement in [
    (source, '/componentize-qjs'),
    (work/'wasm-tools', '/wasm-tools'),
    (Path.home()/'.cargo', '/cargo'),
    (Path.home()/'.rustup', '/rustup'),
])
environment = {**os.environ,
    'CARGO_TARGET_DIR':str(target),
    'CARGO_TARGET_WASM32_WASIP2_RUSTFLAGS':'-Clink-arg=-shared -Clink-arg=-Wl,--no-entry -Clink-arg=-Wl,--allow-undefined -Clto=fat -Copt-level=3 '+remap,
    'CARGO_TARGET_WASM32_WASIP2_LINKER':str(sdk/'bin/clang'),
    'CC_wasm32_wasip2':str(sdk/'bin/clang'), 'CFLAGS_wasm32_wasip2':'-fPIC -O3',
    'WASI_SDK_PATH':str(sdk), 'WASI_SDK':str(sdk),
    'CARGO_PROFILE_RELEASE_CODEGEN_UNITS':'1',
}
environment.pop('CARGO_ENCODED_RUSTFLAGS', None)
base = ['cargo', '+1.99.0', 'build', '--locked', '--release', '--no-default-features']
if args.offline:
    base.append('--offline')
run(['rustup', 'target', 'add', '--toolchain', '1.99.0', 'wasm32-wasip2'])
run([*base, '--target', 'wasm32-wasip2', '-p', 'componentize-qjs-runtime'], source, environment)
runtime = target/'wasm32-wasip2/release/componentize_qjs_runtime.wasm'
prebuilt = source/'crates/core/prebuilt'
prebuilt.mkdir(exist_ok=True)
for name in ['runtime-sync.wasm', 'runtime-opt-size-sync.wasm']:
    if not (prebuilt/name).exists():
        shutil.copy2(runtime, prebuilt/name)
# The patched CLI requires an explicit runtime, so embedded fallbacks are unused.
run([*base, '-p', 'componentize-qjs-cli'], source, {**os.environ, 'CARGO_TARGET_DIR':str(target)})
output = work/'bin'
output.mkdir(exist_ok=True)
shutil.copy2(runtime, output/'runtime.wasm')
shutil.copy2(target/'release/componentize-qjs', output/'componentize-qjs')
(work/'build.json').write_text(json.dumps({'runtime_sha256':sha(output/'runtime.wasm'), 'generator_sha256':sha(output/'componentize-qjs'), 'scope':'Private paired synchronous generator/runtime; compiler paths can change output hashes'}, indent=2)+'\n')
print('Built the paired generator and runtime. Run scripts/prepare.mjs to prepare the example engine.', flush=True)
