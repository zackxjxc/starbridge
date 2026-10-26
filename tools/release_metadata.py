#!/usr/bin/env python3
"""Validate release versions and the complete five-package release set."""

from __future__ import annotations

import argparse
import os
from pathlib import Path
import re
import tomllib

from fetch_runtime import sha256


REPO = Path(__file__).resolve().parents[1]
PACKAGES = (
    ("client", "windows-x86_64", ".zip"),
    ("client", "darwin-arm64", ".zip"),
    ("server", "windows-x86_64", ".zip"),
    ("server", "darwin-arm64", ".tar.gz"),
    ("server", "linux-arm64", ".tar.gz"),
)


def release_version(tag: str = "") -> str:
    version = tomllib.loads((REPO / "Cargo.toml").read_text(encoding="utf-8"))["workspace"]["package"]["version"]
    if not re.fullmatch(r"\d+\.\d+\.\d+", version):
        raise SystemExit("发布版本必须是 Cargo.toml 中的三段数字版本")
    if tag and tag not in (version, f"v{version}"):
        raise SystemExit(f"tag {tag!r} 与 Cargo.toml 版本 {version} 不一致；请使用 {version} 或 v{version}")
    return version


def asset_names(version: str) -> list[str]:
    return [f"StarBridge-{kind}-{version}-{platform}{suffix}" for kind, platform, suffix in PACKAGES]


def checksums(directory: Path, version: str) -> Path:
    expected = set(asset_names(version))
    actual = {path.name for path in directory.iterdir() if path.name != "SHA256SUMS.txt"}
    if actual != expected or any(not (directory / name).is_file() for name in expected):
        raise SystemExit(f"发布物不完整：缺少 {sorted(expected - actual)}；额外文件 {sorted(actual - expected)}")
    lines = []
    for name in sorted(expected):
        path = directory / name
        if path.stat().st_size == 0:
            raise SystemExit(f"发布包为空: {name}")
        lines.append(f"{sha256(path)}  {name}\n")
    output = directory / "SHA256SUMS.txt"
    output.write_text("".join(lines), encoding="utf-8", newline="\n")
    return output


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("command", choices=("version", "checksums"))
    parser.add_argument("--directory", type=Path, default=Path("dist/release"))
    args = parser.parse_args()
    version = release_version(os.environ.get("RELEASE_TAG", ""))
    if args.command == "version":
        print(version)
        if os.environ.get("GITHUB_OUTPUT"):
            with open(os.environ["GITHUB_OUTPUT"], "a", encoding="utf-8", newline="\n") as output:
                output.write(f"version={version}\n")
    else:
        print(checksums(args.directory, version))


if __name__ == "__main__":
    main()
