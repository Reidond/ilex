#!/usr/bin/env python3
"""Run the builder lifecycle race matrix in an isolated Linux checkout.

Run with python3 -B docs/tooling/test-builder-lifecycle.py. Faults are injected
only into this process and its children. No builder services or VMs are changed.
"""
from contextlib import contextmanager
import importlib.util
import json
import os
from pathlib import Path
import select
import signal
import subprocess
import sys
import tempfile
import time
import traceback
import uuid

if sys.platform != "win32":
    SOURCE = Path(__file__).resolve().parents[2] / "crates/ilex-tooling-windows/assets/builder-jobs.py"
    loader = importlib.util.spec_from_file_location("builder_jobs", SOURCE)
    jobs = importlib.util.module_from_spec(loader)
    loader.loader.exec_module(jobs)


@contextmanager
def fixture():
    with tempfile.TemporaryDirectory() as root:
        root = Path(root)
        jobs.JOBS = root / "jobs"
        jobs.CONSOLE_ROOT = root / "console"
        owner, job = str(uuid.uuid4()), str(uuid.uuid4())
        spec = {"schema": 1, "job": job, "snapshot": {
            "identity": {"owner": owner}, "source": {"digest": "sha256:" + "a" * 64}},
            "command": {"operation": "vm", "args": {"command": []}}, "environment": {}}
        path = jobs.job_path(owner, job)
        jobs.atomic(path / "request.json", jobs.encode(spec))
        yield path, spec


def transition(path, spec, actor, phase, **fields):
    with jobs.dispatch(path):
        return jobs.compare_and_swap(path, spec, jobs.record(path, spec), actor, phase, **fields)


def prime(path, spec, phase):
    if phase is None:
        return
    registration = "registered" if phase == "registered" else "starting"
    state = transition(path, spec, "submit", registration, lease=jobs.lease(60))
    if phase == "running" or phase == "collecting" or phase == "recovering":
        state = transition(path, spec, "worker", "running")
    if phase == "collecting":
        state = transition(path, spec, "worker", "collecting")
    if phase == "recovering":
        state = transition(path, spec, "recovery", "recovering")
    return state


