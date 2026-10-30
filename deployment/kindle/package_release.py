#!/usr/bin/env python3
"""Shared Kindle packaging: stage payload, validate, emit Install Ignite vX.Y.Z.sh.

Called by the thin wrappers in scripts/kindle/. This module owns the payload
inventory, Dropbear PINNED.txt enforcement, SFX build, and checksums so
PowerShell and Bash do not duplicate staging logic.

This script never cross-compiles. Wrappers (scripts/kindle/package-release.sh,
package-release.ps1) build ARMv7hf ELFs and Dropbear, then invoke this module.
A local Windows -SkipBuild only skips the wrapper Docker step; it is not
provenance-safe.
"""

from __future__ import annotations

import argparse
import hashlib
import shutil
import stat
import sys
import zipfile
from pathlib import Path

_HERE = Path(__file__).resolve().parent
_SFX = _HERE / "sfx"
for extra in (_HERE, _SFX):
    if str(extra) not in sys.path:
        sys.path.insert(0, str(extra))

from build_sfx import build_sfx, default_template  # noqa: E402
from release_component_profiles import (  # noqa: E402
    ProfileError,
    verify_deterministic,
    write_profiles,
)
from validate_stage import (  # noqa: E402
    installer_sfx_name,
    validate_payload,
    validate_root,
    validate_sfx,
    validate_zip,
)

VERSION_RE = r"^[0-9]+\.[0-9]+\.[0-9]+([.-][A-Za-z0-9.-]+)?$"

TEXT_HELPERS = (
    "install-runtime.sh",
    "uninstall-runtime.sh",
    "backup-runtime.sh",
    "register-helpers.sh",
    "install-recovery.sh",
)

CUSTOMER_SCRIPTS = (
    "Uninstall-Ignite.sh",
    "Backup-Ignite-Data.sh",
    "Diagnose-Ignite.sh",
)


class PackageError(RuntimeError):
    pass


def die(msg: str) -> None:
    raise PackageError(msg)


def load_kv(path: Path) -> dict[str, str]:
    data: dict[str, str] = {}
    for raw in path.read_text(encoding="utf-8").splitlines():
        line = raw.strip()
        if not line or line.startswith("#") or "=" not in line:
            continue
        key, value = line.split("=", 1)
        data[key.strip()] = value.strip()
    return data


def sha256_file(path: Path) -> str:
    h = hashlib.sha256()
    with path.open("rb") as fh:
        for chunk in iter(lambda: fh.read(1024 * 1024), b""):
            h.update(chunk)
    return h.hexdigest()


def write_utf8_lf(path: Path, content: str) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    normalized = content.replace("\r\n", "\n").replace("\r", "\n")
    path.write_bytes(normalized.encode("utf-8"))


def copy_text_lf(src: Path, dst: Path) -> None:
    write_utf8_lf(dst, src.read_text(encoding="utf-8"))


def copy_binary(src: Path, dst: Path) -> None:
    dst.parent.mkdir(parents=True, exist_ok=True)
    shutil.copyfile(src, dst)


def read_version(repo: Path, override: str) -> str:
    import re

    vf = repo / "ignite" / "VERSION"
    if not vf.is_file():
        die(f"missing VERSION file at {vf}")
    file_version = vf.read_text(encoding="utf-8").strip()
    if not file_version:
        die("VERSION file is empty")
    version = override or file_version
    if override and override != file_version:
        print(
            f"warning: version override {override!r} differs from VERSION file "
            f"{file_version!r}; artifact name uses the override, binaries embed "
            "compile-time VERSION",
            file=sys.stderr,
        )
    if not re.match(VERSION_RE, version):
        die(f"invalid version {version!r}")
    return version


def require_release_elf(path: Path) -> None:
    if not path.is_file():
        die(f"missing release binary: {path}")
    if "debug" in path.parts:
        die(f"refusing to package debug binary: {path}")
    if path.stat().st_size < 1024:
        die(f"binary suspiciously small: {path}")


