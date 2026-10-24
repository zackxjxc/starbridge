#!/usr/bin/env python3
"""Fetch locked FRP/OpenSSH artifacts for a local build.

The example manifest intentionally contains placeholders. It fails closed until a
developer fills in an exact release URL and SHA-256 value.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import shutil
import subprocess
import sys
import tempfile
import tarfile
import urllib.request
from urllib.parse import urlparse
import zipfile
from pathlib import Path


def parse_manifest(path: Path) -> list[dict[str, str]]:
    try:
        import tomllib
    except ModuleNotFoundError:  # pragma: no cover
        try:
            import tomli as tomllib
        except ModuleNotFoundError as exc:
            raise SystemExit("需要 Python 3.11+，或在旧版本中安装 tomli") from exc
    data = tomllib.loads(path.read_text(encoding="utf-8"))
    artifacts = data.get("artifact", [])
    if not isinstance(artifacts, list):
        raise SystemExit("vendor/manifest.toml 中的 artifact 必须是数组")
    return artifacts


def validate(artifact: dict[str, str]) -> None:
    required = ("name", "version", "platform", "url", "sha256")
    missing = [key for key in required if not artifact.get(key)]
    if missing:
        raise SystemExit(f"依赖清单缺少字段: {', '.join(missing)}")
    if artifact["version"].startswith(("vX.", "vA.")) or artifact["version"] == "latest":
        raise SystemExit("依赖清单仍是示例版本，请先填入已审核版本")
    if artifact["sha256"] == "..." or len(artifact["sha256"]) != 64:
        raise SystemExit(f"{artifact['name']} 的 SHA-256 不是完整值")
    if not artifact["url"].startswith("https://"):
        raise SystemExit(f"{artifact['name']} 必须使用 HTTPS 下载地址")


def sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as source:
        for chunk in iter(lambda: source.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


# 执行端内置 sshd 需要的程序；macOS 上没有官方预编译包，只能从源码构建。
OPENSSH_TARGETS = ("sshd", "sshd-session", "sshd-auth", "ssh-keygen", "sftp-server")


def build_openssh(unpacked: Path) -> None:
    sources = [path.parent for path in unpacked.rglob("configure")]
    if len(sources) != 1:
        raise SystemExit("OpenSSH 源码包中没有唯一的 configure")
    source = sources[0]
    # StarBridge 只用 ed25519 密钥，内置加密实现足够，不依赖系统没有提供头文件的 OpenSSL。
    steps = (
        ["./configure", "--without-openssl", "--prefix=/opt/starbridge-openssh"],
        ["make", f"-j{os.cpu_count() or 2}", *OPENSSH_TARGETS],
    )
    for step in steps:
        print(f"构建 OpenSSH: {' '.join(step)}")
        result = subprocess.run(step, cwd=source, capture_output=True, text=True)
        if result.returncode != 0:
            raise SystemExit(f"OpenSSH 构建失败（{step[0]}）：{(result.stdout + result.stderr)[-2000:]}")
    for name in OPENSSH_TARGETS:
        built = source / name
        if not built.is_file():
            raise SystemExit(f"OpenSSH 构建后缺少 {name}")
    # 只保留构建产物，避免源码树里的同名文件被复制进运行时目录。
    for candidate in list(unpacked.rglob("*")):
        if candidate.is_file() and candidate.name in OPENSSH_TARGETS and candidate.parent != source:
            candidate.unlink()


def fetch(artifact: dict[str, str], cache: Path, output: Path) -> dict[str, str]:
    validate(artifact)
    cache.mkdir(parents=True, exist_ok=True)
    archive = cache / Path(urlparse(artifact["url"]).path).name
    if not archive.exists():
        print(f"下载 {artifact['name']} -> {archive}")
        urllib.request.urlretrieve(artifact["url"], archive)
    actual = sha256(archive)
    if actual.lower() != artifact["sha256"].lower():
        raise SystemExit(f"{artifact['name']} SHA-256 校验失败: {actual}")
    with tempfile.TemporaryDirectory(prefix="starbridge-runtime-") as directory:
        unpacked = Path(directory)
        if zipfile.is_zipfile(archive):
            with zipfile.ZipFile(archive) as bundle:
                for member in bundle.infolist():
                    destination = (unpacked / member.filename).resolve()
                    if not destination.is_relative_to(unpacked.resolve()):
                        raise SystemExit(f"压缩包路径越界: {member.filename}")
                bundle.extractall(unpacked)
        elif tarfile.is_tarfile(archive):
            with tarfile.open(archive) as bundle:
                for member in bundle.getmembers():
                    destination = (unpacked / member.name).resolve()
                    if not destination.is_relative_to(unpacked.resolve()):
                        raise SystemExit(f"归档路径越界: {member.name}")
                bundle.extractall(unpacked, filter="data")
        else:
            raise SystemExit(f"不支持的运行时归档格式: {archive.name}")
        if artifact.get("build") == "openssh-portable":
            build_openssh(unpacked)
        output.mkdir(parents=True, exist_ok=True)
        allowed = {
            "frpc.exe",
            "frps.exe",
            "frpc",
            "frps",
            "ssh.exe",
            "sftp.exe",
            "scp.exe",
            "ssh-keygen.exe",
            "sshd.exe",
            "sshd-session.exe",
            "sshd-auth.exe",
            "sftp-server.exe",
            "ssh-shellhost.exe",
            "libcrypto.dll",
            "ssh",
            "sftp",
            "scp",
            "ssh-keygen",
            "sshd",
            "sshd-session",
            "sshd-auth",
            "sftp-server",
        }
        copied: list[str] = []
        for candidate in unpacked.rglob("*"):
            if candidate.is_file() and candidate.name in allowed:
                destination = output / candidate.name
                # 先删除旧文件，避免目标是软链接时写穿到链接指向的位置。
                destination.unlink(missing_ok=True)
                shutil.copy2(candidate, destination)
                copied.append(candidate.name)
        if not copied:
            raise SystemExit(f"{artifact['name']} 压缩包中没有允许的运行时文件")
    return {"name": artifact["name"], "version": artifact["version"], "url": artifact["url"], "sha256": actual, "files": copied}


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--manifest", type=Path, default=Path("vendor/manifest.toml"))
    parser.add_argument("--cache", type=Path, default=Path(".cache/runtime"))
    parser.add_argument("--output", type=Path, default=Path("runtime"))
    args = parser.parse_args()
    records = [fetch(artifact, args.cache, args.output) for artifact in parse_manifest(args.manifest)]
    (args.output / "runtime-manifest.json").write_text(json.dumps(records, ensure_ascii=False, indent=2) + "\n", encoding="utf-8")
    return 0


if __name__ == "__main__":
    sys.exit(main())
