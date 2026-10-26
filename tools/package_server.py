#!/usr/bin/env python3
"""Package the central server with locked FRPS, its license, and startup guidance."""

from __future__ import annotations

import argparse
import json
from pathlib import Path
import shutil
import subprocess
import tarfile
import tempfile
import zipfile

from fetch_runtime import fetch, parse_manifest, sha256
from release_metadata import release_version


PLATFORMS = {"windows-x86_64": ".zip", "darwin-arm64": ".tar.gz", "linux-arm64": ".tar.gz"}


def frp_license(archive: Path) -> bytes:
    if zipfile.is_zipfile(archive):
        with zipfile.ZipFile(archive) as bundle:
            names = [entry.filename for entry in bundle.infolist()
                     if not entry.is_dir() and Path(entry.filename).name == "LICENSE"]
            if len(names) == 1:
                return bundle.read(names[0])
    else:
        with tarfile.open(archive) as bundle:
            members = [member for member in bundle.getmembers()
                       if member.isfile() and Path(member.name).name == "LICENSE"]
            if len(members) == 1:
                with bundle.extractfile(members[0]) as source:
                    return source.read()
    raise SystemExit("FRP 来源包缺少唯一的 LICENSE")


def package(binary: Path, manifest: Path, cache: Path, output: Path, verify_executables: bool = False) -> Path:
    artifacts = [record for record in parse_manifest(manifest) if record.get("name") == "frp"]
    if len(artifacts) != 1 or artifacts[0].get("platform") not in PLATFORMS:
        raise SystemExit("依赖清单必须包含唯一的受支持平台 FRP 记录")
    artifact = artifacts[0]
    platform = artifact["platform"]
    suffix = ".exe" if platform == "windows-x86_64" else ""
    archive_suffix = PLATFORMS[platform]
    destination = output / f"StarBridge-server-{release_version()}-{platform}{archive_suffix}"
    if not binary.is_file():
        raise SystemExit(f"缺少服务端二进制: {binary}")
    if destination.exists() or destination.is_symlink():
        raise SystemExit(f"发布包已存在，请使用新的输出目录: {destination}")

    # Use a new staging directory; never copy an existing deployment's state/configuration.
    output.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix=".starbridge-server-", dir=output) as temporary:
        staging = Path(temporary)
        runtime = staging / "runtime-input"
        record = fetch(artifact, cache, runtime)
        name = f"frps{suffix}"
        frps = runtime / name
        if not frps.is_file() or sha256(frps) != record["file_sha256"].get(name):
            raise SystemExit("FRPS 运行时缺失或文件校验失败")
        license_bytes = frp_license(cache / artifact["url"].rsplit("/", 1)[-1])
        if verify_executables:
            for executable in (binary, frps):
                subprocess.run([str(executable.resolve()), "--version"], check=True, timeout=30)

        root = staging / "StarBridge-server"
        (root / "runtime").mkdir(parents=True)
        (root / "licenses/frp").mkdir(parents=True)
        executable = root / f"starbridge-server{suffix}"
        shutil.copy2(binary, executable)
        shutil.copy2(frps, root / "runtime" / name)
        if not suffix:
            executable.chmod(0o755)
            (root / "runtime" / name).chmod(0o755)
        record = {key: record[key] for key in ("name", "version", "url", "sha256")}
        record.update(platform=platform, files=[name], file_sha256={name: sha256(root / "runtime" / name)})
        (root / "runtime/runtime-manifest.json").write_text(
            json.dumps([record], ensure_ascii=False, indent=2) + "\n", encoding="utf-8", newline="\n")
        (root / "licenses/frp/LICENSE").write_bytes(license_bytes)
        (root / "README.txt").write_text(
            f"StarBridge 中心服务 / Central Server {release_version()} ({platform})\n\n"
            "在解压目录打开终端，替换下面的公共地址和令牌后启动：\n"
            "Open a terminal in the extracted directory; replace the public address and token:\n\n"
            f"./starbridge-server{suffix} --listen 127.0.0.1:17850 "
            f"--frps-path ./runtime/{name} --frps-bind-addr 0.0.0.0 "
            "--frps-addr YOUR_PUBLIC_HOST --frps-port 17851 "
            "--frps-token REPLACE_WITH_A_RANDOM_SECRET --data-dir ./state/server\n\n"
            "Windows PowerShell 使用 .\\starbridge-server.exe；其他参数保持相同。\n"
            "On Windows PowerShell use .\\starbridge-server.exe with the same arguments.\n"
            "17850 默认只监听本机；请配置 HTTPS 反向代理，并放行 FRPS 的 17851 TCP 端口。\n"
            "Port 17850 listens locally; configure an HTTPS reverse proxy and allow FRPS TCP port 17851.\n"
            "请为中心设置独立随机 FRP 令牌；生产环境需自行配置进程守护和证书。\n"
            "Set a random FRP token. Configure your process supervisor and TLS certificates.\n"
            "本包不包含账户、证书、私钥、生产配置或 OpenSSH。请保留 licenses 和 runtime 目录。\n"
            "No accounts, certificates, private keys, production settings, or OpenSSH are included.\n",
            encoding="utf-8", newline="\n")
        archive_base = staging / "package"
        if suffix:
            archive = Path(shutil.make_archive(str(archive_base), "zip", staging, root.name))
        else:
            archive = archive_base.with_suffix(".tar.gz")

            def archive_permissions(member: tarfile.TarInfo) -> tarfile.TarInfo:
                executable_names = {f"{root.name}/starbridge-server", f"{root.name}/runtime/frps"}
                member.mode = 0o755 if member.isdir() or member.name in executable_names else 0o644
                return member

            with tarfile.open(archive, "w:gz") as bundle:
                bundle.add(root, arcname=root.name, filter=archive_permissions)
        archive.replace(destination)
    return destination


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, required=True)
    parser.add_argument("--manifest", type=Path, required=True)
    parser.add_argument("--cache", type=Path, default=Path(".cache/runtime"))
    parser.add_argument("--output", type=Path, default=Path("dist/release"))
    parser.add_argument("--verify-executables", action="store_true", help="在本机构建时执行服务端和 FRPS --version 检查")
    args = parser.parse_args()
    print(package(args.binary, args.manifest, args.cache, args.output, args.verify_executables))


if __name__ == "__main__":
    main()
