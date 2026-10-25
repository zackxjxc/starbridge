"""Portable-package checks; fixtures do not execute application binaries."""

from __future__ import annotations

import contextlib
import io
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
            {"component": "client", "files": [*package_windows.CLIENT_RUNTIME, "frps.exe"]},
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
        manifest = json.loads((self.output / "runtime-manifest.json").read_text(encoding="utf-8"))
        self.assertEqual(len(manifest), 1)
        self.assertNotIn("frps.exe", manifest[0]["files"])
        self.assertTrue((self.output / "licenses" / "frp" / "LICENSE").is_file())
        self.assertTrue((self.output / "licenses" / "openssh" / "NOTICE.txt").is_file())
        self.assertIn("Tauri", (self.output / "README.txt").read_text(encoding="utf-8"))
        with zipfile.ZipFile(self.output.with_suffix(".zip")) as archive:
            self.assertIn(f"{self.output.name}/starbridge.exe", archive.namelist())
            self.assertFalse(any(name.endswith((".msi", "frps.exe")) for name in archive.namelist()))

    def test_used_output_is_preserved(self):
        data = self.output / "state" / "controller"
        data.mkdir(parents=True)
        sentinel = data / "identity.json"
        sentinel.write_text("private", encoding="utf-8")
        with self.assertRaisesRegex(SystemExit, "客户端数据"):
            self.package()
        self.assertEqual(sentinel.read_text(encoding="utf-8"), "private")

    def test_missing_input_does_not_remove_previous_package(self):
        self.output.mkdir()
        sentinel = self.output / "previous.txt"
        sentinel.write_text("keep", encoding="utf-8")
        (self.runtime / "frpc.exe").unlink()
        with self.assertRaisesRegex(SystemExit, "运行时缺少文件"):
            self.package()
        self.assertEqual(sentinel.read_text(encoding="utf-8"), "keep")


if __name__ == "__main__":
    unittest.main()
