"""Exercise runner process ownership and interruption without executing benchmarks."""
import json
import os
from pathlib import Path
import signal
import subprocess
import sys
import tempfile
import unittest

RUNNER = Path(__file__).with_name("run.py")
SHIM = r'''
import importlib.util,json,os,pathlib,signal,subprocess,sys
runner,root,mode=sys.argv[1:]
root=pathlib.Path(root)
exe=root/'synthetic.py'
exe.write_text("#!/usr/bin/env python3\nimport json,os,pathlib,time\np=pathlib.Path(os.environ['TEMBRA_BENCH_OUTPUT'])/'synthetic'/'saved';p.mkdir(parents=True);(p/'estimates.json').write_text('{}');(p/'sample.json').write_text('{}')\n" + ("time.sleep(30)\n" if mode=='timeout' else ""))
exe.chmod(0o755)
spec=importlib.util.spec_from_file_location('tested_runner',runner)
module=importlib.util.module_from_spec(spec);spec.loader.exec_module(module)
module.sources=lambda:{'Cargo.lock':{'bytes':0,'sha256':'synthetic-lock'}}
original_output=module.subprocess.check_output
original_popen=module.subprocess.Popen
original_write=module.write
def output(command,*args,**kwargs):
 if command[:2]==['git','rev-parse']:return 'synthetic-revision\n'
 if command[:2]==['git','diff']:return b''
 if command in [['rustc','-Vv'],['cargo','-V']]:return 'synthetic-toolchain\n'
 return original_output(command,*args,**kwargs)
def popen(command,*args,**kwargs):
 build=command[0]=='cargo' and 'bench' in command
 if build:
  code='import json;print('+repr(json.dumps({'reason':'compiler-artifact','target':{'name':'runtime'},'executable':str(exe)}))+')'
  if mode=='launch-signal':code+=';import time;time.sleep(30)'
  process=original_popen([sys.executable,'-c',code],*args,**kwargs)
  if mode=='launch-signal':os.kill(os.getpid(),signal.SIGTERM)
 else:
  process=original_popen(command,*args,**kwargs)
  if mode=='late-wait-signal':
   wait=process.wait
   def late_wait(*args,**kwargs):
    result=wait(*args,**kwargs)
    os.kill(os.getpid(),signal.SIGTERM)
    return result
   process.wait=late_wait
 return process
def write(path,value):
 original_write(path,value)
 if mode=='baseline-signal' and path.name=='current.json':
  os.kill(os.getpid(),signal.SIGTERM)
module.subprocess.check_output=output
module.subprocess.Popen=popen
module.write=write
sys.argv=[runner,'--output',str(root/'results'),'--label',mode,'--timeout','1' if mode=='timeout' else '8','--','--test']
if mode=='baseline-signal':sys.argv+=['--save-baseline','saved']
sys.exit(module.main())
'''


class RunnerProcessTests(unittest.TestCase):
    def invoke(self, root, mode):
        process = subprocess.Popen(
            [sys.executable, "-c", SHIM, str(RUNNER.resolve()), str(root), mode],
            stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True, start_new_session=True,
        )
        try:
            stdout, stderr = process.communicate(timeout=12)
        finally:
            if process.poll() is None:
                os.killpg(process.pid, signal.SIGKILL)
                process.wait()
        paths = sorted((root / "results").glob("*/receipt.json"))
        self.assertTrue(paths, stderr)
        receipt_path = paths[-1]
        receipt = json.loads(receipt_path.read_text())
        for child in receipt["commands"]:
            with self.assertRaises(ProcessLookupError):
                os.killpg(child["pgid"], 0)
        return process.returncode, receipt, receipt_path, stdout

    def test_fresh_output_and_success(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            for _ in range(2):
                code, receipt, path, _stdout = self.invoke(root, "fresh")
                self.assertEqual(code, 0)
                self.assertEqual(receipt["result"], "passed")
                self.assertTrue((path.parent / "criterion/synthetic/saved/estimates.json").is_file())
            self.assertEqual(len(list((root / "results").glob("*/receipt.json"))), 2)

    def test_timeout_and_signals_preserve_incomplete_execution(self):
        for mode in ["timeout", "launch-signal", "late-wait-signal", "baseline-signal"]:
            with self.subTest(mode=mode), tempfile.TemporaryDirectory() as directory:
                root = Path(directory)
                pointer = root / "results/baselines/saved/current.json"
                pointer.parent.mkdir(parents=True)
                pointer.write_text('{"version":"previous"}\n')
                code, receipt, _path, _stdout = self.invoke(root, mode)
                self.assertNotEqual(code, 0)
                self.assertEqual(receipt["result"], "failed" if mode == "timeout" else "incomplete")
                if mode != "timeout":
                    self.assertEqual(receipt["signal"], signal.SIGTERM)
                if mode == "baseline-signal":
                    self.assertEqual(json.loads(pointer.read_text()), {"version": "previous"})
                    self.assertTrue(receipt["baseline_publication_reverted"])


if __name__ == "__main__":
    unittest.main()