def enforce_dropbear_pin(elf: Path, pinned: Path) -> tuple[str, int]:
    kv = load_kv(pinned)
    want_sha = kv.get("elf_sha256", "")
    want_bytes_s = kv.get("elf_bytes", "")
    if len(want_sha) != 64 or any(c not in "0123456789abcdef" for c in want_sha):
        die(f"{pinned} elf_sha256 missing or malformed")
    try:
        want_bytes = int(want_bytes_s)
    except ValueError:
        die(f"{pinned} elf_bytes missing or malformed")
    have_sha = sha256_file(elf)
    have_bytes = elf.stat().st_size
    if have_sha != want_sha or have_bytes != want_bytes:
        die(
            "Dropbear ELF does not match tools/dropbear/PINNED.txt\n"
            f"  have sha256={have_sha} bytes={have_bytes}\n"
            f"  want sha256={want_sha} bytes={want_bytes}\n"
            "Do not update PINNED.txt to silence this. Investigate the toolchain "
            "image, upstream tarball, configure flags, and strip step."
        )
    return have_sha, have_bytes


def stage_payload(
    repo: Path,
    payload: Path,
    version: str,
    app_bin: Path,
    life_bin: Path,
    dropbear_dir: Path,
    pinned: Path,
) -> None:
    if payload.exists():
        shutil.rmtree(payload)
    payload.mkdir(parents=True)

    deploy = repo / "deployment" / "kindle"
    copy_binary(app_bin, payload / "bin" / "scribe-app")
    copy_binary(life_bin, payload / "bin" / "scribe-lifecycle")
    copy_text_lf(
        deploy / "scribe-lifecycle-host.sh",
        payload / "host" / "scribe-lifecycle-host.sh",
    )
    copy_text_lf(repo / "tools" / "kindle_launcher" / "Scribe.sh", payload / "launcher" / "Scribe.sh")
    copy_text_lf(
        repo / "tools" / "kindle_app" / "registration" / "ignite.json",
        payload / "registration" / "ignite.json",
    )
    copy_binary(
        repo / "tools" / "kindle_launcher" / "assets" / "scribe-icon.png",
        payload / "assets" / "scribe-icon.png",
    )
    copy_text_lf(
        repo / "ignite" / "licenses" / "noto-emoji" / "OFL.txt",
        payload / "assets" / "licenses" / "noto-emoji" / "OFL.txt",
    )
    copy_text_lf(
        repo / "ignite" / "assets" / "typography" / "OFL.txt",
        payload / "assets" / "licenses" / "source-sans-3" / "OFL.txt",
    )
    ofl_desc = repo / "ignite" / "licenses" / "noto-emoji" / "DESCRIPTION.en_us.html"
    if ofl_desc.is_file():
        copy_text_lf(ofl_desc, payload / "assets" / "licenses" / "noto-emoji" / "DESCRIPTION.en_us.html")

    dropbear_bin = dropbear_dir / "dropbearmulti"
    dropbear_sum = dropbear_dir / "elf.sha256"
    dropbear_prov = dropbear_dir / "provenance.txt"
    if not dropbear_bin.is_file():
        die(
            f"missing verified Dropbear client at {dropbear_bin} "
            "(run tools/dropbear/docker_build.sh)"
        )
    if not dropbear_sum.is_file():
        die(f"missing Dropbear digest at {dropbear_sum}")
    if not dropbear_prov.is_file():
        die(f"missing Dropbear provenance at {dropbear_prov}")

    pin_sha, _pin_bytes = enforce_dropbear_pin(dropbear_bin, pinned)
    copy_binary(dropbear_bin, payload / "recovery" / "bin" / "dropbearmulti")
    copy_text_lf(dropbear_sum, payload / "recovery" / "manifest" / "elf.sha256")
    copy_text_lf(dropbear_prov, payload / "recovery" / "manifest" / "provenance.txt")
    packaged = payload / "recovery" / "bin" / "dropbearmulti"
    sidecar = (payload / "recovery" / "manifest" / "elf.sha256").read_text(encoding="utf-8").split()[0].strip().lower()
    have = sha256_file(packaged)
    if have != pin_sha or sidecar != pin_sha:
        die(f"packaged Dropbear digest {have} != PINNED/manifest {pin_sha}")

    for helper in TEXT_HELPERS:
        src = deploy / "lib" / helper
        dst = payload / "lib" / helper
        if helper == "uninstall-runtime.sh":
            text = src.read_text(encoding="utf-8")
            if "@@IGNITE_PACKAGE_VERSION@@" not in text:
                die("uninstall-runtime.sh missing @@IGNITE_PACKAGE_VERSION@@ stamp")
            write_utf8_lf(dst, text.replace("@@IGNITE_PACKAGE_VERSION@@", version))
        else:
            copy_text_lf(src, dst)

    for name in CUSTOMER_SCRIPTS:
        copy_text_lf(deploy / name, payload / name)

    for rel in ("bin/scribe-app", "bin/scribe-lifecycle", "recovery/bin/dropbearmulti"):
        path = payload / rel
        path.chmod(path.stat().st_mode | stat.S_IXUSR | stat.S_IXGRP | stat.S_IXOTH)


