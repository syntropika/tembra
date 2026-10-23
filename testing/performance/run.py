#!/usr/bin/env python3
"""Build and run real benchmark executables with immutable inputs and external artifacts."""
import argparse
import datetime
import hashlib
import json
import os
from pathlib import Path
import platform
import re
import shutil
import signal
import subprocess
import sys
import time

ROOT = Path(__file__).resolve().parents[2]


def sha(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def sources():
    paths = subprocess.check_output(["git", "ls-files", "--cached", "--others", "--exclude-standard", "-z"], cwd=ROOT).decode().split("\0")
    return {p: {"bytes": (ROOT / p).stat().st_size, "sha256": sha(ROOT / p)} for p in paths if p and (ROOT / p).is_file()}


def files(directory):
    return {str(p.relative_to(directory)): {"bytes": p.stat().st_size, "sha256": sha(p)} for p in directory.rglob("*") if p.is_file()}


def write(path, value):
    path.write_text(json.dumps(value, indent=2) + "\n")


def argument(arguments, flag):
    if flag in arguments:
        return arguments[arguments.index(flag) + 1]
    return next((item.split("=", 1)[1] for item in arguments if item.startswith(flag + "=")), None)


class Interrupted(Exception):
    pass


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--label", required=True)
    parser.add_argument("--bench", choices=["runtime", "composed", "executor"], default="runtime")
    parser.add_argument("--sample-every", type=int)
    parser.add_argument("--capacity", type=int, default=131_072)
    parser.add_argument("--unchanged-runtime", action="store_true", help="compile the common harness without the new runtime API")
    parser.add_argument("--timeout", type=int, default=600)
    parser.add_argument("--jobs", type=int, default=2)
    parser.add_argument("arguments", nargs=argparse.REMAINDER)
    args = parser.parse_args()
    destination = args.output.resolve()
    if destination.is_relative_to(ROOT) or args.jobs < 1 or args.timeout < 1 or not 1 <= args.capacity <= 1_048_576:
        parser.error("use an external output directory and positive bounded limits")
    if args.sample_every is not None and (args.sample_every < 1 or args.unchanged_runtime):
        parser.error("sampling requires a candidate runtime and a positive interval")
    forwarded = args.arguments[1:] if args.arguments[:1] == ["--"] else args.arguments
    save, compare = (argument(forwarded, key) for key in ["--save-baseline", "--baseline"])
    for name in [save, compare]:
        if name is not None and (not re.fullmatch(r"[A-Za-z0-9][A-Za-z0-9_.-]*", name) or name in {".", ".."}):
            parser.error("baseline names must be simple labels")
    stamp = datetime.datetime.now(datetime.timezone.utc).strftime("%Y%m%dT%H%M%S.%fZ")
    run = destination / (stamp + "-" + Path(args.label).name)
    run.mkdir(parents=True, exist_ok=False)
    criterion = run / "criterion"
    criterion.mkdir()
    deadline = time.monotonic() + args.timeout
    environment = dict(os.environ, CARGO_BUILD_JOBS=str(args.jobs), TEMBRA_BENCH_OUTPUT=str(criterion), TEMBRA_OBSERVABILITY_CAPACITY=str(args.capacity))
    environment.pop("TEMBRA_OBSERVABILITY_SAMPLE_EVERY", None)
    if args.sample_every is not None:
        environment["TEMBRA_OBSERVABILITY_SAMPLE_EVERY"] = str(args.sample_every)
    receipt = {"mode": "functional-smoke" if "--test" in forwarded else "measurement", "commands": [], "signal": None}
    active = None

    def interrupt(signum, _frame):
        receipt["signal"] = signum
        if active is not None:
            try:
                os.killpg(active.pid, signal.SIGTERM)
            except ProcessLookupError:
                pass
        # Never unwind Popen before the returned child has been assigned to active.
        # The bounded wait loop observes this flag and enters owned cleanup.

    previous = {sig: signal.signal(sig, interrupt) for sig in [signal.SIGTERM, signal.SIGINT]}

    def check_interrupted():
        if receipt["signal"] is not None:
            raise Interrupted("received signal " + str(receipt["signal"]))

    def child(name, command):
        nonlocal active
        started = time.monotonic()
        code = None
        with (run / (name + ".stdout")).open("w") as stdout, (run / (name + ".stderr")).open("w") as stderr:
            active = subprocess.Popen(command, cwd=ROOT, env=environment, stdout=stdout, stderr=stderr, start_new_session=True)
            pgid = active.pid
            try:
                while True:
                    check_interrupted()
                    remaining_seconds = deadline - time.monotonic()
                    if remaining_seconds <= 0:
                        raise subprocess.TimeoutExpired(command, args.timeout)
                    try:
                        code = active.wait(timeout=min(0.1, remaining_seconds))
                        break
                    except subprocess.TimeoutExpired:
                        continue
            finally:
                if active.poll() is None:
                    os.killpg(pgid, signal.SIGTERM)
                    try:
                        active.wait(timeout=3)
                    except subprocess.TimeoutExpired:
                        os.killpg(pgid, signal.SIGKILL)
                        active.wait()
                code = active.returncode
                try:
                    os.killpg(pgid, 0)
                except ProcessLookupError:
                    remaining = False
                else:
                    remaining = True
                    os.killpg(pgid, signal.SIGKILL)
                receipt["commands"].append({"phase": name, "argv": command, "exit": code, "seconds": time.monotonic() - started, "pgid": pgid, "remaining_children_killed": remaining})
                active = None
                write(run / "progress.json", receipt)
        check_interrupted()
        if code:
            raise RuntimeError(name + " failed: " + str(code))

    before = None
    executable = None
    baseline_pointer = None
    previous_baseline = None
    try:
        before = sources()
        write(run / "source-before-build.json", before)
        (run / "patch.diff").write_bytes(subprocess.check_output(["git", "diff", "--binary"], cwd=ROOT))
        metadata = {"source": subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=ROOT, text=True).strip(),
                    "source_map_sha256": sha(run / "source-before-build.json"), "cargo_lock_sha256": before["Cargo.lock"]["sha256"],
                    "platform": platform.platform(), "profile": "bench", "sample_every": args.sample_every, "capacity": args.capacity,
                    "unchanged_runtime_harness": args.unchanged_runtime, "threshold_policy": "trend-only", "cpu_measurement": None, "allocation_measurement": None,
                    "build_environment": {k: v for k, v in environment.items() if k in {"RUSTFLAGS", "CARGO_ENCODED_RUSTFLAGS", "CARGO_TARGET_DIR", "RUSTUP_TOOLCHAIN"} or k.startswith(("CARGO_PROFILE_BENCH_", "TEMBRA_BENCH_"))}}
        for name, command in [("rustc", ["rustc", "-Vv"]), ("cargo", ["cargo", "-V"])]:
            metadata[name] = subprocess.check_output(command, cwd=ROOT, text=True).strip()
        write(run / "metadata.json", metadata)
        if compare:
            baseline = destination / "baselines" / compare
            pointer = json.loads((baseline / "current.json").read_text())
            saved = baseline / pointer["version"]
            manifest = json.loads((saved / "provenance.json").read_text())
            if files(saved / "data") != manifest["files"]:
                raise RuntimeError("baseline input identity mismatch")
            shutil.copytree(saved / "data", criterion, dirs_exist_ok=True)
            write(run / "baseline-input.json", manifest)
        build = ["cargo", "bench", "--locked", "--no-run", "--message-format=json", "-p", "tembra-testing", "--bench", args.bench]
        features = []
        if not args.unchanged_runtime:
            features.append("observability-bench")
        if args.bench == "executor":
            features.append("embedded-executor")
        if features:
            build += ["--features", ",".join(features)]
        child("build", build)
        artifacts = [json.loads(line) for line in (run / "build.stdout").read_text().splitlines() if line.startswith("{")]
        selected = [entry["executable"] for entry in artifacts if entry.get("reason") == "compiler-artifact" and entry.get("target", {}).get("name") == args.bench and entry.get("executable")]
        if len(set(selected)) != 1:
            raise RuntimeError("build did not identify exactly one benchmark executable")
        executable = Path(selected[0])
        receipt["executable"] = {"path": str(executable), "before_sha256": sha(executable)}
        after_build = sources()
        write(run / "source-after-build.json", after_build)
        if before != after_build:
            raise RuntimeError("source changed during build")
        child("capture", [str(executable), "--bench", *forwarded])
        receipt["executable"]["after_sha256"] = sha(executable)
        if receipt["executable"]["before_sha256"] != receipt["executable"]["after_sha256"]:
            raise RuntimeError("executable changed during capture")
        write(run / "criterion-output.json", files(criterion))
        check_interrupted()
        if save:
            baseline = destination / "baselines" / save
            version = baseline / stamp
            data = version / "data"
            data.mkdir(parents=True)
            for directory in criterion.rglob(save):
                if directory.is_dir() and (directory / "estimates.json").is_file():
                    shutil.copytree(directory, data / directory.relative_to(criterion))
            if not files(data):
                raise RuntimeError("Criterion produced no named baseline")
            write(version / "provenance.json", {"run": str(run), "metadata": metadata, "executable": receipt["executable"], "files": files(data)})
            check_interrupted()
            baseline_pointer = baseline / "current.json"
            previous_baseline = baseline_pointer.read_bytes() if baseline_pointer.exists() else None
            write(baseline_pointer, {"version": stamp})
        check_interrupted()
        receipt["result"] = "passed"
    except (Exception, KeyboardInterrupt) as error:
        receipt["result"] = "incomplete" if isinstance(error, Interrupted) else "failed"
        receipt["error"] = repr(error)
    finally:
        if active is not None:
            if active.poll() is None:
                os.killpg(active.pid, signal.SIGKILL)
                active.wait()
            receipt["outer_cleanup_pgid"] = active.pid
        if before is not None:
            after = sources()
            write(run / "source-after-capture.json", after)
            receipt["source_unchanged"] = before == after
            if not receipt["source_unchanged"]:
                receipt["result"] = "invalid-source-change"
        # Commit completion after execution and source checks. Signals already
        # received make the run incomplete, including during baseline saving.
        # Signals are blocked while publishing the final disposition; this is
        # the execution completion boundary for subsequent caller signals.
        completed_mask = signal.pthread_sigmask(signal.SIG_BLOCK, previous)
        try:
            if receipt["signal"] is not None:
                receipt["result"] = "incomplete"
                receipt["error"] = "received signal " + str(receipt["signal"])
            if baseline_pointer is not None and receipt.get("result") != "passed":
                if previous_baseline is None:
                    baseline_pointer.unlink(missing_ok=True)
                else:
                    baseline_pointer.write_bytes(previous_baseline)
                receipt["baseline_publication_reverted"] = True
            receipt["finished_utc"] = datetime.datetime.now(datetime.timezone.utc).isoformat()
            write(run / "receipt.json", receipt)
            for sig, handler in previous.items():
                signal.signal(sig, handler)
        finally:
            signal.pthread_sigmask(signal.SIG_SETMASK, completed_mask)
    print(run)
    return 0 if receipt["result"] == "passed" else 1


if __name__ == "__main__":
    sys.exit(main())
