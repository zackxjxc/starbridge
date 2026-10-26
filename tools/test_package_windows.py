"""Portable-package checks; fixtures do not execute application binaries."""

from __future__ import annotations

import contextlib
import io
import hashlib
import json
from pathlib import Path
import sys
import tempfile
import unittest
from unittest.mock import patch
import zipfile

import package_windows


class PortablePackageTests(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory(prefix="starbridge package ")
        self.addCleanup(self.directory.cleanup)
        self.root = Path(self.directory.name)
        self.binary = self.root / "client.exe"
        self.binary.write_bytes(b"test-client")
        self.runtime = self.root / "runtime"
        self.runtime.mkdir()
        for name in package_windows.CLIENT_RUNTIME:
            (self.runtime / name).write_bytes(name.encode())
        (self.runtime / "runtime-manifest.json").write_text(json.dumps([
            {"component": "client", "files": [*package_windows.CLIENT_RUNTIME, "frps.exe"],
             "file_sha256": {name: hashlib.sha256(name.encode()).hexdigest()
                             for name in package_windows.CLIENT_RUNTIME}},
            {"component": "server", "files": ["frps.exe"]},
        ]), encoding="utf-8")
        self.cache = self.root / "cache"
        self.cache.mkdir()
        for archive, names in package_windows.LICENSES.values():
            with zipfile.ZipFile(self.cache / archive, "w") as bundle:
                for name in names:
                    bundle.writestr(f"component/{name}", "license")
        self.output = self.root / "portable output"

    def package(self):
        argv = ["package_windows.py", "--binary", str(self.binary),
                "--runtime", str(self.runtime), "--cache", str(self.cache),
                "--output", str(self.output)]
        with patch.object(sys, "argv", argv), contextlib.redirect_stdout(io.StringIO()):
            return package_windows.main()

    def test_portable_archive_contains_client_and_licenses(self):
        self.assertEqual(self.package(), 0)
        self.assertEqual((self.output / "starbridge.exe").read_bytes(), b"test-client")
        self.assertFalse((self.output / "runtime" / "frps.exe").exists())
        manifest = json.loads((self.output / "runtime" / "runtime-manifest.json").read_text(encoding="utf-8"))
        self.assertEqual(len(manifest), 1)
        self.assertNotIn("frps.exe", manifest[0]["files"])
        self.assertNotIn("frps.exe", manifest[0]["file_sha256"])
        self.assertEqual(manifest[0]["file_sha256"]["frpc.exe"],
                         hashlib.sha256(b"frpc.exe").hexdigest())
        self.assertFalse((self.output / "runtime-manifest.json").exists())
        self.assertTrue((self.output / "licenses" / "frp" / "LICENSE").is_file())
        self.assertTrue((self.output / "licenses" / "openssh" / "NOTICE.txt").is_file())
        readme = (self.output / "README.txt").read_text(encoding="utf-8")
        self.assertIn("双击 starbridge.exe", readme)
        self.assertNotIn(".bat", readme)
        self.assertEqual({path.name for path in self.output.iterdir()},
                         {"starbridge.exe", "README.txt", "runtime", "licenses"})
        expected_files = {"starbridge.exe", "README.txt", "runtime/runtime-manifest.json"}
        expected_files.update(f"runtime/{name}" for name in package_windows.CLIENT_RUNTIME)
        for component, (_, names) in package_windows.LICENSES.items():
            expected_files.update(f"licenses/{component}/{name}" for name in names)
        self.assertEqual({path.relative_to(self.output).as_posix()
                          for path in self.output.rglob("*") if path.is_file()}, expected_files)
        with zipfile.ZipFile(self.output.with_suffix(".zip")) as archive:
            self.assertIsNone(archive.testzip())
            self.assertEqual({name.split("/")[0] for name in archive.namelist()},
                             {"starbridge.exe", "README.txt", "runtime", "licenses"})
            self.assertEqual({entry.filename for entry in archive.infolist() if not entry.is_dir()},
                             expected_files)
            for name in expected_files:
                self.assertEqual(archive.read(name), (self.output / name).read_bytes())

    def test_used_output_is_preserved(self):
        data = self.output / "state" / "controller"
        data.mkdir(parents=True)
        sentinel = data / "identity.json"
        sentinel.write_text("private", encoding="utf-8")
        with self.assertRaisesRegex(SystemExit, "客户端数据"):
            self.package()
        self.assertEqual(sentinel.read_text(encoding="utf-8"), "private")

    def test_secure_and_migrated_profiles_are_preserved(self):
        for name in ("identity.secure", "auth.secure", "machine.json", "instance-scope.json", "legacy-migration.json"):
            with self.subTest(name=name):
                data = self.output / "old-profile"
                data.mkdir(parents=True, exist_ok=True)
                sentinel = data / name
                sentinel.write_text("private", encoding="utf-8")
                with self.assertRaisesRegex(SystemExit, "客户端数据"):
                    self.package()
                self.assertEqual(sentinel.read_text(encoding="utf-8"), "private")
                sentinel.unlink()

    def test_runtime_input_cannot_copy_private_data_into_zip(self):
        for name in ("identity.json", "identity.secure", "auth.secure", "machine.json", "instance-scope.json"):
            (self.runtime / name).write_bytes(b"private-profile-sentinel")
        ssh = self.runtime / "ssh"
        ssh.mkdir()
        (ssh / "id_ed25519").write_bytes(b"private-key-sentinel")
        manifest_path = self.runtime / "runtime-manifest.json"
        manifest = json.loads(manifest_path.read_text(encoding="utf-8"))
        manifest[0]["device_token"] = "private-token-sentinel"
        manifest_path.write_text(json.dumps(manifest), encoding="utf-8")
        self.assertEqual(self.package(), 0)
        with zipfile.ZipFile(self.output.with_suffix(".zip")) as archive:
            self.assertFalse(any(Path(name).name in package_windows.PRIVATE_DATA_NAMES for name in archive.namelist()))
            for name in archive.namelist():
                self.assertNotIn(b"private-", archive.read(name))

    def test_missing_input_does_not_remove_previous_package(self):
        self.output.mkdir()
        sentinel = self.output / "previous.txt"
        sentinel.write_text("keep", encoding="utf-8")
        (self.runtime / "frpc.exe").unlink()
        with self.assertRaisesRegex(SystemExit, "运行时缺少文件"):
            self.package()
        self.assertEqual(sentinel.read_text(encoding="utf-8"), "keep")

    def test_changed_runtime_is_rejected_before_replacing_output(self):
        self.output.mkdir()
        sentinel = self.output / "previous.txt"
        sentinel.write_text("keep", encoding="utf-8")
        (self.runtime / "frpc.exe").write_bytes(b"changed-after-download")
        with self.assertRaisesRegex(SystemExit, "文件校验失败"):
            self.package()
        self.assertEqual(sentinel.read_text(encoding="utf-8"), "keep")


if __name__ == "__main__":
    unittest.main()
