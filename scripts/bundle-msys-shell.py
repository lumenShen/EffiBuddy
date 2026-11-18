#!/usr/bin/env python3
"""Assemble the portable MSYS2 bash runtime shipped with Windows builds.

Run at release build time (see `.github/workflows/release.yml`). The script
downloads the live MSYS2 repository database, resolves the dependency closure
for bash plus the text/file utilities the agent needs, verifies every package
against the checksum stored in the database, and extracts a pruned tree into
the destination directory. Nothing is pinned: each release picks up whatever
MSYS2 currently publishes.

The resulting tree is distributed as `runtime/msys/` next to `effibuddy.exe`.
`ShellTool` prefers `runtime/msys/usr/bin/bash.exe` and falls back to `cmd /C`
when the tree is missing (developer builds).
"""

from __future__ import annotations

import hashlib
import io
import os
import re
import shutil
import subprocess
import sys
import tarfile
import time
import urllib.error
import urllib.request
from pathlib import Path

# Canonical repository (no mirror redirect: `mirror.msys2.org` bounces to
# third-party mirrors whose TLS chains vary by geography and can be stale).
# The alternates are tried only after the canonical host refuses (GitHub runners
# do get 403s from it when its CDN throttles a shared IP range).
MIRRORS = [
    "https://repo.msys2.org/msys/x86_64",
    "https://mirror.msys2.org/msys/x86_64",
]
MIRROR = MIRRORS[0]
DB_NAME = "msys.db.tar.zst"
# A 403/429/5xx from the CDN is usually momentary backpressure, not a missing
# file, so each mirror gets a few attempts with exponential backoff before the
# script gives up.
DOWNLOAD_ATTEMPTS = 4

# bash 的 DEPENDS 元数据为空（msys2-runtime 被视作基础包），所以显式列出它。
ROOT_PACKAGES = [
    "bash",
    "coreutils",
    "diffutils",
    "findutils",
    "gawk",
    "grep",
    "gzip",
    "msys2-runtime",
    "sed",
    "tar",
    "which",
]

# 非交互 shell 用不到文档、翻译和 pacman 元数据；删掉能省不少安装体积。
PRUNE_DIRS = [
    "var",
    "usr/share/doc",
    "usr/share/info",
    "usr/share/locale",
    "usr/share/man",
]
PRUNE_FILES = [".BUILDINFO", ".MTREE", ".PKGINFO"]

VERSION_SPEC = re.compile(r"[<>=].*$")
# 包名来自数据库，下载前只接受 MSYS2 的合法文件名形状。
PACKAGE_NAME = re.compile(r"[A-Za-z0-9._+~-]+\.pkg\.tar\.zst")

# MSYS2 包只给 gcc-libs/ncurses 带了许可证文件；bash/coreutils 等 GNU 组件
# 统一附一份 GPLv3 文本，满足随二进制分发许可证的要求。
GPL_URL = "https://www.gnu.org/licenses/gpl-3.0.txt"


def download(url: str) -> bytes:
    request = urllib.request.Request(
        url, headers={"User-Agent": "effibuddy-release-builder"}
    )
    with urllib.request.urlopen(request, timeout=180) as response:
        return response.read()


def download_package_rooted(path: str) -> bytes:
    """Fetch `path` from the MSYS2 package repository.

    GitHub runners regularly get `403 Forbidden` from the CDN that fronts
    `repo.msys2.org`; a plain one-shot GET then fails the whole Windows job (and
    with it the release, because publishing needs every platform). Retry with
    backoff, then fall back to the mirror redirect before giving up.
    """
    errors: list[str] = []
    for base in MIRRORS:
        for attempt in range(DOWNLOAD_ATTEMPTS):
            if attempt:
                time.sleep(min(2**attempt, 20))
            url = f"{base}/{path}"
            try:
                return download(url)
            except urllib.error.HTTPError as error:
                errors.append(f"{url} -> {error.code} {error.reason}")
                # A 404 is a real answer: that mirror does not carry the file.
                if error.code == 404:
                    break
            except OSError as error:  # timeouts, resets, DNS hiccups
                errors.append(f"{url} -> {error}")
    raise SystemExit("cannot download from the MSYS2 repository:\n" + "\n".join(errors))


def zstd_decompress(data: bytes) -> bytes:
    # Windows runners ship zstd.exe; using the CLI keeps this script on the
    # standard library (compression.zstd only exists in Python 3.14+).
    process = subprocess.run(
        ["zstd", "-d", "-c"], input=data, capture_output=True, check=False
    )
    if process.returncode != 0:
        raise SystemExit(
            "zstd failed (is zstd.exe on PATH?): "
            + process.stderr.decode("utf-8", "replace")
        )
    return process.stdout


def parse_desc(text: str) -> dict[str, list[str]]:
    fields: dict[str, list[str]] = {}
    key: str | None = None
    for line in text.splitlines():
        if len(line) > 2 and line.startswith("%") and line.endswith("%"):
            key = line[1:-1]
            fields[key] = []
        elif line.strip() and key is not None:
            fields[key].append(line.strip())
    return fields


def first(fields: dict[str, list[str]], key: str) -> str:
    values = fields.get(key) or []
    return values[0] if values else ""


