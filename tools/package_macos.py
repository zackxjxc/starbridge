#!/usr/bin/env python3
"""Package the Apple Silicon client and verified runtime as a signed local .app."""

from __future__ import annotations

import argparse
import copy
import json
from pathlib import Path
import plistlib
import shutil
import subprocess
import sys
import tarfile
import tempfile

from fetch_runtime import parse_manifest, sha256


REPO = Path(__file__).resolve().parents[1]
CLIENT_RUNTIME = {
    "frp": ("frpc",),
    "openssh": ("sshd", "sshd-session", "sshd-auth", "ssh-keygen", "sftp-server"),
}
LICENSES = {"frp": ("LICENSE",), "openssh": ("LICENCE",)}


def package(binary: Path, runtime: Path, cache: Path, output: Path) -> Path:
    if output.suffix != ".app" or output.exists() or output.is_symlink():
        raise SystemExit("输出必须是尚不存在的 .app 路径；请使用新的 --output，保留已有应用和数据")
    if not binary.is_file():
        raise SystemExit(f"缺少客户端: {binary}，请先执行 cargo build --release -p starbridge-client")
    manifest = json.loads((runtime / "runtime-manifest.json").read_text(encoding="utf-8"))
    locked = parse_manifest(REPO / "vendor/manifest.darwin-arm64.toml")
    records = []
    licenses = {}
    # Validate all inputs before creating a bundle. Do not package server binaries or user state.
    for artifact in locked:
        name = artifact["name"]
        matches = [record for record in manifest if record.get("name") == name]
        if len(matches) != 1:
            raise SystemExit(f"运行时清单必须包含唯一的 {name} 记录")
        record = copy.deepcopy(matches[0])
        if any(record.get(key) != artifact[key] for key in ("version", "url", "sha256")):
            raise SystemExit(f"{name} 运行时与 macOS 锁定清单不一致")
        archive = cache / artifact["url"].rsplit("/", 1)[-1]
        if not archive.is_file() or sha256(archive) != artifact["sha256"]:
            raise SystemExit(f"来源包缺失或 SHA-256 校验失败: {archive}")
        for filename in CLIENT_RUNTIME[name]:
            source = runtime / filename
            expected = record.get("file_sha256", {}).get(filename)
            if not source.is_file() or not expected or sha256(source) != expected:
                raise SystemExit(f"运行时文件校验失败: {filename}，请重新执行 tools/fetch_runtime.py")
        with tarfile.open(archive) as bundle:
            licenses[name] = {}
            for filename in LICENSES[name]:
                members = [member for member in bundle.getmembers()
                           if member.isfile() and Path(member.name).name == filename]
                if len(members) != 1:
                    raise SystemExit(f"{name} 来源包缺少唯一的许可证 {filename}")
                with bundle.extractfile(members[0]) as source:
                    licenses[name][filename] = source.read()
        record["files"] = list(CLIENT_RUNTIME[name])
        record["file_sha256"] = {filename: record["file_sha256"][filename]
                                 for filename in record["files"]}
        records.append(record)

    for source in [binary, *(runtime / filename for names in CLIENT_RUNTIME.values() for filename in names)]:
        subprocess.run(["lipo", str(source), "-verify_arch", "arm64"], check=True, capture_output=True)
    config = json.loads((REPO / "client/tauri.conf.json").read_text(encoding="utf-8"))
    output.parent.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix=".starbridge-package-", dir=output.parent) as temporary:
        app = Path(temporary) / "StarBridge.app"
        contents = app / "Contents"
        resources = contents / "Resources"
        bundled_runtime = resources / "runtime"
        bundled_runtime.mkdir(parents=True)
        executable = contents / "MacOS" / "starbridge"
        executable.parent.mkdir()
        shutil.copy2(binary, executable)
        executable.chmod(0o755)
        shutil.copy2(REPO / "client/icons/icon.icns", resources / "icon.icns")
        for record in records:
            for filename in record["files"]:
                destination = bundled_runtime / filename
                shutil.copy2(runtime / filename, destination)
                destination.chmod(0o755)
                subprocess.run(["codesign", "--force", "--sign", "-", str(destination)],
                               check=True, capture_output=True)
            # Signing changes Mach-O bytes; record hashes of the distributed files.
            record["file_sha256"] = {filename: sha256(bundled_runtime / filename)
                                     for filename in record["files"]}
        manifest_bytes = (json.dumps(records, ensure_ascii=False, indent=2) + "\n").encode("utf-8")
        (bundled_runtime / "runtime-manifest.json").write_bytes(manifest_bytes)
        (resources / "runtime-manifest.json").write_bytes(manifest_bytes)
        for component, files in licenses.items():
            destination = resources / "licenses" / component
            destination.mkdir(parents=True)
            for filename, data in files.items():
                (destination / filename).write_bytes(data)
        (contents / "Info.plist").write_bytes(plistlib.dumps({
            "CFBundleName": config["productName"],
            "CFBundleDisplayName": config["productName"],
            "CFBundleIdentifier": config["identifier"],
            "CFBundleExecutable": "starbridge",
            "CFBundleIconFile": "icon.icns",
            "CFBundlePackageType": "APPL",
            "CFBundleShortVersionString": config["version"],
            "CFBundleVersion": config["version"],
            "LSMinimumSystemVersion": "12.0",
            "NSHighResolutionCapable": True,
        }))
        subprocess.run(["codesign", "--force", "--sign", "-", str(app)], check=True, capture_output=True)
        subprocess.run(["codesign", "--verify", "--deep", "--strict", str(app)],
                       check=True, capture_output=True)
        archive = Path(temporary) / "StarBridge.zip"
        # ditto preserves executable permissions and the app bundle layout.
        subprocess.run(["ditto", "-c", "-k", "--sequesterRsrc", "--keepParent", str(app), str(archive)],
                       check=True, capture_output=True)
        app.rename(output)
        archive.replace(output.with_suffix(".zip"))
    return output.with_suffix(".zip")


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, default=REPO / "target/release/starbridge-client")
    parser.add_argument("--runtime", type=Path, default=REPO / "state/runtime")
    parser.add_argument("--cache", type=Path, default=REPO / ".cache/runtime")
    parser.add_argument("--output", type=Path, default=REPO / "dist/StarBridge.app")
    args = parser.parse_args()
    if sys.platform != "darwin":
        parser.error("macOS 打包需要在配备 Xcode 命令行工具的 Mac 上运行")
    try:
        archive = package(args.binary.resolve(), args.runtime.resolve(), args.cache.resolve(),
                          args.output.absolute())
    except subprocess.CalledProcessError as error:
        raise SystemExit(f"打包命令失败: {error.cmd}\n{error.stderr.decode(errors='replace')}") from error
    print(args.output)
    print(archive)
    return 0


if __name__ == "__main__":
    sys.exit(main())
