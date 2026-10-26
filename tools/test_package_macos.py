"""Packaging safety and manifest checks without executing fixture binaries."""

import io
import json
import os
from pathlib import Path
import plistlib
import tarfile
import tempfile
import tomllib
import unittest
from unittest.mock import patch
import zipfile

import package_macos


class MacPackageTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory(prefix="starbridge mac package ")
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        self.binary = self.root / "client"
        self.binary.write_bytes(b"fixture-client")
        self.runtime = self.root / "runtime"
        self.cache = self.root / "cache"
        self.runtime.mkdir()
        self.cache.mkdir()
        self.output = self.root / "StarBridge.app"
        self.locked = []
        self.signing_commands = []
        records = []
        for component, filenames in package_macos.CLIENT_RUNTIME.items():
            archive = self.cache / f"{component}.tar.gz"
            with tarfile.open(archive, "w:gz") as bundle:
                for filename in package_macos.LICENSES[component]:
                    data = b"fixture-license"
                    member = tarfile.TarInfo(f"source/{filename}")
                    member.size = len(data)
                    bundle.addfile(member, io.BytesIO(data))
            record = {"name": component, "version": "fixture", "url": f"https://example.test/{archive.name}",
                      "sha256": package_macos.sha256(archive), "files": list(filenames), "file_sha256": {}}
            for filename in filenames:
                (self.runtime / filename).write_bytes(filename.encode())
                record["file_sha256"][filename] = package_macos.sha256(self.runtime / filename)
            self.locked.append(dict(record))
            records.append(record)
        (self.runtime / "frps").write_bytes(b"server")
        (self.runtime / "identity.json").write_bytes(b"private")
        (self.runtime / "runtime-manifest.json").write_text(json.dumps(records), encoding="utf-8")

    def run_command(self, command, **kwargs):
        if command[0] == "codesign" and "--sign" in command:
            self.signing_commands.append(command)
        if command[0] == "codesign" and "--sign" in command and Path(command[-1]).is_file():
            with Path(command[-1]).open("ab") as destination:
                destination.write(b"signed")
        if command[0] == "ditto":
            app = Path(command[-2])
            with zipfile.ZipFile(command[-1], "w") as archive:
                for path in app.rglob("*"):
                    if path.is_file():
                        archive.write(path, path.relative_to(app.parent))

    def package(self, signing_identity="-"):
        with patch.object(package_macos, "parse_manifest", return_value=self.locked), \
             patch.object(package_macos.subprocess, "run", side_effect=self.run_command):
            return package_macos.package(self.binary, self.runtime, self.cache, self.output, signing_identity)

    def test_fixed_signing_identity_is_used_for_app_and_every_runtime(self):
        identity = "Developer ID Application: Fixture"
        self.package(identity)
        self.assertEqual(len(self.signing_commands), 1 + sum(map(len, package_macos.CLIENT_RUNTIME.values())))
        self.assertTrue(all(command[command.index("--sign") + 1] == identity for command in self.signing_commands))

    def test_app_contains_signed_runtime_and_no_user_state(self):
        archive = self.package()
        contents = self.output / "Contents"
        info = plistlib.loads((contents / "Info.plist").read_bytes())
        self.assertEqual(info["CFBundleExecutable"], "starbridge")
        version = tomllib.loads((package_macos.REPO / "Cargo.toml").read_text(encoding="utf-8"))["workspace"]["package"]["version"]
        self.assertEqual(info["CFBundleShortVersionString"], version)
        self.assertEqual(info["CFBundleVersion"], version)
        self.assertEqual(info["LSMinimumSystemVersion"], "12.0")
        resources = contents / "Resources"
        records = json.loads((resources / "runtime-manifest.json").read_bytes())
        for record in records:
            for name, digest in record["file_sha256"].items():
                binary = resources / "runtime" / name
                self.assertEqual(package_macos.sha256(binary), digest)
                if os.name != "nt":
                    self.assertEqual(binary.stat().st_mode & 0o777, 0o755)
        self.assertEqual((resources / "runtime-manifest.json").read_bytes(),
                         (resources / "runtime/runtime-manifest.json").read_bytes())
        self.assertTrue((resources / "licenses/openssh/LICENCE").is_file())
        self.assertFalse((resources / "runtime/frps").exists())
        self.assertFalse((resources / "runtime/identity.json").exists())
        with zipfile.ZipFile(archive) as bundle:
            entry = bundle.getinfo("StarBridge.app/Contents/MacOS/starbridge")
            if os.name != "nt":
                self.assertEqual((entry.external_attr >> 16) & 0o777, 0o755)

    def test_existing_application_is_preserved(self):
        self.output.mkdir()
        sentinel = self.output / "identity.json"
        sentinel.write_bytes(b"private")
        with self.assertRaisesRegex(SystemExit, "尚不存在"):
            self.package()
        self.assertEqual(sentinel.read_bytes(), b"private")

    def test_changed_runtime_is_rejected(self):
        (self.runtime / "frpc").write_bytes(b"changed")
        with self.assertRaisesRegex(SystemExit, "运行时文件校验失败"):
            self.package()
        self.assertFalse(self.output.exists())

    def test_changed_source_archive_is_rejected(self):
        (self.cache / "frp.tar.gz").write_bytes(b"changed")
        with self.assertRaisesRegex(SystemExit, "来源包缺失或 SHA-256"):
            self.package()
        self.assertFalse(self.output.exists())


if __name__ == "__main__":
    unittest.main()