def load_database() -> dict[str, dict[str, object]]:
    data = download_package_rooted(DB_NAME)
    packages: dict[str, dict[str, object]] = {}
    with tarfile.open(fileobj=io.BytesIO(zstd_decompress(data))) as archive:
        for member in archive.getmembers():
            if not member.isfile() or not member.name.endswith("/desc"):
                continue
            handle = archive.extractfile(member)
            if handle is None:
                continue
            fields = parse_desc(handle.read().decode("utf-8", "replace"))
            name = first(fields, "NAME")
            filename = first(fields, "FILENAME")
            if not name or not filename:
                continue
            if not PACKAGE_NAME.fullmatch(filename):
                raise SystemExit(f"unexpected package filename in database: {filename!r}")
            packages[name] = {
                "filename": filename,
                "version": first(fields, "VERSION"),
                "sha256": first(fields, "SHA256SUM"),
                "depends": fields.get("DEPENDS", []),
                "provides": fields.get("PROVIDES", []),
            }
    if not packages:
        raise SystemExit("MSYS2 database is empty")
    return packages


def dependency_name(spec: str) -> str:
    return VERSION_SPEC.sub("", spec).strip()


def resolve(packages: dict[str, dict[str, object]]) -> dict[str, dict[str, object]]:
    providers: dict[str, str] = {}
    for name in sorted(packages):
        providers.setdefault(name, name)
        provides = packages[name].get("provides") or []
        for provide in provides:
            provider_name = dependency_name(provide)
            if provider_name:
                providers.setdefault(provider_name, name)

    chosen: dict[str, dict[str, object]] = {}
    missing: set[str] = set()

    def visit(name: str) -> None:
        if name in chosen:
            return
        meta = packages.get(name)
        if meta is None:
            provider = providers.get(name)
            if provider is None or provider == name:
                missing.add(name)
                return
            visit(provider)
            return
        chosen[name] = meta
        depends = meta.get("depends") or []
        for dependency in depends:
            resolved = dependency_name(dependency)
            if resolved:
                visit(resolved)

    for root in ROOT_PACKAGES:
        visit(root)
    if missing:
        raise SystemExit(
            "unresolved MSYS2 dependencies: " + ", ".join(sorted(missing))
        )
    return chosen


def fetch_package(meta: dict[str, object]) -> bytes:
    filename = str(meta["filename"])
    data = download_package_rooted(filename)
    expected = str(meta.get("sha256") or "")
    digest = hashlib.sha256(data).hexdigest()
    if expected and digest != expected:
        raise SystemExit(
            f"checksum mismatch for {filename}: {digest} != {expected}"
        )
    return data


def extract_package(data: bytes, destination: Path) -> None:
    with tarfile.open(fileobj=io.BytesIO(zstd_decompress(data))) as archive:
        # `data` rejects absolute paths and links escaping the destination.
        if hasattr(tarfile, "data_filter"):
            archive.extractall(destination, filter="data")
        else:  # Python < 3.12
            archive.extractall(destination)


def prune(destination: Path) -> None:
    for relative in PRUNE_DIRS:
        path = destination / relative
        if path.is_dir():
            shutil.rmtree(path)
    for name in PRUNE_FILES:
        for path in destination.rglob(name):
            path.unlink(missing_ok=True)
    # 非交互命令也会用到临时目录（tar、sort 等）；先建好，避免依赖
    # Cygwin 对 /tmp 的默认映射。
    (destination / "tmp").mkdir(exist_ok=True)
    (destination / "var" / "tmp").mkdir(parents=True, exist_ok=True)


def write_build_info(destination: Path, chosen: dict[str, dict[str, object]]) -> None:
    lines = [
        "MSYS2 bash runtime bundled with EffiBuddy at release build time.",
        f"repository: {MIRROR}/",
        "packages:",
    ]
    for name in sorted(chosen):
        meta = chosen[name]
        lines.append(f"  {name} {meta['version']} ({meta['filename']})")
    (destination / "BASH-VERSION.txt").write_text(
        "\n".join(lines) + "\n", encoding="utf-8"
    )


def smoke_test(destination: Path) -> None:
    if sys.platform != "win32":
        print("skipping bash smoke test on non-Windows host")
        return
    bash = destination / "usr" / "bin" / "bash.exe"
    entries = [
        str(destination / "usr" / "bin"),
        str(destination / "usr" / "local" / "bin"),
    ]
    if os.environ.get("PATH"):
        entries.append(os.environ["PATH"])
    env = os.environ.copy()
    env["PATH"] = os.pathsep.join(entries)
    process = subprocess.run(
        [
            str(bash),
            "--noprofile",
            "--norc",
            "-c",
            "echo bundled-bash-ok; ls --version | head -1; "
            "grep --version | head -1; sed --version | head -1",
        ],
        env=env,
        capture_output=True,
        text=True,
        check=False,
    )
    output = (process.stdout or "") + (process.stderr or "")
    if process.returncode != 0:
        raise SystemExit(f"bundled bash smoke test failed:\n{output}")
    print(output.strip())


def main(argv: list[str]) -> int:
    if len(argv) != 2:
        print("usage: bundle-msys-shell.py <destination-dir>", file=sys.stderr)
        return 2
    destination = Path(argv[1]).resolve()
    destination.mkdir(parents=True, exist_ok=True)

    chosen = resolve(load_database())
    for name in sorted(chosen):
        meta = chosen[name]
        print(f"  {name} {meta['version']}")
        extract_package(fetch_package(meta), destination)

    prune(destination)
    (destination / "BASH-COPYING").write_bytes(download(GPL_URL))
    write_build_info(destination, chosen)
    smoke_test(destination)
    print(f"MSYS2 bash runtime ready at {destination}")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
