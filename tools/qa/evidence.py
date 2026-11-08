"""Structured QA evidence and release-state evaluation (no publication)."""

from __future__ import annotations

from datetime import datetime, timezone
from typing import Any

SCHEMA = "ignite-qa-evidence-v1"
PREFLIGHT_SCHEMA = "ignite-kindle-preflight-v1"

STATES = (
    "SOURCE_READY",
    "AUTOMATED_VERIFIED",
    "ARTIFACT_VERIFIED",
    "DEVICE_PREFLIGHT_PASS",
    "PHYSICAL_QA_PASS",
    "RELEASE_CANDIDATE",
    "PUBLISHED",
    "OTA_VERIFIED",
)

STATUS_PASS = "PASS"
STATUS_FAIL = "FAIL"
STATUS_NOT_RUN = "NOT_RUN"
STATUS_BLOCKED = "BLOCKED"


def utc_now() -> str:
    return datetime.now(timezone.utc).replace(microsecond=0).isoformat().replace("+00:00", "Z")


def check(*, name: str, status: str, detail: str = "", extra: dict[str, Any] | None = None) -> dict[str, Any]:
    row: dict[str, Any] = {"name": name, "status": status, "detail": detail}
    if extra:
        row.update(extra)
    return row


def group_status(checks: list[dict[str, Any]]) -> str:
    if not checks:
        return STATUS_NOT_RUN
    statuses = [c["status"] for c in checks]
    if any(s == STATUS_FAIL for s in statuses):
        return STATUS_FAIL
    if any(s == STATUS_NOT_RUN for s in statuses):
        return STATUS_NOT_RUN
    if any(s == STATUS_BLOCKED for s in statuses):
        return STATUS_BLOCKED
    return STATUS_PASS


def _group(report: dict[str, Any], name: str) -> str:
    g = report.get("groups", {}).get(name) or {}
    return g.get("status", STATUS_NOT_RUN)


def evidence_matches_live(report: dict[str, Any], live_source: dict[str, Any] | None) -> tuple[bool, str]:
    """Evidence from another host or session is only reusable at the same tree fingerprint."""
    if live_source is None:
        return True, ""
    source = report.get("source") or {}
    recorded = source.get("tree_fingerprint") or ""
    if not recorded:
        return False, "incomplete_evidence: missing tree_fingerprint"
    live_fp = live_source.get("tree_fingerprint") or ""
    if live_fp and live_fp != recorded:
        return False, (
            f"stale_evidence: recorded tree_fingerprint {recorded[:16]}… "
            f"!= live {live_fp[:16]}…"
        )
    live_commit = live_source.get("commit") or ""
    recorded_commit = source.get("commit") or ""
    if live_commit and recorded_commit and live_commit != recorded_commit:
        return False, "stale_evidence: commit mismatch"
    return True, ""