def write_sha256sum(path: Path, digest: str, filename: str) -> None:
    write_utf8_lf(path, f"{digest}  {filename}\n")


def write_support_zip(repo: Path, support_dir: Path, zip_path: Path, sfx_path: Path, version: str) -> None:
    if support_dir.exists():
        shutil.rmtree(support_dir)
    support_dir.mkdir(parents=True)
    shutil.copyfile(sfx_path, support_dir / sfx_path.name)
    readme = (repo / "deployment" / "kindle" / "README.txt").read_text(encoding="utf-8")
    readme = readme.replace("Version: see the folder name of this package", f"Version: {version}")
    write_utf8_lf(support_dir / "README.txt", readme)

    errors = validate_root(support_dir, version)
    if errors:
        die("support-folder validation failed:\n" + "\n".join(errors))

    if zip_path.exists():
        zip_path.unlink()
    root_name = support_dir.name
    files = sorted(p for p in support_dir.rglob("*") if p.is_file())
    with zipfile.ZipFile(zip_path, "w", compression=zipfile.ZIP_DEFLATED) as zf:
        for path in files:
            rel = path.relative_to(support_dir).as_posix()
            zf.write(path, f"{root_name}/{rel}")

    errors = validate_zip(zip_path, version)
    if errors:
        die("support ZIP validation failed:\n" + "\n".join(errors))


