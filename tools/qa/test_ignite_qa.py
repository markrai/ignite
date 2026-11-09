#!/usr/bin/env python3
"""Unit tests for reusable Ignite QA tooling (no device mutation, no publish)."""

from __future__ import annotations

import json
import struct
import sys
import tempfile
import unittest
from pathlib import Path

_QA = Path(__file__).resolve().parent
if str(_QA) not in sys.path:
    sys.path.insert(0, str(_QA))

from elf import ElfError, inspect_armv7hf_elf  # noqa: E402
from evidence import (  # noqa: E402
    STATUS_BLOCKED,
    STATUS_FAIL,
    STATUS_NOT_RUN,
    STATUS_PASS,
    evaluate_states,
    group_status,
)
from env import _untracked_content_payload  # noqa: E402
from ignite_qa import _preserve_evidence, _run_stamp  # noqa: E402
from preflight import evaluate_isolation, parse_preflight_kv, run_preflight  # noqa: E402
from qualify import GROUPS, Qualify  # noqa: E402


def _fake_arm_elf(path: Path) -> None:
    interp = b"/lib/ld-linux-armhf.so.3\x00"
    ehdr = bytearray(52)
    ehdr[0:4] = b"\x7fELF"
    ehdr[4] = 1
    ehdr[5] = 1
    ehdr[6] = 1
    struct.pack_into("<H", ehdr, 16, 3)
    struct.pack_into("<H", ehdr, 18, 40)
    struct.pack_into("<I", ehdr, 20, 1)
    struct.pack_into("<I", ehdr, 28, 52)
    struct.pack_into("<I", ehdr, 36, 0x05000400)
    struct.pack_into("<H", ehdr, 40, 52)
    struct.pack_into("<H", ehdr, 42, 32)
    struct.pack_into("<H", ehdr, 44, 1)
    phdr = bytearray(32)
    struct.pack_into("<I", phdr, 0, 3)
    struct.pack_into("<I", phdr, 4, 52 + 32)
    struct.pack_into("<I", phdr, 16, len(interp))
    path.write_bytes(bytes(ehdr) + bytes(phdr) + interp)


