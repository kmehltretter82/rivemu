#!/usr/bin/env python3
"""Run one Linux reproducer against two kernels and emit reviewable evidence."""

from __future__ import annotations

import argparse
import dataclasses
import datetime as dt
import hashlib
import json
import os
import platform
import shlex
import signal
import subprocess
import sys
import time
from pathlib import Path
from typing import Any


ROOT = Path(__file__).resolve().parents[1]
RESULT_PREFIX = "RIVEMU_AB_RESULT="
MAX_RESULT_LINE_BYTES = 16 * 1024
MAX_RETAINED_CONFLICTS = 16


class EvidenceError(Exception):
    """Invalid inputs or an inability to create trustworthy evidence."""


@dataclasses.dataclass
class RunResult:
    name: str
    label: str
    kernel: Path
    expected: str
    command: list[str]
    elapsed_seconds: float
    returncode: int | None
    timed_out: bool
    expected_line_count: int
    conflicting_result_count: int
    conflicting_result_lines: list[str]
    conflicting_result_lines_truncated: bool
    launch_error: str | None
    passed: bool
    console_file: str

    def manifest(self) -> dict[str, Any]:
        result = dataclasses.asdict(self)
        result["kernel"] = str(self.kernel)
        return result


def file_arg(value: str) -> Path:
    path = Path(value).expanduser().resolve()
    if not path.is_file():
        raise argparse.ArgumentTypeError(f"not a file: {path}")
    return path


def positive_int(value: str) -> int:
    parsed = int(value)
    if parsed <= 0:
        raise argparse.ArgumentTypeError("must be greater than zero")
    return parsed


def positive_float(value: str) -> float:
    parsed = float(value)
    if parsed <= 0:
        raise argparse.ArgumentTypeError("must be greater than zero")
    return parsed


def parse_args(argv: list[str]) -> argparse.Namespace:
    default_runner = os.environ.get(
        "RIVEMU_RUNNER", str(ROOT / "target/release/rivemu-runner")
    )
    parser = argparse.ArgumentParser(
        description=(
            "run an identical init/reproducer under two Linux kernels in rivemu "
            "and write a hashed A/B evidence bundle"
        )
    )
    parser.add_argument("--kernel-a", required=True, type=file_arg)
    parser.add_argument("--kernel-b", required=True, type=file_arg)
    parser.add_argument("--label-a", required=True)
    parser.add_argument("--label-b", required=True)
    parser.add_argument("--init", required=True, type=file_arg, dest="init")
    parser.add_argument("--expect-a", required=True)
    parser.add_argument("--expect-b", required=True)
    parser.add_argument("--runner", type=file_arg, default=default_runner)
    parser.add_argument("--smp", type=positive_int, default=1)
    parser.add_argument("--strict", action="store_true")
    parser.add_argument("--timeout", type=positive_float, default=90.0)
    parser.add_argument("--out", type=Path)
    parser.add_argument("--allow-same-kernel", action="store_true")
    parser.add_argument(
        "guest_args",
        nargs=argparse.REMAINDER,
        help="arguments after -- are appended to the guest kernel command line",
    )
    args = parser.parse_args(argv)
    if args.guest_args[:1] == ["--"]:
        args.guest_args = args.guest_args[1:]
    for option in ("label_a", "label_b", "expect_a", "expect_b"):
        value = getattr(args, option)
        if (
            not value
            or len(value.encode()) > MAX_RESULT_LINE_BYTES
            or "\n" in value
            or "\r" in value
        ):
            parser.error(f"--{option.replace('_', '-')} must be one nonempty line")
    if not os.access(args.runner, os.X_OK):
        parser.error(
            f"runner is not executable: {args.runner} "
            "(build it with: cargo build --release --bin rivemu-runner)"
        )
    return args


def sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        for chunk in iter(lambda: stream.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def git_metadata() -> dict[str, Any]:
    def git(*arguments: str) -> subprocess.CompletedProcess[str]:
        return subprocess.run(
            ["git", "-C", str(ROOT), *arguments],
            text=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            timeout=10,
            check=False,
        )

    try:
        head = git("rev-parse", "HEAD")
        status = git("status", "--porcelain=v1", "--untracked-files=all")
    except (OSError, subprocess.TimeoutExpired) as error:
        return {"commit": None, "dirty": None, "status": None, "error": str(error)}
    if head.returncode != 0 or status.returncode != 0:
        return {
            "commit": None,
            "dirty": None,
            "status": None,
            "error": (head.stderr + status.stderr).strip(),
        }
    status_text = status.stdout.rstrip()
    return {
        "commit": head.stdout.strip(),
        "dirty": bool(status_text),
        "status": status_text,
        "error": None,
    }


def terminate_process_group(process: subprocess.Popen[bytes]) -> None:
    group_id = process.pid
    try:
        os.killpg(group_id, signal.SIGTERM)
    except ProcessLookupError:
        pass
    try:
        process.wait(timeout=2)
    except subprocess.TimeoutExpired:
        pass
    try:
        os.killpg(group_id, signal.SIGKILL)
    except ProcessLookupError:
        pass
    if process.poll() is None:
        try:
            process.wait(timeout=2)
        except subprocess.TimeoutExpired as error:
            raise EvidenceError(
                f"runner process group {group_id} survived SIGKILL"
            ) from error
    deadline = time.monotonic() + 2
    while time.monotonic() < deadline:
        try:
            os.killpg(group_id, 0)
        except ProcessLookupError:
            return
        time.sleep(0.05)
    raise EvidenceError(f"runner process group {group_id} survived SIGKILL")


def scan_console(path: Path, expected: str) -> tuple[int, int, list[str], bool]:
    expected_count = 0
    conflict_count = 0
    conflicts: list[str] = []
    retained_conflicts: set[str] = set()
    conflicts_truncated = False
    pending = bytearray()
    discarding_long_line = False

    def consume_line() -> None:
        nonlocal conflict_count, conflicts_truncated, expected_count
        line = bytes(pending)
        if line.endswith(b"\r"):
            line = line[:-1]
        decoded = line.decode("utf-8", errors="replace")
        if decoded == expected:
            expected_count += 1
        elif decoded.startswith(RESULT_PREFIX):
            conflict_count += 1
            if decoded not in retained_conflicts:
                if len(conflicts) < MAX_RETAINED_CONFLICTS:
                    conflicts.append(decoded)
                    retained_conflicts.add(decoded)
                else:
                    conflicts_truncated = True

    with path.open("rb") as stream:
        while chunk := stream.read(64 * 1024):
            parts = chunk.split(b"\n")
            for index, part in enumerate(parts):
                terminated = index + 1 < len(parts)
                if discarding_long_line:
                    if terminated:
                        discarding_long_line = False
                    continue
                if len(pending) + len(part) > MAX_RESULT_LINE_BYTES:
                    pending.clear()
                    discarding_long_line = not terminated
                    continue
                pending.extend(part)
                if terminated:
                    consume_line()
                    pending.clear()
    if pending and not discarding_long_line:
        consume_line()
    return expected_count, conflict_count, sorted(conflicts), conflicts_truncated


def run_one(
    *,
    name: str,
    label: str,
    kernel: Path,
    expected: str,
    runner: Path,
    init: Path,
    guest_args: list[str],
    smp: int,
    strict: bool,
    timeout: float,
    output_dir: Path,
) -> RunResult:
    command = [str(runner), str(init), *guest_args]
    environment = os.environ.copy()
    environment["RIVEMU_KERNEL"] = str(kernel)
    environment["RIVEMU_SMP"] = str(smp)
    environment["RIVEMU_REQUIRE_POWEROFF"] = "1"
    environment.pop("RIVEMU_SHARE_DIR", None)
    if strict:
        environment["RIVEMU_STRICT"] = "1"
    else:
        environment.pop("RIVEMU_STRICT", None)

    started = time.monotonic()
    timed_out = False
    launch_error = None
    returncode: int | None
    console_name = f"{name.lower()}-console.log"
    console_path = output_dir / console_name
    with console_path.open("wb") as console:
        try:
            process = subprocess.Popen(
                command,
                env=environment,
                stdout=console,
                stderr=subprocess.STDOUT,
                start_new_session=True,
            )
            try:
                try:
                    process.wait(timeout=timeout)
                except subprocess.TimeoutExpired:
                    timed_out = True
                    terminate_process_group(process)
            except BaseException:
                terminate_process_group(process)
                raise
            returncode = process.returncode
        except OSError as error:
            launch_error = str(error)
            returncode = None
            console.write(f"linux_ab.py: failed to launch runner: {error}\n".encode())
    elapsed = time.monotonic() - started

    expected_count, conflict_count, conflicts, conflicts_truncated = scan_console(
        console_path, expected
    )
    passed = (
        not timed_out
        and launch_error is None
        and returncode == 0
        and expected_count == 1
        and conflict_count == 0
    )
    return RunResult(
        name=name,
        label=label,
        kernel=kernel,
        expected=expected,
        command=command,
        elapsed_seconds=round(elapsed, 6),
        returncode=returncode,
        timed_out=timed_out,
        expected_line_count=expected_count,
        conflicting_result_count=conflict_count,
        conflicting_result_lines=conflicts,
        conflicting_result_lines_truncated=conflicts_truncated,
        launch_error=launch_error,
        passed=passed,
        console_file=console_name,
    )


def markdown_code(value: str) -> str:
    escaped = value.replace("`", "\\`").replace("|", "\\|")
    return f"`{escaped}`"


def workspace_revision(metadata: dict[str, Any]) -> str:
    commit = metadata.get("commit") or "unknown"
    revision = commit[:12]
    if metadata.get("dirty"):
        revision += "-dirty"
    return revision


def make_report(manifest: dict[str, Any], run_a: RunResult, run_b: RunResult) -> str:
    passed = manifest["passed"]
    control = manifest["control"]
    workspace_start = manifest["workspace"]["start"]
    workspace_end = manifest["workspace"]["end"]
    revision_start = workspace_revision(workspace_start)
    revision_end = workspace_revision(workspace_end)
    inputs = manifest["inputs"]
    config = manifest["config"]
    strict_text = "strict mode" if config["strict"] else "non-strict mode"
    guest_args = shlex.join(config["guest_args"]) if config["guest_args"] else "(none)"
    if control:
        status = "CONTROL PASS" if passed else "CONTROL FAIL"
        title = "# Linux same-kernel control"
    else:
        status = "PASS" if passed else "FAIL"
        title = "# Linux kernel A/B evidence"
    lines = [
        title,
        "",
        f"Overall result: **{status}**",
        "",
        "| Run | Kernel label | Expected exact line | Exit | Timeout | Verdict |",
        "|---|---|---|---:|---|---|",
    ]
    for run in (run_a, run_b):
        lines.append(
            f"| {run.name} | {markdown_code(run.label)} | "
            f"{markdown_code(run.expected)} | {run.returncode} | "
            f"{'yes' if run.timed_out else 'no'} | "
            f"{'PASS' if run.passed else 'FAIL'} |"
        )
    lines.extend(["", "## Interpretation", ""])
    if control and passed:
        lines.extend(
            [
                (
                    "Same-kernel control passed with rivemu runner sha256 "
                    f"{inputs['runner']['sha256']}."
                ),
                (
                    "This validates the orchestration and oracle path only; it is "
                    "not Linux kernel A/B evidence and must not be quoted as such."
                ),
            ]
        )
    elif control:
        lines.append(
            "The same-kernel control failed; inspect the console logs and manifest "
            "before running a kernel A/B comparison."
        )
    elif passed:
        lines.extend(
            [
                (
                    "Verified A/B with rivemu runner sha256 "
                    f"{inputs['runner']['sha256']}:"
                ),
                f"A ({run_a.label}, Image sha256 {inputs['kernel_a']['sha256']}): {run_a.expected}",
                f"B ({run_b.label}, Image sha256 {inputs['kernel_b']['sha256']}): {run_b.expected}",
                (
                    "Both runs used reproducer sha256 "
                    f"{inputs['init']['sha256']}, {config['smp']} hart(s), "
                    f"{strict_text}, and the same guest arguments: {guest_args}."
                ),
                (
                    f"The source workspace was {revision_start} at launch; the "
                    "runner hash above is the executable identity, and no "
                    "binary-to-source revision binding is asserted."
                ),
            ]
        )
    else:
        lines.append(
            "A/B verification did not pass; inspect the console logs and manifest "
            "before quoting it."
        )
    lines.extend(
        [
            "",
            "## Artifact identity",
            "",
            f"- Runner sha256: {markdown_code(inputs['runner']['sha256'])}",
            f"- Driver sha256: {markdown_code(inputs['driver']['sha256'])}",
            f"- Reproducer/init sha256: {markdown_code(inputs['init']['sha256'])}",
            f"- Workspace source at launch: {markdown_code(revision_start)}",
            f"- Workspace source after runs: {markdown_code(revision_end)}",
            (
                "- Inputs unchanged across both runs: "
                f"{'yes' if manifest['input_validation']['stable'] else 'NO'}"
            ),
            f"- SMP: {config['smp']}",
            f"- Mode: {strict_text}",
            "- Clean poweroff required: yes",
            f"- Timeout per run: {config['timeout_seconds']} seconds",
            f"- Guest arguments: {markdown_code(guest_args)}",
            "",
            "Full commands, paths, host details, hashes, elapsed times, and "
            "validation fields are in `manifest.json`.",
            "",
        ]
    )
    return "\n".join(lines)


def file_manifest(path: Path) -> dict[str, Any]:
    return {"path": str(path), "size": path.stat().st_size, "sha256": sha256(path)}


def collect_inputs(args: argparse.Namespace) -> dict[str, dict[str, Any]]:
    return {
        "kernel_a": file_manifest(args.kernel_a),
        "kernel_b": file_manifest(args.kernel_b),
        "init": file_manifest(args.init),
        "runner": file_manifest(args.runner),
        "driver": file_manifest(Path(__file__).resolve()),
    }


def revalidate_inputs(
    before: dict[str, dict[str, Any]],
) -> tuple[dict[str, dict[str, Any]], list[str]]:
    after: dict[str, dict[str, Any]] = {}
    changes = []
    for name, original in before.items():
        path = Path(original["path"])
        try:
            current = file_manifest(path)
        except OSError as error:
            current = {"path": str(path), "error": str(error)}
        after[name] = current
        if current.get("size") != original["size"] or current.get("sha256") != original["sha256"]:
            changes.append(name)
    return after, changes


def default_output_dir() -> Path:
    stamp = dt.datetime.now(dt.timezone.utc).strftime("%Y%m%dT%H%M%SZ")
    return ROOT / "linux-ab-results" / f"{stamp}-{os.getpid()}"


def execute(args: argparse.Namespace) -> tuple[Path, bool, bool]:
    if os.environ.get("RIVEMU_SHARE_DIR"):
        raise EvidenceError(
            "RIVEMU_SHARE_DIR is not allowed because mutable shared state would "
            "make the two runs non-identical"
        )

    inputs = collect_inputs(args)
    control = inputs["kernel_a"]["sha256"] == inputs["kernel_b"]["sha256"]
    if control and not args.allow_same_kernel:
        raise EvidenceError(
            "kernel A and B have identical SHA-256 hashes; use "
            "--allow-same-kernel only for a control run"
        )
    workspace_start = git_metadata()

    output_dir = (args.out or default_output_dir()).expanduser().resolve()
    try:
        output_dir.mkdir(parents=True, exist_ok=False)
    except FileExistsError as error:
        raise EvidenceError(
            f"refusing to overwrite existing output directory: {output_dir}"
        ) from error

    run_a = run_one(
        name="A",
        label=args.label_a,
        kernel=args.kernel_a,
        expected=args.expect_a,
        runner=args.runner,
        init=args.init,
        guest_args=args.guest_args,
        smp=args.smp,
        strict=args.strict,
        timeout=args.timeout,
        output_dir=output_dir,
    )
    run_b = run_one(
        name="B",
        label=args.label_b,
        kernel=args.kernel_b,
        expected=args.expect_b,
        runner=args.runner,
        init=args.init,
        guest_args=args.guest_args,
        smp=args.smp,
        strict=args.strict,
        timeout=args.timeout,
        output_dir=output_dir,
    )

    inputs_after, changed_inputs = revalidate_inputs(inputs)
    workspace_end = git_metadata()
    passed = run_a.passed and run_b.passed and not changed_inputs

    manifest: dict[str, Any] = {
        "schema_version": 2,
        "created_utc": dt.datetime.now(dt.timezone.utc).isoformat(),
        "mode": "control" if control else "a-b",
        "control": control,
        "passed": passed,
        "workspace": {"start": workspace_start, "end": workspace_end},
        "host": {
            "platform": platform.platform(),
            "machine": platform.machine(),
            "python": platform.python_version(),
        },
        "inputs": inputs,
        "input_validation": {
            "stable": not changed_inputs,
            "changed": changed_inputs,
            "after": inputs_after,
        },
        "config": {
            "smp": args.smp,
            "strict": args.strict,
            "timeout_seconds": args.timeout,
            "guest_args": args.guest_args,
            "allow_same_kernel": args.allow_same_kernel,
            "require_poweroff": True,
            "share_dir": None,
        },
        "runs": [run_a.manifest(), run_b.manifest()],
    }
    (output_dir / "manifest.json").write_text(
        json.dumps(manifest, indent=2, sort_keys=True) + "\n", encoding="utf-8"
    )
    (output_dir / "report.md").write_text(
        make_report(manifest, run_a, run_b), encoding="utf-8"
    )
    return output_dir, passed, control


def main(argv: list[str] | None = None) -> int:
    args = parse_args(sys.argv[1:] if argv is None else argv)
    try:
        output_dir, passed, control = execute(args)
    except EvidenceError as error:
        print(f"linux_ab.py: {error}", file=sys.stderr)
        return 2
    kind = "same-kernel control" if control else "A/B evidence"
    print(f"Linux {kind}: {'PASS' if passed else 'FAIL'}")
    print(f"Artifacts: {output_dir}")
    return 0 if passed else 1


if __name__ == "__main__":
    raise SystemExit(main())