def package(
    repo: Path,
    version: str,
    release_bin_dir: Path,
    dropbear_dir: Path,
    out_root: Path,
    pinned: Path,
    support_zip: bool,
) -> dict[str, str | int]:
    app_bin = release_bin_dir / "scribe-app"
    life_bin = release_bin_dir / "scribe-lifecycle"
    require_release_elf(app_bin)
    require_release_elf(life_bin)

    staging = out_root / "staging"
    package_name = f"Ignite-Kindle-{version}"
    payload = staging / package_name
    sfx_name = installer_sfx_name(version)
    sfx_path = out_root / sfx_name
    sfx_sha_path = out_root / f"Install Ignite v{version}.sha256"

    print(f"== Ignite Kindle package version={version} profile=full-feature")
    if staging.exists():
        shutil.rmtree(staging)
    print(f"== Stage payload {package_name}")
    stage_payload(repo, payload, version, app_bin, life_bin, dropbear_dir, pinned)

    errors = validate_payload(payload, version)
    if errors:
        die("payload validation failed:\n" + "\n".join(errors))

    print("== Generate release-component profiles (build evidence; not customer assets)")
    try:
        profiles = verify_deterministic(payload, version)
    except ProfileError as exc:
        die(f"component profile generation failed: {exc}")
    profile_dir = out_root / "component-profiles"
    write_profiles(profile_dir, profiles)
    result_profiles = {
        "component_profile_dir": str(profile_dir),
        "component_profile_package_sha256": sha256_file(
            profile_dir / "ignite-kindle-component-profile-package.v1"
        ),
    }

    print(f"== Build customer {sfx_name}")
    out_root.mkdir(parents=True, exist_ok=True)
    sfx_path.write_bytes(build_sfx(payload, version, default_template()))
    errors = validate_sfx(sfx_path, version)
    if errors:
        die("SFX validation failed:\n" + "\n".join(errors))

    sfx_hash = sha256_file(sfx_path)
    write_sha256sum(sfx_sha_path, sfx_hash, sfx_name)

    result: dict[str, str | int] = {
        "version": version,
        "sfx": str(sfx_path),
        "sfx_sha256": sfx_hash,
        "sfx_bytes": sfx_path.stat().st_size,
        "sfx_checksum": str(sfx_sha_path),
        **result_profiles,
    }

    if support_zip:
        support_dir = staging / f"Ignite Kindle {version}"
        zip_path = out_root / f"{package_name}.zip"
        zip_sha_path = out_root / f"{package_name}.sha256"
        print("== Stage developer/support ZIP (not a GitHub Release asset)")
        write_support_zip(repo, support_dir, zip_path, sfx_path, version)
        zip_hash = sha256_file(zip_path)
        write_sha256sum(zip_sha_path, zip_hash, f"{package_name}.zip")
        result["support_zip"] = str(zip_path)
        result["support_sha256"] = zip_hash
        result["support_bytes"] = zip_path.stat().st_size

    print("")
    print("======== Ignite Kindle release ========")
    print(f"version:              {version}")
    print("profile:              full-feature (sync-dir + device-dir enabled; optional to use)")
    print(f"CUSTOMER ARTIFACT:    {sfx_path}")
    print(f"customer_size_bytes:  {result['sfx_bytes']}")
    print(f"customer_sha256:      {sfx_hash}")
    print(f"customer_checksum:    {sfx_sha_path}")
    print(f"component_profiles:   {result['component_profile_dir']} (build evidence only)")
    print(f"package_profile_sha256: {result['component_profile_package_sha256']}")
    if support_zip:
        print(f"support ZIP (optional, not for end users): {result['support_zip']}")
        print(f"support_size_bytes:   {result['support_bytes']}")
        print(f"support_sha256:       {result['support_sha256']}")
    print("=======================================")
    return result


def parse_args(argv: list[str] | None = None) -> argparse.Namespace:
    repo_default = _HERE.parents[1]
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument("--repo-root", type=Path, default=repo_default)
    p.add_argument("--version", default="", help="artifact version override (default: ignite/VERSION)")
    p.add_argument(
        "--release-bin-dir",
        type=Path,
        default=None,
        help="directory containing scribe-app and scribe-lifecycle",
    )
    p.add_argument("--dropbear-dir", type=Path, default=None)
    p.add_argument("--out-root", type=Path, default=None)
    p.add_argument("--pinned-file", type=Path, default=None)
    p.add_argument("--no-support-zip", action="store_true", help="omit developer/support ZIP")
    return p.parse_args(argv)


def main(argv: list[str] | None = None) -> int:
    args = parse_args(argv)
    repo = args.repo_root.resolve()
    try:
        version = read_version(repo, args.version)
        release_bin_dir = (
            args.release_bin_dir.resolve()
            if args.release_bin_dir
            else repo / "target" / "armv7-unknown-linux-gnueabihf" / "release"
        )
        dropbear_dir = (
            args.dropbear_dir.resolve()
            if args.dropbear_dir
            else repo / "target" / "dropbear-kindle"
        )
        out_root = (
            args.out_root.resolve()
            if args.out_root
            else repo / "target" / "package" / "kindle"
        )
        pinned = (
            args.pinned_file.resolve()
            if args.pinned_file
            else repo / "tools" / "dropbear" / "PINNED.txt"
        )
        package(
            repo=repo,
            version=version,
            release_bin_dir=release_bin_dir,
            dropbear_dir=dropbear_dir,
            out_root=out_root,
            pinned=pinned,
            support_zip=not args.no_support_zip,
        )
    except PackageError as exc:
        print(f"package_release.py: {exc}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
