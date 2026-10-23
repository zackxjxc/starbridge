#!/usr/bin/env python3
"""Assemble a Windows test package from a release build and a verified runtime.

Run after `cargo build --release -p starbridge-client` and
`python tools/fetch_runtime.py --output state/runtime`.
"""

from __future__ import annotations

import argparse
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


def main() -> int:
    parser = argparse.ArgumentParser()
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

    if args.output.exists():
        shutil.rmtree(args.output)
    runtime = args.output / "runtime"
    runtime.mkdir(parents=True)
    shutil.copy2(args.binary, args.output / "starbridge.exe")
    for name in CLIENT_RUNTIME:
        shutil.copy2(args.runtime / name, runtime / name)
    manifest = json.loads((args.runtime / "runtime-manifest.json").read_text(encoding="utf-8"))
    for record in manifest:
        record["files"] = [name for name in record["files"] if name in CLIENT_RUNTIME]
    (args.output / "runtime-manifest.json").write_text(
        json.dumps(manifest, ensure_ascii=False, indent=2) + "\n", encoding="utf-8"
    )
    for component, (archive, names) in LICENSES.items():
        destination = args.output / "licenses" / component
        destination.mkdir(parents=True)
        with zipfile.ZipFile(args.cache / archive) as bundle:
            for member in bundle.namelist():
                if Path(member).name in names:
                    (destination / Path(member).name).write_bytes(bundle.read(member))
    for script in [*Path("deploy/windows").glob("*.bat"), *Path("deploy/windows").glob("*.ps1")]:
        shutil.copy2(script, args.output / script.name)

    archive = shutil.make_archive(str(args.output), "zip", args.output.parent, args.output.name)
    print(archive)
    return 0


if __name__ == "__main__":
    sys.exit(main())