def at_replace(path, action, fault, verify_old, verify_new, after=False, target_path=None):
    target_path = target_path or path / "state.json"
    ready, notify = os.pipe()
    server, unblock = os.pipe()
    pid = os.fork()
    if pid == 0:
        os.close(ready)
        os.close(unblock)
        replace = os.replace

        def pause(source, target):
            if Path(target) != target_path:
                return replace(source, target)
            if after:
                replace(source, target)
            os.write(notify, b"!")
            if not select.select([server], [], [], 8)[0] or os.read(server, 1) != b"+":
                os._exit(2)
            if not after:
                replace(source, target)

        os.replace = pause
        try:
            action()
        except BaseException:
            traceback.print_exc()
            os._exit(1)
        os._exit(0)
    os.close(notify)
    os.close(server)
    stopped = False
    try:
        assert select.select([ready], [], [], 8)[0] and os.read(ready, 1) == b"!", "actor missed injection point"
        (verify_new if after else verify_old)()
        if fault == "delay":
            time.sleep(.08)
            (verify_new if after else verify_old)()
            os.write(unblock, b"+")
            _, rc = os.waitpid(pid, 0)
            stopped = True
            assert os.WIFEXITED(rc) and os.WEXITSTATUS(rc) == 0
            verify_new()
        else:
            if fault == "hang":
                start = time.monotonic()
                try:
                    with jobs.lock(path / "dispatch.lock", seconds=.15):
                        raise AssertionError("hung actor did not hold the lock")
                except BlockingIOError:
                    assert time.monotonic() - start < 1, "dispatch wait exceeded its bound"
            os.kill(pid, signal.SIGKILL)
            _, rc = os.waitpid(pid, 0)
            stopped = True
            assert os.WIFSIGNALED(rc)
            if after:
                verify_new()
            else:
                verify_old()
                action()
                verify_new()
    finally:
        if not stopped:
            try:
                os.kill(pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
            os.waitpid(pid, 0)
        os.close(ready)
        os.close(unblock)


def check_transition(source, actor, target, fault, fields):
    with fixture() as (path, spec):
        old = prime(path, spec, source)

        def action():
            with jobs.dispatch(path):
                state = jobs.record(path, spec)
                if target in jobs.TERMINAL_STATES:
                    jobs.publish(path, spec, state, actor, target, 130 if target != "complete" else 0, artifacts=[])
                else:
                    jobs.compare_and_swap(path, spec, state, actor, target, **fields)

        def old_record():
            assert jobs.record(path, spec) == old

        def new_record():
            state = jobs.record(path, spec)
            assert state["state"] == target and state["generation"] == (old or {}).get("generation", 0) + 1
            if target in jobs.TERMINAL_STATES:
                manifest = state["result"]
                assert manifest["job"] == spec["job"]
                with jobs.dispatch(path):
                    try:
                        jobs.compare_and_swap(path, spec, old, actor, target)
                    except AssertionError as error:
                        assert str(error) == "job record changed"
                    else:
                        raise AssertionError("replayed publisher wrote twice")
                assert jobs.record(path, spec) == state

        # The actor still owns the dispatch lock at this point. Test duplicate
        # publication after it exits, while the atomic view is visible immediately.
        def visible():
            state = jobs.record(path, spec)
            assert state["state"] == target and state["generation"] == (old or {}).get("generation", 0) + 1
            if target in jobs.TERMINAL_STATES:
                assert state["result"]["job"] == spec["job"]

        at_replace(path, action, fault, old_record, visible,
                   after=fault == "kill" and target in jobs.TERMINAL_STATES)
        new_record()


def heartbeat_and_attach():
    with fixture() as (path, spec):
        old = prime(path, spec, "registered")
        with jobs.dispatch(path):
            jobs.compare_and_swap(path, spec, old, "heartbeat", "registered", lease=jobs.lease(.2))
        time.sleep(.12)
        assert jobs.renew(path.parent.name, path.name)["lease"] == "renewed"
        time.sleep(.12)
        with jobs.dispatch(path):
            renewed = jobs.record(path, spec)
            assert not jobs.expired(renewed)
            accepted = jobs.compare_and_swap(path, spec, renewed, "attach", "starting", lease=jobs.lease(2))
            assert not jobs.expired(accepted)
        assert jobs.renew(path.parent.name, path.name)["lease"] == "consumed"
        assert jobs.job_status(path, spec, resume_terminal=True)["state"]["state"] == "starting"
    with fixture() as (path, spec):
        old = prime(path, spec, "registered")
        with jobs.dispatch(path):
            jobs.compare_and_swap(path, spec, old, "heartbeat", "registered", lease=jobs.lease(0))
        assert jobs.renew(path.parent.name, path.name)["lease"] == "expired"
        result = jobs.job_status(path, spec, resume_terminal=True)["result"]
        assert result["terminal_error"] == "terminal registration expired"


def cancel_retry_and_collecting_resume():
    with fixture() as (path, spec):
        prime(path, spec, "running")
        unit_live, checked = jobs.unit_live, jobs.checked
        signals = []
        jobs.unit_live = lambda *_args, **_kwargs: True

        def send(command, **_kwargs):
            signals.append(command[2])
            if len(signals) == 1:
                raise subprocess.CalledProcessError(1, command)

        jobs.checked = send
        grace, wait = jobs.CANCEL_GRACE_SECONDS, jobs.CANCEL_WAIT_SECONDS
        jobs.CANCEL_GRACE_SECONDS = .09
        jobs.CANCEL_WAIT_SECONDS = .15
        try:
            assert jobs.cancel_step(path, spec)[0] == "pending"
            assert "delivered" not in jobs.record(path, spec)
            assert jobs.cancel_step(path, spec)[0] == "pending"
            assert jobs.record(path, spec)["delivered"] == "SIGTERM"
            time.sleep(.1)
            assert jobs.cancel_job(path, spec)["cancel"] == "pending"
            assert signals == ["--signal=SIGTERM", "--signal=SIGTERM", "--signal=SIGKILL"]
            transition(path, spec, "worker", "collecting")
            assert jobs.cancel_job(path, spec)["cancel"] == "confirmed"
            assert jobs.cancel_job(path, spec)["cancel"] == "confirmed"
            status = jobs.job_status(path, spec, resume_terminal=True)
            assert status["state"]["state"] == "collecting" and status["result"] is None
            with jobs.dispatch(path):
                current = jobs.record(path, spec)
                jobs.publish(path, spec, current, "worker", "cancelled", 130, artifacts=[])
            assert jobs.job_status(path, spec, resume_terminal=True)["result"]["exit_code"] == 130
            assert signals[-1] == "--signal=SIGKILL" and len(signals) == 3
        finally:
            jobs.CANCEL_GRACE_SECONDS, jobs.CANCEL_WAIT_SECONDS = grace, wait
            jobs.unit_live, jobs.checked = unit_live, checked


def hung_cleanup_and_console(*, maintenance=False):
    with fixture() as (path, spec):
        prime(path, spec, "collecting")
        logs = jobs.CONSOLE_ROOT / path.parent.name / path.name
        logs.mkdir(parents=True)
        (logs / "fixture-console.log").write_text("serial evidence\n")
        jobs.atomic(path / "resources/vm.json", jobs.encode({"provider": "libvirt", "name": "fixture"}))
        jobs.atomic(path / "lease.json", b"{}")
        unit_live = jobs.unit_live
        source = jobs.__file__
        cleanup_seconds = jobs.CLEANUP_SECONDS
        hung = path / "hung-cleanup.py"
        hung.write_text("import time\ntime.sleep(60)\n")
        jobs.__file__ = str(hung)
        jobs.unit_live = lambda *_args, **_kwargs: False
        jobs.CLEANUP_SECONDS = .25
        try:
            start = time.monotonic()
            result = jobs.job_status(path, spec, resume_terminal=True)["result"]
            assert time.monotonic() - start < 2
            assert result["interrupted"] and not result["resource_lease_retained"]
            assert result["resources_pending_cleanup"] == ["fixture"]
            assert jobs.read_json(path / "reap.json")["names"] == ["fixture"]
            assert not (path / "lease.json").exists()
            assert "resource cleanup deadline exceeded" in result["resource_cleanup_error"]
            assert "fixture-platform.log" in result["evidence_error"]
            assert "fixture-console.log" not in result["evidence_error"]
            assert "artifacts/console/fixture-console.log" in {entry["path"] for entry in result["artifacts"]}
            assert (path / "exports/artifacts/console/fixture-console.log").read_text() == "serial evidence\n"
            if maintenance:
                # The failed service can remain down during the first idle probe.
                assert jobs.maintenance() == ""
                assert (path / "reap.json").exists()
                hung.write_text("pass\n")
                assert jobs.maintenance() == ""
                assert not (path / "reap.json").exists()
            assert jobs.job_status(path, spec)["result"] == result
        finally:
            jobs.__file__ = source
            jobs.unit_live = unit_live
            jobs.CLEANUP_SECONDS = cleanup_seconds


def cleanup_timeout_maintenance_reaps():
    hung_cleanup_and_console(maintenance=True)


def crash_after_disk_is_frozen():
    for manifested in (False, True):
        with fixture() as (path, spec):
            spec["command"] = {"operation": "disk", "args": {}}
            jobs.atomic(path / "request.json", jobs.encode(spec))
            previous = prime(path, spec, "collecting")
            checkout = path / "checkout-output"
            (checkout / "disks").mkdir(parents=True)
            disk = checkout / "disks/probe.qcow2"
            disk.write_bytes(b"frozen disk bytes")
            target = path / "exports/disks/probe.qcow2"

            def copy_and_hash():
                jobs.artifact_inventory(path, spec, checkout, [])

            def visible():
                assert target.read_bytes() == b"frozen disk bytes"
                assert jobs.record(path, spec) == previous

            at_replace(path, copy_and_hash, "kill", lambda: None, visible, after=True, target_path=target)
            (target.parent / "unfinished.qcow2.partial").write_bytes(b"partial")
            (target.parent / "not-a-disk.txt").write_bytes(b"other")
            expected = jobs.artifact_entry(target, "disks/probe.qcow2")
            unit_live, cleanup, hash_file = jobs.unit_live, jobs.cleanup_bounded, jobs.artifact_entry
            if manifested:
                jobs.atomic(path / "frozen-manifest.json", jobs.encode({
                    "job": spec["job"], "owner": path.parent.name,
                    "source_digest": spec["snapshot"]["source"]["digest"], "artifacts": [expected],
                }))
                def hash_unlisted(file, name):
                    assert file != target, "valid frozen manifest did not supply the disk checksum"
                    return hash_file(file, name)
                jobs.artifact_entry = hash_unlisted
            jobs.unit_live = lambda *_args, **_kwargs: False
            jobs.cleanup_bounded = lambda *_args: None
            try:
                result = jobs.job_status(path, spec)["result"]
                assert result["interrupted"] and result["evidence_error"] is None
                assert [entry for entry in result["artifacts"] if entry["path"].startswith("disks/")] == [expected]
                _, bytes_read = jobs.request({"schema": 1, "operation": "fetch", "owner": path.parent.name,
                                              "job": path.name, "path": expected["path"]}, b"")
                assert bytes_read == target.read_bytes()
            finally:
                jobs.unit_live, jobs.cleanup_bounded, jobs.artifact_entry = unit_live, cleanup, hash_file


def maintenance_while_terminal_renewed():
    with fixture() as (path, spec):
        prime(path, spec, "registered")
        transition(path, spec, "heartbeat", "registered", lease=jobs.lease(.15))
        time.sleep(.08)
        assert jobs.renew(path.parent.name, path.name)["lease"] == "renewed"
        time.sleep(.1)
        with jobs.lock(path / "maintenance.lock"):
            assert jobs.maintenance() == f"busy: job {path.name}"
            transition(path, spec, "attach", "starting", lease=jobs.lease(2))
            assert jobs.maintenance() == f"busy: job {path.name}"
        assert jobs.cancel_job(path, spec)["cancel"] == "confirmed"
        assert jobs.maintenance() == ""


def main():
    faults = ("kill", "hang", "delay")
    failed = False
    for actor, edges in jobs.TRANSITIONS.items():
        for source, targets in edges.items():
            for target in sorted(targets):
                fields = {"cancel_at": 1} if actor == "controller" else {"lease": jobs.lease(60)}
                outcomes = []
                for fault in faults:
                    try:
                        check_transition(source, actor, target, fault, fields)
                        outcomes.append(f"PASS {fault}")
                    except Exception:
                        traceback.print_exc()
                        outcomes.append(f"FAIL {fault}")
                        failed = True
                print(f"MATRIX {actor} {source or 'absent'} -> {target} | {' | '.join(outcomes)}", flush=True)
    for check in (heartbeat_and_attach, cancel_retry_and_collecting_resume, hung_cleanup_and_console,
                  cleanup_timeout_maintenance_reaps, crash_after_disk_is_frozen, maintenance_while_terminal_renewed):
        try:
            check()
            print(f"SCENARIO {check.__name__} PASS", flush=True)
        except Exception:
            traceback.print_exc()
            print(f"SCENARIO {check.__name__} FAIL", flush=True)
            failed = True
    raise SystemExit(int(failed))


if __name__ == "__main__":
    if sys.platform == "win32":
        from windows_builder_faults import ctrl_c_during_interactive_submit
        ctrl_c_during_interactive_submit()
        print("SCENARIO ctrl_c_during_interactive_submit PASS", flush=True)
    else:
        main()
