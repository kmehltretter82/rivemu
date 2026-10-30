from __future__ import annotations

import json
import os
import subprocess
import sys
import tempfile
import textwrap
import time
import unittest
from pathlib import Path


ROOT = Path(__file__).resolve().parents[2]
SCRIPT = ROOT / "scripts/linux_ab.py"


class LinuxAbTests(unittest.TestCase):
    def setUp(self) -> None:
        self.temporary = tempfile.TemporaryDirectory()
        self.directory = Path(self.temporary.name)
        self.kernel_a = self.directory / "Image.a"
        self.kernel_b = self.directory / "Image.b"
        self.init = self.directory / "init"
        self.runner = self.directory / "fake-runner"
        self.kernel_a.write_text("kernel-a", encoding="utf-8")
        self.kernel_b.write_text("kernel-b", encoding="utf-8")
        self.init.write_text("fake-init", encoding="utf-8")

    def tearDown(self) -> None:
        self.temporary.cleanup()

    def write_runner(self, body: str) -> None:
        self.runner.write_text(
            "#!/usr/bin/env python3\n"
            "import os\n"
            "import pathlib\n"
            "import subprocess\n"
            "import sys\n"
            "import time\n"
            + textwrap.dedent(body),
            encoding="utf-8",
        )
        self.runner.chmod(0o755)

    def command(
        self,
        output: Path,
        *extra: str,
        expect_a: str = "RIVEMU_AB_RESULT=BUG",
        expect_b: str = "RIVEMU_AB_RESULT=PASS",
    ) -> list[str]:
        return [
            sys.executable,
            str(SCRIPT),
            "--kernel-a",
            str(self.kernel_a),
            "--kernel-b",
            str(self.kernel_b),
            "--label-a",
            "bad-commit",
            "--label-b",
            "fixed-commit",
            "--init",
            str(self.init),
            "--expect-a",
            expect_a,
            "--expect-b",
            expect_b,
            "--runner",
            str(self.runner),
            "--out",
            str(output),
            *extra,
        ]

    def test_success_writes_hashed_report_grade_bundle(self) -> None:
        self.write_runner(
            """
            assert os.environ.get("RIVEMU_REQUIRE_POWEROFF") == "1"
            kernel = pathlib.Path(os.environ["RIVEMU_KERNEL"]).read_text()
            print("RIVEMU_AB_RESULT=BUG" if kernel == "kernel-a" else "RIVEMU_AB_RESULT=PASS")
            """
        )
        output = self.directory / "evidence"
        completed = subprocess.run(
            self.command(output, "--strict", "--", "guest-arg=42"),
            text=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            check=False,
        )

        self.assertEqual(completed.returncode, 0, completed.stderr)
        manifest = json.loads((output / "manifest.json").read_text(encoding="utf-8"))
        self.assertTrue(manifest["passed"])
        self.assertTrue(manifest["config"]["strict"])
        self.assertEqual(manifest["config"]["guest_args"], ["guest-arg=42"])
        self.assertEqual(len(manifest["inputs"]["kernel_a"]["sha256"]), 64)
        self.assertTrue(all(run["passed"] for run in manifest["runs"]))
        self.assertIn("RIVEMU_AB_RESULT=BUG", (output / "a-console.log").read_text())
        self.assertIn("RIVEMU_AB_RESULT=PASS", (output / "b-console.log").read_text())
        report = (output / "report.md").read_text(encoding="utf-8")
        self.assertIn("Verified A/B with rivemu", report)
        self.assertIn("bad-commit", report)
        self.assertIn("fixed-commit", report)

    def test_conflicting_marker_fails_but_preserves_both_runs(self) -> None:
        self.write_runner(
            """
            kernel = pathlib.Path(os.environ["RIVEMU_KERNEL"]).read_text()
            if kernel == "kernel-a":
                print("RIVEMU_AB_RESULT=BUG")
                print("RIVEMU_AB_RESULT=PASS")
            else:
                print("RIVEMU_AB_RESULT=PASS")
            """
        )
        output = self.directory / "conflict"
        completed = subprocess.run(
            self.command(output),
            text=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            check=False,
        )

        self.assertEqual(completed.returncode, 1)
        manifest = json.loads((output / "manifest.json").read_text(encoding="utf-8"))
        self.assertFalse(manifest["passed"])
        self.assertFalse(manifest["runs"][0]["passed"])
        self.assertEqual(
            manifest["runs"][0]["conflicting_result_lines"],
            ["RIVEMU_AB_RESULT=PASS"],
        )
        self.assertTrue(manifest["runs"][1]["passed"])
        self.assertIn("did not pass", (output / "report.md").read_text(encoding="utf-8"))

    def test_timeout_kills_a_and_still_runs_b(self) -> None:
        child_pid_file = self.directory / "child.pid"
        self.write_runner(
            f"""
            kernel = pathlib.Path(os.environ["RIVEMU_KERNEL"]).read_text()
            if kernel == "kernel-a":
                child = subprocess.Popen([
                    sys.executable,
                    "-c",
                    "import signal,time; signal.signal(signal.SIGTERM, signal.SIG_IGN); time.sleep(60)",
                ])
                pathlib.Path({str(child_pid_file)!r}).write_text(str(child.pid))
                time.sleep(10)
            else:
                print("RIVEMU_AB_RESULT=PASS")
            """
        )
        output = self.directory / "timeout"
        completed = subprocess.run(
            self.command(output, "--timeout", "0.5"),
            text=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            timeout=5,
            check=False,
        )

        self.assertEqual(completed.returncode, 1)
        manifest = json.loads((output / "manifest.json").read_text(encoding="utf-8"))
        self.assertTrue(manifest["runs"][0]["timed_out"])
        self.assertFalse(manifest["runs"][0]["passed"])
        self.assertTrue(manifest["runs"][1]["passed"])
        child_pid = int(child_pid_file.read_text(encoding="utf-8"))
        deadline = time.monotonic() + 2
        child_alive = True
        while time.monotonic() < deadline:
            try:
                os.kill(child_pid, 0)
            except ProcessLookupError:
                child_alive = False
                break
            time.sleep(0.05)
        self.assertFalse(child_alive, f"orphaned child process {child_pid}")

    def test_identical_kernels_require_explicit_control_flag(self) -> None:
        self.write_runner('print("RIVEMU_AB_RESULT=BUG")\n')
        self.kernel_b.write_bytes(self.kernel_a.read_bytes())
        output = self.directory / "same"
        completed = subprocess.run(
            self.command(output),
            text=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            check=False,
        )

        self.assertEqual(completed.returncode, 2)
        self.assertIn("identical SHA-256", completed.stderr)
        self.assertFalse(output.exists())

    def test_identical_kernel_control_never_claims_verified_ab(self) -> None:
        self.write_runner('print("RIVEMU_AB_RESULT=PASS")\n')
        self.kernel_b.write_bytes(self.kernel_a.read_bytes())
        output = self.directory / "control"
        completed = subprocess.run(
            self.command(
                output,
                "--allow-same-kernel",
                expect_a="RIVEMU_AB_RESULT=PASS",
            ),
            text=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            check=False,
        )

        self.assertEqual(completed.returncode, 0, completed.stderr)
        manifest = json.loads((output / "manifest.json").read_text(encoding="utf-8"))
        self.assertTrue(manifest["control"])
        report = (output / "report.md").read_text(encoding="utf-8")
        self.assertIn("CONTROL PASS", report)
        self.assertIn("not Linux kernel A/B evidence", report)
        self.assertNotIn("Verified A/B", report)

    def test_input_mutation_invalidates_otherwise_passing_runs(self) -> None:
        self.write_runner(
            """
            kernel = pathlib.Path(os.environ["RIVEMU_KERNEL"]).read_text()
            if kernel == "kernel-a":
                pathlib.Path(sys.argv[1]).write_text("mutated-init")
                print("RIVEMU_AB_RESULT=BUG")
            else:
                print("RIVEMU_AB_RESULT=PASS")
            """
        )
        output = self.directory / "mutation"
        completed = subprocess.run(
            self.command(output),
            text=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            check=False,
        )

        self.assertEqual(completed.returncode, 1)
        manifest = json.loads((output / "manifest.json").read_text(encoding="utf-8"))
        self.assertFalse(manifest["passed"])
        self.assertFalse(manifest["input_validation"]["stable"])
        self.assertEqual(manifest["input_validation"]["changed"], ["init"])

    def test_oversized_log_line_is_discarded_without_hiding_marker(self) -> None:
        self.write_runner(
            """
            print("x" * 100_000)
            kernel = pathlib.Path(os.environ["RIVEMU_KERNEL"]).read_text()
            print("RIVEMU_AB_RESULT=BUG" if kernel == "kernel-a" else "RIVEMU_AB_RESULT=PASS")
            """
        )
        output = self.directory / "large-log"
        completed = subprocess.run(
            self.command(output),
            text=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            check=False,
        )

        self.assertEqual(completed.returncode, 0, completed.stderr)
        self.assertGreater((output / "a-console.log").stat().st_size, 100_000)

    def test_conflicting_result_flood_retains_only_bounded_sample(self) -> None:
        self.write_runner(
            """
            for index in range(100):
                print(f"RIVEMU_AB_RESULT=CONFLICT_{index}")
            kernel = pathlib.Path(os.environ["RIVEMU_KERNEL"]).read_text()
            print("RIVEMU_AB_RESULT=BUG" if kernel == "kernel-a" else "RIVEMU_AB_RESULT=PASS")
            """
        )
        output = self.directory / "conflict-flood"
        completed = subprocess.run(
            self.command(output),
            text=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            check=False,
        )

        self.assertEqual(completed.returncode, 1)
        manifest = json.loads((output / "manifest.json").read_text(encoding="utf-8"))
        first = manifest["runs"][0]
        self.assertEqual(first["conflicting_result_count"], 100)
        self.assertEqual(len(first["conflicting_result_lines"]), 16)
        self.assertTrue(first["conflicting_result_lines_truncated"])

    def test_matching_marker_does_not_hide_nonzero_exit(self) -> None:
        self.write_runner(
            """
            kernel = pathlib.Path(os.environ["RIVEMU_KERNEL"]).read_text()
            if kernel == "kernel-a":
                print("RIVEMU_AB_RESULT=BUG")
                raise SystemExit(3)
            print("RIVEMU_AB_RESULT=PASS")
            """
        )
        output = self.directory / "nonzero"
        completed = subprocess.run(
            self.command(output),
            text=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            check=False,
        )

        self.assertEqual(completed.returncode, 1)
        manifest = json.loads((output / "manifest.json").read_text(encoding="utf-8"))
        self.assertEqual(manifest["runs"][0]["returncode"], 3)
        self.assertFalse(manifest["runs"][0]["passed"])

    def test_duplicate_expected_marker_is_ambiguous(self) -> None:
        self.write_runner(
            """
            kernel = pathlib.Path(os.environ["RIVEMU_KERNEL"]).read_text()
            marker = "RIVEMU_AB_RESULT=BUG" if kernel == "kernel-a" else "RIVEMU_AB_RESULT=PASS"
            print(marker)
            if kernel == "kernel-a":
                print(marker)
            """
        )
        output = self.directory / "duplicate"
        completed = subprocess.run(
            self.command(output),
            text=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            check=False,
        )

        self.assertEqual(completed.returncode, 1)
        manifest = json.loads((output / "manifest.json").read_text(encoding="utf-8"))
        self.assertEqual(manifest["runs"][0]["expected_line_count"], 2)
        self.assertFalse(manifest["runs"][0]["passed"])

    def test_existing_output_directory_is_never_overwritten(self) -> None:
        self.write_runner('print("RIVEMU_AB_RESULT=PASS")\n')
        output = self.directory / "existing"
        output.mkdir()
        sentinel = output / "keep"
        sentinel.write_text("unchanged", encoding="utf-8")
        completed = subprocess.run(
            self.command(output),
            text=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            check=False,
        )

        self.assertEqual(completed.returncode, 2)
        self.assertEqual(sentinel.read_text(encoding="utf-8"), "unchanged")


if __name__ == "__main__":
    unittest.main()
