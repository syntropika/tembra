"""Run the complete example from a checkout, with a local API and scoped cleanup."""
from pathlib import Path
import argparse
import datetime
import hashlib
import http.server
import json
import os
import shutil
import socket
import subprocess
import threading

here = Path(__file__).resolve().parent
parser = argparse.ArgumentParser()
parser.add_argument('--node', default=shutil.which('node'))
parser.add_argument('--engine', type=Path, default=here/'assets/engine.wasm')
parser.add_argument('--target-dir', type=Path, default=Path(os.environ.get('CARGO_TARGET_DIR', str(here/'build/target'))))
parser.add_argument('--offline', action='store_true')
args = parser.parse_args()
assert args.node, 'Node.js must be available on PATH or selected with --node'
args.target_dir = args.target_dir.resolve()
engine = args.engine.resolve()
out = here/'build/runs'/datetime.datetime.now(datetime.timezone.utc).strftime('%Y%m%dT%H%M%S%fZ')
out.mkdir(parents=True)
sha = lambda path: hashlib.sha256(path.read_bytes()).hexdigest()
commands = []

def run(command, expected=0, timeout=180):
    result = subprocess.run([str(item) for item in command], cwd=here, capture_output=True, text=True, timeout=timeout)
    commands.append({'command':[str(item) for item in command], 'exit_code':result.returncode, 'stdout':result.stdout, 'stderr':result.stderr})
    (out/'commands.json').write_text(json.dumps(commands, indent=2)+'\n')
    codes = (expected,) if isinstance(expected, int) else expected
    assert result.returncode in codes, result.stdout+'\n'+result.stderr
    if result.stdout:
        print(result.stdout, end='', flush=True)
    return result

manifest = json.loads((here/'assets/engine.json').read_text())
assert sha(here/'assets/engine.wasm') == manifest['component_sha256']
for name, digest in manifest['patches'].items():
    assert sha(here/'toolchain'/name) == digest
engine_before = sha(engine)
run([args.node, 'scripts/types.mjs'])
run([args.node, 'bundle.mjs'])
compiler = here/'node_modules/typescript/lib/tsc.js'
run([args.node, compiler, '-p', 'tsconfig.json'])
rejected = run([args.node, compiler, '-p', 'tsconfig-invalid.json'], expected=(1, 2))
assert "Type 'string' is not assignable to type 'number'" in rejected.stdout
command = ['cargo', 'build', '--locked', '--release', '--target-dir', args.target_dir]
if args.offline:
    command.append('--offline')
run(command, timeout=600)
binary = args.target_dir/'release'/('typescript-http-orb.exe' if os.name == 'nt' else 'typescript-http-orb')
paths = [engine, binary, *sorted((here/'src').rglob('*.rs')), *sorted((here/'fixtures').glob('*')), *sorted((here/'wit').rglob('*.wit')), here/'build/analysis-a.js', here/'build/analysis-b.js', here/'Cargo.toml', here/'Cargo.lock', here/'bun.lock']
identities = {str(path):sha(path) for path in paths}
(out/'inputs.json').write_text(json.dumps(identities, indent=2)+'\n')
http_trace = []

class API(http.server.BaseHTTPRequestHandler):
    def do_GET(self):
        status, body = {
            '/readings': (200, json.dumps({'station':'greenhouse', 'samples':[18,20,21,25]})),
            '/invalid': (200, json.dumps({'station':'greenhouse', 'samples':['bad']})),
            '/broken-json': (200, '{broken'),
            '/http-error': (503, json.dumps({'error':'temporarily unavailable'})),
        }.get(self.path, (404, '{}'))
        encoded = body.encode()
        http_trace.append({'path':self.path, 'status':status, 'body':body})
        self.send_response(status)
        self.send_header('Content-Type', 'application/json')
        self.send_header('Content-Length', str(len(encoded)))
        self.end_headers()
        self.wfile.write(encoded)

    def log_message(self, *_):
        pass

server = http.server.HTTPServer(('127.0.0.1', 0), API)
port = server.server_address[1]
worker = threading.Thread(target=server.serve_forever, name='readings-example-api')
worker.start()
native_passed = False
try:
    run([binary, engine, here/'build/analysis-a.js', here/'build/analysis-b.js', f'http://127.0.0.1:{port}', out/'native.json'], timeout=90)
    native = json.loads((out/'native.json').read_text())
    assert native['semantic_pass'] and len(native['checks']) == 8
    native_passed = True
    assert [row['path'] for row in http_trace] == [row['path'] for row in native['requests']]
    assert len(http_trace) == 11 and len(native['progress']) == 28
    assert all(row['principal'] == 'alice' for row in native['requests']+native['progress'])
    assert all(sha(Path(path)) == digest for path, digest in identities.items())
    assert sha(engine) == engine_before
    result = {'semantic_pass':True, 'native_checks':8, 'http_requests':11, 'progress_events':28, 'engine_sha256_before':engine_before, 'engine_sha256_after':sha(engine), 'component_builds_during_script_edit':0, 'typescript_positive_pass':True, 'typescript_negative_rejected':True, 'scope':'Native example with actual HTTP, Tembra and Wasmtime; no browser or mobile qualification'}
    (out/'result.json').write_text(json.dumps(result, indent=2)+'\n')
finally:
    server.shutdown()
    server.server_close()
    worker.join(timeout=5)
    probe = socket.socket()
    port_closed = probe.connect_ex(('127.0.0.1', port)) != 0
    probe.close()
    cleanup = {'api_thread_stopped':not worker.is_alive(), 'api_port_closed':port_closed, 'native_process_foreground_and_reaped':True, 'http_workers_joined_by_native_host':native_passed}
    (out/'http.json').write_text(json.dumps(http_trace, indent=2)+'\n')
    (out/'cleanup.json').write_text(json.dumps(cleanup, indent=2)+'\n')
    assert not worker.is_alive() and port_closed
(here/'build/latest.json').write_text(json.dumps({'directory':str(out)}, indent=2)+'\n')
print('PASS: TypeScript + Zod + HTTP + statistics + progress + live replacement + cleanup', flush=True)
