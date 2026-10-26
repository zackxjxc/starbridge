#!/usr/bin/env python3
"""Assemble a portable Tauri client directory and ZIP, without an installer.

Run after `cargo build --release -p starbridge-client` and
`python tools/fetch_runtime.py --output state/runtime`.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import shutil
import sys
import zipfile
from pathlib import Path

# 端侧只带 frpc 和 OpenSSH；frps 只属于中心部署包。
CLIENT_RUNTIME = (
    "frpc.exe",
    "ssh.exe",
    "scp.exe",
    "sftp.exe",
    "ssh-keygen.exe",
    "sshd.exe",
    "sshd-session.exe",
    "sshd-auth.exe",
    "sftp-server.exe",
    "ssh-shellhost.exe",
    "libcrypto.dll",
)

LICENSES = {
    "frp": ("frp_0.71.1_windows_amd64.zip", ("LICENSE",)),
    "openssh": ("OpenSSH-Win64.zip", ("LICENSE.txt", "NOTICE.txt")),
}

PRIVATE_DATA_NAMES = {
    "state", "controller-data", "executor-data", "ssh", "config.json", "identity.json",
    "identity.secure", "auth.json", "auth.secure", "machine.json",
    "instance-scope.json", "migrated-to.json", "legacy-migration.json",
}


def contains_client_data(directory: Path) -> bool:
    return directory.exists() and any(
        path.name.lower() in PRIVATE_DATA_NAMES or path.suffix.lower() == ".secure"
        for path in directory.rglob("*")
    )


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, default=Path("target/release/starbridge-client.exe"))
    parser.add_argument("--runtime", type=Path, default=Path("state/runtime"))
    parser.add_argument("--cache", type=Path, default=Path(".cache/runtime"))
    parser.add_argument("--output", type=Path, default=Path("dist/StarBridge-test"))
    args = parser.parse_args()

    if not args.binary.is_file():
        raise SystemExit(f"缺少 {args.binary}，请先运行 cargo build --release -p starbridge-client")
    missing = [name for name in CLIENT_RUNTIME if not (args.runtime / name).is_file()]
    if missing:
        raise SystemExit(f"运行时缺少文件: {', '.join(missing)}，请先运行 tools/fetch_runtime.py")

    manifest_path = args.runtime / "runtime-manifest.json"
    if not manifest_path.is_file():
        raise SystemExit(f"缺少运行时清单: {manifest_path}")
    manifest = json.loads(manifest_path.read_text(encoding="utf-8"))
    expected_files = {
        name: digest for record in manifest
        for name, digest in record.get("file_sha256", {}).items()
        if name in CLIENT_RUNTIME
    }
    for name in CLIENT_RUNTIME:
        if name not in expected_files:
            raise SystemExit("运行时缺少文件校验信息，请重新运行 tools/fetch_runtime.py")
        actual = hashlib.sha256((args.runtime / name).read_bytes()).hexdigest()
        if actual != expected_files[name]:
            raise SystemExit(f"运行时文件校验失败: {name}，请重新准备官方运行时")
    for archive, _ in LICENSES.values():
        if not (args.cache / archive).is_file():
            raise SystemExit(f"缺少许可证来源包: {args.cache / archive}")

    output = args.output.resolve()
    sources = (
        args.binary.resolve(), args.runtime.resolve(), args.cache.resolve(),
        Path.cwd(), Path(__file__).resolve().parent.parent,
    )
    if (output == output.parent or args.output.is_symlink()
            or any(source == output or output in source.parents for source in sources)):
        raise SystemExit("输出目录不能包含项目或打包输入文件，也不能是符号链接")
    if contains_client_data(args.output):
        raise SystemExit("输出目录包含客户端数据，请使用新的 --output 目录，避免删除数据或把登录状态打进 ZIP")

    if args.output.exists():
        shutil.rmtree(args.output)
    runtime = args.output / "runtime"
    runtime.mkdir(parents=True)
    shutil.copy2(args.binary, args.output / "starbridge.exe")
    for name in CLIENT_RUNTIME:
        shutil.copy2(args.runtime / name, runtime / name)
    # Publish only runtime metadata, never unrelated fields from a used directory.
    manifest = [
        {**{key: record[key] for key in ("name", "component", "version", "url", "sha256") if key in record},
         "files": [name for name in record["files"] if name in CLIENT_RUNTIME],
         "file_sha256": {name: expected_files[name] for name in record["files"] if name in CLIENT_RUNTIME}}
        for record in manifest if any(name in CLIENT_RUNTIME for name in record["files"])
    ]
    (runtime / "runtime-manifest.json").write_text(
        json.dumps(manifest, ensure_ascii=False, indent=2) + "\n", encoding="utf-8"
    )
    for component, (archive, names) in LICENSES.items():
        destination = args.output / "licenses" / component
        destination.mkdir(parents=True)
        with zipfile.ZipFile(args.cache / archive) as bundle:
            for member in bundle.namelist():
                if Path(member).name in names:
                    (destination / Path(member).name).write_bytes(bundle.read(member))
    (args.output / "README.txt").write_text(
        "StarBridge 便携客户端\n\n"
        "完整解压 ZIP 后，双击 starbridge.exe 打开设备中心；同账户设备可以互相连接。\n"
        "无需安装程序；请保留 runtime 和 licenses 目录。Windows 需要已安装 Microsoft Edge WebView2 Runtime。\n"
        "使用账号密码登录或注册，连接服务已内置。关闭窗口会退出客户端并停止其连接组件。\n\n"
        "右上角「中 / EN」可切换简体中文、English 或跟随系统，选择会在这台电脑上保留。\n\n"
        "账户和设备数据默认保存在当前用户的 %LOCALAPPDATA%\\StarBridge，与程序解压位置无关。\n"
        "重新解压或移动程序可继续使用原有设备；请勿复制、删除或转发用户数据目录。分享时使用原始 ZIP。\n\n"
        "StarBridge Portable Client\n\n"
        "Extract the entire ZIP, then double-click starbridge.exe. Keep the runtime and licenses folders.\n"
        "No installer is needed. Windows requires Microsoft Edge WebView2 Runtime.\n"
        "Sign in or create an account. The connection service is built in.\n"
        "Use the top-right globe / 中 / EN menu to choose English, 简体中文, or the system language.\n"
        "Your language choice is remembered on this computer. Closing the window stops the client and its connection components.\n"
        "Account and device data stay in %LOCALAPPDATA%\\StarBridge, independently of the extracted program folder.\n"
        "Moving or extracting the program again preserves the device identity. Do not share or delete the user data folder.\n"
        "Share the original ZIP, without your account data.\n",
        encoding="utf-8",
        newline="\n",
    )

    archive = shutil.make_archive(str(args.output), "zip", args.output)
    print(archive)
    return 0


if __name__ == "__main__":
    sys.exit(main())