class ElfTests(unittest.TestCase):
    def test_accepts_armv7hf_hard_float_stripped(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "scribe-app"
            _fake_arm_elf(path)
            info = inspect_armv7hf_elf(path)
            self.assertEqual(info["machine"], "ARM")
            self.assertTrue(info["hard_float"])
            self.assertEqual(info["interpreter"], "/lib/ld-linux-armhf.so.3")
            self.assertTrue(info["stripped"])

    def test_refuses_non_elf(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "scribe-app"
            path.write_bytes(b"not-elf")
            with self.assertRaises(ElfError):
                inspect_armv7hf_elf(path)


class IsolationTests(unittest.TestCase):
    def test_production_slot_refuses_physical_qa(self) -> None:
        kv = parse_preflight_kv(
            """
host_has_prod_data=yes
host_has_prod_sync=yes
host_has_prod_device=yes
notebook_dirs=25
prod_pairing_files=present
qa_root=absent
qa_host=absent
qa_rows=0
"""
        )
        result = evaluate_isolation(kv)
        self.assertTrue(result["production_bound"])
        self.assertFalse(result["isolated_qa_slot_available"])
        self.assertTrue(result["refuse_physical_qa"])
        self.assertEqual(result["notebook_count"], 25)
        self.assertIn("/mnt/us/scribe-data", result["refuse_reason"])

    def test_empty_production_slot_still_refused_without_qa_handler(self) -> None:
        result = evaluate_isolation(
            {
                "host_has_prod_data": "yes",
                "host_has_prod_sync": "yes",
                "host_has_prod_device": "yes",
                "notebook_dirs": "0",
                "prod_pairing_files": "absent",
                "qa_root": "absent",
                "qa_host": "absent",
                "qa_rows": "0",
            }
        )
        self.assertTrue(result["refuse_physical_qa"])

    def test_qa_slot_would_clear_isolation_only_when_complete(self) -> None:
        result = evaluate_isolation(
            {
                "host_has_prod_data": "no",
                "host_has_prod_sync": "no",
                "host_has_prod_device": "no",
                "notebook_dirs": "0",
                "prod_pairing_files": "absent",
                "qa_root": "present",
                "qa_host": "present",
                "qa_rows": "1",
            }
        )
        self.assertTrue(result["isolated_qa_slot_available"])
        self.assertFalse(result["refuse_physical_qa"])


class StateMachineTests(unittest.TestCase):
    def test_dirty_tree_blocks_release_candidate(self) -> None:
        report = {
            "source": {"commit": "abc", "dirty": True, "version": "9.9.9"},
            "groups": {
                "identity": {"status": STATUS_PASS},
                "rust": {"status": STATUS_PASS},
                "wasm": {"status": STATUS_PASS},
                "web": {"status": STATUS_PASS},
                "e2e": {"status": STATUS_PASS},
                "armv7": {"status": STATUS_PASS},
            },
            "artifacts": {"scribe-app": {"sha256": "ab"}},
        }
        states = evaluate_states(report=report, preflight=None, physical_pass=None)
        self.assertEqual(states["SOURCE_READY"]["status"], STATUS_PASS)
        self.assertEqual(states["AUTOMATED_VERIFIED"]["status"], STATUS_PASS)
        self.assertEqual(states["DEVICE_PREFLIGHT_PASS"]["status"], STATUS_NOT_RUN)
        self.assertEqual(states["RELEASE_CANDIDATE"]["status"], STATUS_BLOCKED)
        self.assertIn("unrecorded local changes", states["RELEASE_CANDIDATE"]["reason"])

    def test_missing_groups_are_not_pass(self) -> None:
        report = {
            "source": {"commit": "abc", "dirty": False},
            "groups": {
                "identity": {"status": STATUS_PASS},
                "rust": {"status": STATUS_NOT_RUN},
                "wasm": {"status": STATUS_NOT_RUN},
                "web": {"status": STATUS_NOT_RUN},
                "e2e": {"status": STATUS_NOT_RUN},
                "armv7": {"status": STATUS_NOT_RUN},
            },
        }
        states = evaluate_states(report=report, preflight=None, physical_pass=None)
        self.assertEqual(states["AUTOMATED_VERIFIED"]["status"], STATUS_NOT_RUN)
        self.assertEqual(states["PUBLISHED"]["status"], STATUS_NOT_RUN)

    def test_production_preflight_blocks_device_pass(self) -> None:
        report = {
            "source": {"commit": "abc", "dirty": False},
            "groups": {name: {"status": STATUS_PASS} for name in GROUPS},
            "artifacts": {"scribe-app": {"sha256": "ab"}},
        }
        preflight = {
            "refuse_physical_qa": True,
            "refuse_reason": "production-bound slot",
            "isolated_qa_slot_available": False,
            "status": STATUS_BLOCKED,
        }
        states = evaluate_states(report=report, preflight=preflight, physical_pass=None)
        self.assertEqual(states["DEVICE_PREFLIGHT_PASS"]["status"], STATUS_BLOCKED)
        self.assertEqual(states["PHYSICAL_QA_PASS"]["status"], STATUS_NOT_RUN)


class StaleEvidenceTests(unittest.TestCase):
    def test_mismatched_fingerprint_cannot_advance_release(self) -> None:
        report = {
            "source": {
                "commit": "abc",
                "dirty": False,
                "tree_fingerprint": "aaa",
                "version": "9.9.9",
            },
            "groups": {name: {"status": STATUS_PASS} for name in GROUPS},
            "artifacts": {"scribe-app": {"sha256": "ab"}},
        }
        preflight = {
            "refuse_physical_qa": False,
            "isolated_qa_slot_available": True,
            "status": STATUS_PASS,
        }
        physical = {"passed": True, "artifact_sha256": "ab"}
        states = evaluate_states(
            report=report,
            preflight=preflight,
            physical_pass=physical,
            live_source={"commit": "abc", "dirty": False, "tree_fingerprint": "bbb"},
        )
        self.assertEqual(states["AUTOMATED_VERIFIED"]["status"], STATUS_BLOCKED)
        self.assertEqual(states["ARTIFACT_VERIFIED"]["status"], STATUS_BLOCKED)
        self.assertEqual(states["RELEASE_CANDIDATE"]["status"], STATUS_BLOCKED)
        self.assertIn("stale_evidence", states["RELEASE_CANDIDATE"]["reason"])


def _green_report() -> dict:
    return {
        "source": {
            "commit": "abc",
            "dirty": False,
            "tree_fingerprint": "fp1",
            "version": "0.9.23",
        },
        "groups": {name: {"status": STATUS_PASS} for name in GROUPS},
        "artifacts": {"scribe-app": {"sha256": "deadbeef"}},
    }


def _green_preflight() -> dict:
    return {
        "refuse_physical_qa": False,
        "isolated_qa_slot_available": True,
        "status": STATUS_PASS,
        "data_compatibility": "ok",
        "qualify_tree_fingerprint": "fp1",
        "artifact_match": {"status": STATUS_PASS, "qualified": "deadbeef", "installed": "deadbeef"},
    }


def _green_physical() -> dict:
    return {"passed": True, "artifact_sha256": "deadbeef"}


class FabricatedEvidenceTests(unittest.TestCase):
    def test_matching_evidence_can_be_release_candidate(self) -> None:
        states = evaluate_states(
            report=_green_report(),
            preflight=_green_preflight(),
            physical_pass=_green_physical(),
        )
        self.assertEqual(states["RELEASE_CANDIDATE"]["status"], STATUS_PASS)

    def test_physical_sha_mismatch_cannot_be_release_candidate(self) -> None:
        states = evaluate_states(
            report=_green_report(),
            preflight=_green_preflight(),
            physical_pass={"passed": True, "artifact_sha256": "cafebabe"},
        )
        self.assertEqual(states["PHYSICAL_QA_PASS"]["status"], STATUS_BLOCKED)
        self.assertEqual(states["RELEASE_CANDIDATE"]["status"], STATUS_BLOCKED)

    def test_unknown_data_compatibility_cannot_be_release_candidate(self) -> None:
        pf = _green_preflight()
        pf["data_compatibility"] = "unknown"
        states = evaluate_states(
            report=_green_report(), preflight=pf, physical_pass=_green_physical()
        )
        self.assertEqual(states["RELEASE_CANDIDATE"]["status"], STATUS_BLOCKED)
        self.assertIn("data_compatibility=unknown", states["RELEASE_CANDIDATE"]["reason"])

    def test_missing_data_compatibility_cannot_be_release_candidate(self) -> None:
        pf = _green_preflight()
        pf.pop("data_compatibility")
        states = evaluate_states(
            report=_green_report(), preflight=pf, physical_pass=_green_physical()
        )
        self.assertEqual(states["RELEASE_CANDIDATE"]["status"], STATUS_BLOCKED)
        self.assertIn("data_compatibility=missing", states["RELEASE_CANDIDATE"]["reason"])

    def test_preflight_fingerprint_mismatch_cannot_be_release_candidate(self) -> None:
        pf = _green_preflight()
        pf["qualify_tree_fingerprint"] = "other"
        states = evaluate_states(
            report=_green_report(), preflight=pf, physical_pass=_green_physical()
        )
        self.assertEqual(states["RELEASE_CANDIDATE"]["status"], STATUS_BLOCKED)
        self.assertIn("preflight qualify_tree_fingerprint mismatch", states["RELEASE_CANDIDATE"]["reason"])

    def test_fabricated_physical_pass_without_sha_is_blocked(self) -> None:
        states = evaluate_states(
            report=_green_report(),
            preflight=_green_preflight(),
            physical_pass={"passed": True},
        )
        self.assertEqual(states["PHYSICAL_QA_PASS"]["status"], STATUS_BLOCKED)
        self.assertEqual(states["RELEASE_CANDIDATE"]["status"], STATUS_BLOCKED)

    def test_untracked_fingerprint_includes_file_contents(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            (root / "secret.rs").write_text("alpha", encoding="utf-8")
            a = _untracked_content_payload(root, "?? secret.rs\n")
            (root / "secret.rs").write_text("beta", encoding="utf-8")
            b = _untracked_content_payload(root, "?? secret.rs\n")
            self.assertNotEqual(a, b)
            self.assertIn("secret.rs", a)

    def test_incompatible_data_compatibility_cannot_be_release_candidate(self) -> None:
        pf = _green_preflight()
        pf["data_compatibility"] = "incompatible"
        states = evaluate_states(
            report=_green_report(), preflight=pf, physical_pass=_green_physical()
        )
        self.assertEqual(states["RELEASE_CANDIDATE"]["status"], STATUS_BLOCKED)
        self.assertIn("data_compatibility=incompatible", states["RELEASE_CANDIDATE"]["reason"])

    def test_preflight_artifact_mismatch_cannot_be_release_candidate(self) -> None:
        pf = _green_preflight()
        pf["artifact_match"] = {
            "status": STATUS_PASS,
            "qualified": "cafebabe",
            "installed": "deadbeef",
        }
        states = evaluate_states(
            report=_green_report(), preflight=pf, physical_pass=_green_physical()
        )
        self.assertEqual(states["RELEASE_CANDIDATE"]["status"], STATUS_BLOCKED)
        self.assertIn(
            "preflight qualified artifact does not match",
            states["RELEASE_CANDIDATE"]["reason"],
        )

    def test_generated_untracked_paths_are_excluded_from_fingerprint(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            wasm = root / "ignite/web/src/wasm"
            wasm.mkdir(parents=True)
            (wasm / "scribe_web_wasm.wasm").write_bytes(b"generated")
            dist = root / "ignite/web/dist"
            dist.mkdir(parents=True)
            (dist / "index.html").write_text("x", encoding="utf-8")
            tgt = root / "target/qa-evidence"
            tgt.mkdir(parents=True)
            (tgt / "qualify.json").write_text("{}", encoding="utf-8")
            (root / "note.rs").write_text("keep", encoding="utf-8")
            payload = _untracked_content_payload(
                root,
                "?? ignite/web/src/wasm/scribe_web_wasm.wasm\n"
                "?? ignite/web/dist/\n"
                "?? target/qa-evidence/\n"
                "?? note.rs\n",
            )
            self.assertIn("note.rs", payload)
            self.assertNotIn("scribe_web_wasm.wasm", payload)
            self.assertNotIn("index.html", payload)
            self.assertNotIn("qualify.json", payload)


class QualifySelectionTests(unittest.TestCase):
    def test_unselected_groups_are_not_run(self) -> None:
        root = Path(__file__).resolve().parents[2]
        with tempfile.TemporaryDirectory() as tmp:
            calls: list[list[str]] = []

            def runner(argv, cwd, env, timeout):
                calls.append(list(argv))
                return {
                    "argv": argv,
                    "cwd": str(cwd),
                    "exit_code": 0,
                    "seconds": 0.01,
                    "stdout_tail": "ok",
                    "stderr_tail": "",
                }

            env = {
                "wsl_available": False,
                "linux_rust_available": False,
                "wasm32_available": False,
                "tools": {
                    "cargo": {"available": False, "how": "missing"},
                    "node": {"available": True, "how": "native"},
                    "npm": {"available": True, "how": "native"},
                    "npx": {"available": True, "how": "native"},
                    "docker": {"available": False, "how": "missing"},
                    "git": {"available": True, "how": "native"},
                },
                "pinned_rust_armv7hf_image": "rust:1-bullseye@sha256:dead",
            }
            report = Qualify(
                root,
                out_dir=Path(tmp),
                groups=("identity",),
                runner=runner,
                skip_env_probe=env,
            ).run()
            self.assertEqual(report["groups"]["identity"]["status"], STATUS_PASS)
            self.assertEqual(report["groups"]["rust"]["status"], STATUS_NOT_RUN)
            self.assertEqual(report["groups"]["armv7"]["status"], STATUS_NOT_RUN)
            self.assertEqual(report["overall"], STATUS_PASS)
            self.assertEqual(report["exit_code"], 0)
            self.assertTrue(any("check_version_sync.py" in " ".join(map(str, c)) for c in calls))
            vs = next(c for c in report["groups"]["identity"]["checks"] if c["name"] == "version-sync")
            self.assertIn("argv", vs["run"])
            self.assertTrue(report["source"].get("tree_fingerprint"))
            self.assertEqual(report["states"]["RELEASE_CANDIDATE"]["status"], STATUS_BLOCKED)

    def test_failed_selected_group_is_fail_not_pass(self) -> None:
        root = Path(__file__).resolve().parents[2]
        with tempfile.TemporaryDirectory() as tmp:
            def runner(argv, cwd, env, timeout):
                return {
                    "argv": argv,
                    "cwd": str(cwd),
                    "exit_code": 1,
                    "seconds": 0.01,
                    "stdout_tail": "boom",
                    "stderr_tail": "err",
                    "log": str(Path(tmp) / "cargo-test-workspace.log"),
                }

            env = {
                "wsl_available": True,
                "linux_rust_available": True,
                "wasm32_available": False,
                "tools": {"cargo": {"available": True, "how": "wsl"}, "docker": {"available": False}},
            }
            report = Qualify(
                root,
                out_dir=Path(tmp),
                groups=("rust",),
                runner=runner,
                skip_env_probe=env,
            ).run()
            self.assertEqual(report["groups"]["rust"]["status"], STATUS_FAIL)
            self.assertEqual(report["exit_code"], 1)
            ws = next(
                c for c in report["groups"]["rust"]["checks"] if c["name"] == "cargo-test-workspace"
            )
            self.assertEqual(ws["run"]["exit_code"], 1)
            self.assertTrue(ws["run"]["argv"])

    def test_timestamped_evidence_survives_latest_pointer(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            out_dir = Path(tmp)
            first = out_dir / _run_stamp()
            first.mkdir()
            _preserve_evidence(out_dir, "qualify", {"overall": "FAIL", "n": 1}, "first\n", first)
            second = out_dir / (_run_stamp() + "-b")
            second.mkdir()
            _preserve_evidence(out_dir, "qualify", {"overall": "PASS", "n": 2}, "second\n", second)
            self.assertEqual(json.loads((first / "qualify.json").read_text())["n"], 1)
            self.assertEqual(json.loads((out_dir / "qualify.json").read_text())["n"], 2)

    def test_missing_linux_cargo_is_not_run(self) -> None:
        root = Path(__file__).resolve().parents[2]
        with tempfile.TemporaryDirectory() as tmp:
            env = {
                "wsl_available": False,
                "linux_rust_available": False,
                "wasm32_available": False,
                "tools": {"cargo": {"available": True, "how": "native"}, "docker": {"available": False}},
            }
            report = Qualify(
                root,
                out_dir=Path(tmp),
                groups=("rust",),
                runner=lambda *a, **k: (_ for _ in ()).throw(AssertionError("must not invoke cargo")),
                skip_env_probe=env,
            ).run()
            self.assertEqual(report["groups"]["rust"]["status"], STATUS_NOT_RUN)
            self.assertEqual(report["exit_code"], 2)


class PreflightCommandTests(unittest.TestCase):
    def test_fixture_script_never_approves_production_slot(self) -> None:
        fixture = """
prettyversion=Kindle 5.18.6 (~~otaVersion~~)
arch=armv7l
health_candidate_version=0.9.22
bin_scribe-app_sha256=aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa
host_has_prod_data=yes
host_has_prod_sync=yes
host_has_prod_device=yes
notebook_dirs=25
prod_pairing_files=present
qa_root=absent
qa_host=absent
qa_rows=0
canonical_rows=1
canonical_command=/mnt/us/scribe-app/bin/scribe-lifecycle-host.sh
library_meta_sha256=bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb
library_meta_bytes=577
ignite_proc=none_matched
activeApp=com.lab126.KPPMainApp
preflight_exec_scribe_app=no
"""
        report = run_preflight(
            ssh_host="kindle",
            qualified_artifacts={
                "artifacts": {
                    "scribe-app": {
                        "sha256": "23e2b7282aae95b9adc53e754a580f11dc9f73b40e512bbbf4be4aa01245fbb8"
                    }
                }
            },
            run_script=lambda _script: fixture,
        )
        self.assertTrue(report["refuse_physical_qa"])
        self.assertFalse(report["isolated_qa_slot_available"])
        self.assertFalse(report["exec_scribe_app"])
        self.assertEqual(report["exit_code"], 3)
        self.assertEqual(report["notebook_count"], 25)
        self.assertEqual(report["artifact_match"]["status"], STATUS_FAIL)
        dumped = json.dumps(report)
        self.assertNotIn("Bearer", dumped)
        self.assertNotIn("server-profile.json contents", dumped)

    def test_unreachable_kindle_is_not_run_not_pass(self) -> None:
        report = run_preflight(
            ssh_host="kindle",
            run_script=lambda _script: (_ for _ in ()).throw(OSError("connection timed out")),
        )
        self.assertEqual(report["overall"], STATUS_NOT_RUN)
        self.assertEqual(report["exit_code"], 2)
        self.assertTrue(report["refuse_physical_qa"])
        self.assertFalse(report["exec_scribe_app"])

    def test_group_status_fail_beats_not_run(self) -> None:
        self.assertEqual(
            group_status(
                [
                    {"status": STATUS_PASS},
                    {"status": STATUS_NOT_RUN},
                    {"status": STATUS_FAIL},
                ]
            ),
            STATUS_FAIL,
        )


if __name__ == "__main__":
    unittest.main()