def evaluate_states(
    *,
    report: dict[str, Any],
    preflight: dict[str, Any] | None,
    physical_pass: dict[str, Any] | None,
    live_source: dict[str, Any] | None = None,
) -> dict[str, Any]:
    """Objective release-state evidence. Never promotes, installs, or publishes."""
    source = report.get("source") or {}
    dirty = bool(source.get("dirty"))
    fresh, stale_reason = evidence_matches_live(report, live_source)
    version_ok = _group(report, "identity") == STATUS_PASS
    rust_ok = _group(report, "rust") == STATUS_PASS
    wasm_ok = _group(report, "wasm") == STATUS_PASS
    web_ok = _group(report, "web") == STATUS_PASS
    e2e_ok = _group(report, "e2e") == STATUS_PASS
    arm_ok = _group(report, "armv7") == STATUS_PASS

    states: dict[str, Any] = {}

    def put(name: str, status: str, reason: str) -> None:
        states[name] = {"status": status, "reason": reason}

    if version_ok and source.get("commit"):
        put(
            "SOURCE_READY",
            STATUS_PASS,
            "commit, version files, and toolchain inventory recorded"
            + (" (working tree dirty; cannot become RELEASE_CANDIDATE)" if dirty else ""),
        )
    elif _group(report, "identity") == STATUS_NOT_RUN:
        put("SOURCE_READY", STATUS_NOT_RUN, "identity group did not run")
    else:
        put("SOURCE_READY", STATUS_BLOCKED, "source identity incomplete or version files disagree")

    auto_bits = (rust_ok, wasm_ok, web_ok, e2e_ok)
    if all(auto_bits) and not fresh:
        put("AUTOMATED_VERIFIED", STATUS_BLOCKED, stale_reason)
    elif all(auto_bits):
        put("AUTOMATED_VERIFIED", STATUS_PASS, "rust, wasm, web, and chromium e2e passed")
    elif any(_group(report, n) == STATUS_FAIL for n in ("rust", "wasm", "web", "e2e")):
        put("AUTOMATED_VERIFIED", STATUS_FAIL, "a required automated group failed")
    else:
        put(
            "AUTOMATED_VERIFIED",
            STATUS_NOT_RUN,
            "rust/wasm/web/e2e not all PASS (missing groups are NOT_RUN, not PASS)",
        )

    artifacts = report.get("artifacts") or {}
    if arm_ok and artifacts and not fresh:
        put("ARTIFACT_VERIFIED", STATUS_BLOCKED, stale_reason)
    elif arm_ok and artifacts:
        put("ARTIFACT_VERIFIED", STATUS_PASS, "official ARMv7hf ELFs inspected (arch/ABI/stripped/sha256)")
    elif _group(report, "armv7") == STATUS_FAIL:
        put("ARTIFACT_VERIFIED", STATUS_FAIL, "ARMv7hf build or ELF verification failed")
    else:
        put("ARTIFACT_VERIFIED", STATUS_NOT_RUN, "official ARMv7hf build did not run")

    qualified_app = ((artifacts.get("scribe-app") or {}).get("sha256") or "").lower()

    if preflight is None:
        put("DEVICE_PREFLIGHT_PASS", STATUS_NOT_RUN, "kindle preflight not run")
    elif preflight.get("refuse_physical_qa"):
        put(
            "DEVICE_PREFLIGHT_PASS",
            STATUS_BLOCKED,
            preflight.get("refuse_reason") or "production-bound slot; isolation required",
        )
    elif preflight.get("isolated_qa_slot_available") and preflight.get("status") == STATUS_PASS:
        put("DEVICE_PREFLIGHT_PASS", STATUS_PASS, "isolated QA slot present and preflight passed")
    else:
        put("DEVICE_PREFLIGHT_PASS", STATUS_BLOCKED, "preflight did not prove an isolated QA slot")

    if physical_pass is None:
        put("PHYSICAL_QA_PASS", STATUS_NOT_RUN, "no human physical-QA evidence file")
    else:
        physical_sha = (physical_pass.get("artifact_sha256") or "").lower()
        if not physical_pass.get("passed"):
            put("PHYSICAL_QA_PASS", STATUS_BLOCKED, "physical QA evidence present but incomplete")
        elif not qualified_app:
            put("PHYSICAL_QA_PASS", STATUS_BLOCKED, "no qualified scribe-app sha256 to match")
        elif not physical_sha:
            put("PHYSICAL_QA_PASS", STATUS_BLOCKED, "physical QA evidence missing artifact_sha256")
        elif physical_sha != qualified_app:
            put(
                "PHYSICAL_QA_PASS",
                STATUS_BLOCKED,
                "physical QA artifact_sha256 does not match qualified scribe-app",
            )
        else:
            put("PHYSICAL_QA_PASS", STATUS_PASS, "human-signed physical QA matches qualified scribe-app sha256")

    unresolved = list(report.get("unresolved_gates") or [])
    if not fresh:
        unresolved.append(stale_reason)
    if preflight is not None:
        compat = preflight.get("data_compatibility")
        if compat != "ok":
            unresolved.append(f"data_compatibility={compat if compat not in (None, '') else 'missing'}")
        pf_fp = preflight.get("qualify_tree_fingerprint") or ""
        rec_fp = source.get("tree_fingerprint") or ""
        if rec_fp and pf_fp and pf_fp != rec_fp:
            unresolved.append("stale_evidence: preflight qualify_tree_fingerprint mismatch")
        elif rec_fp and not pf_fp:
            unresolved.append("incomplete_evidence: preflight missing qualify_tree_fingerprint")
        match = preflight.get("artifact_match") or {}
        matched_q = (match.get("qualified") or "").lower()
        if qualified_app and matched_q and matched_q != qualified_app:
            unresolved.append("stale_evidence: preflight qualified artifact does not match this qualify report")

    candidate_ok = (
        states["SOURCE_READY"]["status"] == STATUS_PASS
        and not dirty
        and states["AUTOMATED_VERIFIED"]["status"] == STATUS_PASS
        and states["ARTIFACT_VERIFIED"]["status"] == STATUS_PASS
        and states["DEVICE_PREFLIGHT_PASS"]["status"] == STATUS_PASS
        and states["PHYSICAL_QA_PASS"]["status"] == STATUS_PASS
        and not unresolved
    )
    if candidate_ok:
        put("RELEASE_CANDIDATE", STATUS_PASS, "all prior states passed; human publication still required")
    else:
        reasons = []
        if dirty:
            reasons.append("unrecorded local changes")
        if unresolved:
            reasons.append("unresolved compatibility gates: " + ", ".join(unresolved))
        if not reasons:
            reasons.append("prior required states are not all PASS")
        put("RELEASE_CANDIDATE", STATUS_BLOCKED, "; ".join(reasons))

    put("PUBLISHED", STATUS_NOT_RUN, "publication is a separate human-approved promote step")
    put("OTA_VERIFIED", STATUS_NOT_RUN, "OTA verification is a separate human-approved step")
    return states


def human_summary(report: dict[str, Any]) -> str:
    lines = [
        f"ignite-qa schema={report.get('schema')} command={report.get('command')} overall={report.get('overall')}",
    ]
    src = report.get("source") or {}
    if src.get("commit") or src.get("version"):
        lines.append(
            f"source version={src.get('version')} commit={src.get('commit')} dirty={src.get('dirty')} "
            f"fingerprint={(src.get('tree_fingerprint') or '')[:16]}"
        )
    for name, group in (report.get("groups") or {}).items():
        lines.append(f"  group {name}: {group.get('status')}")
        for c in group.get("checks") or []:
            lines.append(f"    {c.get('status'):8} {c.get('name')}  {c.get('detail', '')}")
    for name, st in (report.get("states") or {}).items():
        lines.append(f"  state {name}: {st.get('status')} — {st.get('reason')}")
    if report.get("refuse_physical_qa"):
        lines.append(f"physical QA refused: {report.get('refuse_reason')}")
    return "\n".join(lines) + "\n"
