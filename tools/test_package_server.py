"""Exercise server archives and release gates without network or executable fixtures."""

import hashlib
import io
import json
from pathlib import Path
import tarfile
import tempfile
import unittest
from unittest.mock import patch
import zipfile

import package_server
import release_metadata


class ServerPackageTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory(prefix="starbridge server package ")
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        self.binary = self.root / "server"
        self.binary.write_bytes(b"fixture-server")
        self.cache = self.root / "cache"
        self.cache.mkdir()
        self.output = self.root / "output"

    def package(self, platform, license_count=1):
        suffix = ".exe" if platform == "windows-x86_64" else ""
        name = f"frps{suffix}"
        archive = self.cache / ("frp.zip" if suffix else "frp.tar.gz")
        if suffix:
            with zipfile.ZipFile(archive, "w") as bundle:
                for index in range(license_count):
                    bundle.writestr(f"frp{index}/LICENSE", b"fixture-license")
        else:
            with tarfile.open(archive, "w:gz") as bundle:
                for index in range(license_count):
                    member = tarfile.TarInfo(f"frp{index}/LICENSE")
                    member.size = len(b"fixture-license")
                    bundle.addfile(member, io.BytesIO(b"fixture-license"))
        artifact = {"name": "frp", "version": "fixture", "platform": platform,
                    "url": f"https://example.test/{archive.name}", "sha256": package_server.sha256(archive)}

        def fetch(locked, cache, runtime):
            runtime.mkdir()
            (runtime / name).write_bytes(b"fixture-frps")
            (runtime / f"frpc{suffix}").write_bytes(b"client-must-not-ship")
            (runtime / "identity.secure").write_bytes(b"private-must-not-ship")
            return {**artifact, "files": [name], "file_sha256": {name: package_server.sha256(runtime / name)}}

        with patch.object(package_server, "parse_manifest", return_value=[artifact]), \
             patch.object(package_server, "fetch", side_effect=fetch):
            return package_server.package(self.binary, self.root / "manifest.toml", self.cache, self.output)

    def test_three_platform_archives_contain_only_server_runtime_license_and_instructions(self):
        for platform in package_server.PLATFORMS:
            with self.subTest(platform=platform):
                archive = self.package(platform)
                if archive.suffix == ".zip":
                    with zipfile.ZipFile(archive) as bundle:
                        files = {entry.filename: bundle.read(entry) for entry in bundle.infolist() if not entry.is_dir()}
                else:
                    with tarfile.open(archive) as bundle:
                        files = {member.name: bundle.extractfile(member).read()
                                 for member in bundle.getmembers() if member.isfile()}
                        for member in bundle.getmembers():
                            if member.name.endswith(("/starbridge-server", "/runtime/frps")):
                                self.assertEqual(member.mode & 0o777, 0o755)
                suffix = ".exe" if platform == "windows-x86_64" else ""
                self.assertEqual(set(files), {
                    f"StarBridge-server/starbridge-server{suffix}", f"StarBridge-server/runtime/frps{suffix}",
                    "StarBridge-server/runtime/runtime-manifest.json", "StarBridge-server/licenses/frp/LICENSE",
                    "StarBridge-server/README.txt",
                })
                record = json.loads(files["StarBridge-server/runtime/runtime-manifest.json"])[0]
                self.assertEqual(record["platform"], platform)
                self.assertEqual(record["file_sha256"], {f"frps{suffix}": hashlib.sha256(b"fixture-frps").hexdigest()})
                self.assertEqual(files["StarBridge-server/licenses/frp/LICENSE"], b"fixture-license")

    def test_existing_release_is_not_overwritten(self):
        archive = self.package("windows-x86_64")
        before = archive.read_bytes()
        with self.assertRaisesRegex(SystemExit, "发布包已存在"):
            self.package("windows-x86_64")
        self.assertEqual(archive.read_bytes(), before)

    def test_missing_or_ambiguous_license_does_not_produce_release(self):
        for count in (0, 2):
            for platform in package_server.PLATFORMS:
                with self.subTest(count=count, platform=platform):
                    with self.assertRaisesRegex(SystemExit, "缺少唯一的 LICENSE"):
                        self.package(platform, license_count=count)
                    self.assertFalse(any(self.output.iterdir()))


class ReleaseValidationTests(unittest.TestCase):
    def test_version_must_match_tag(self):
        version = release_metadata.release_version()
        self.assertEqual(release_metadata.release_version(version), version)
        self.assertEqual(release_metadata.release_version(f"v{version}"), version)
        for tag in ("v999.0.0", "latest", "../../escape", f"v{version}-rc.1"):
            with self.subTest(tag=tag), self.assertRaisesRegex(SystemExit, "不一致"):
                release_metadata.release_version(tag)

    def test_missing_empty_or_extra_packages_block_checksums(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            version = release_metadata.release_version()
            names = release_metadata.asset_names(version)
            for name in names[:-1]:
                (root / name).write_bytes(b"fixture")
            with self.assertRaisesRegex(SystemExit, "发布物不完整"):
                release_metadata.checksums(root, version)
            (root / names[-1]).touch()
            with self.assertRaisesRegex(SystemExit, "发布包为空"):
                release_metadata.checksums(root, version)
            (root / names[-1]).write_bytes(b"fixture")
            (root / "identity.secure").write_bytes(b"private")
            with self.assertRaisesRegex(SystemExit, "额外文件"):
                release_metadata.checksums(root, version)

    def test_complete_set_produces_repeatable_checksums_for_exactly_five_packages(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            version = release_metadata.release_version()
            for name in release_metadata.asset_names(version):
                (root / name).write_bytes(name.encode())
            checksums = release_metadata.checksums(root, version)
            before = checksums.read_bytes()
            self.assertEqual(len(before.splitlines()), 5)
            for line in before.decode().splitlines():
                digest, name = line.split("  ")
                self.assertEqual(digest, hashlib.sha256(name.encode()).hexdigest())
            self.assertEqual(release_metadata.checksums(root, version).read_bytes(), before)


if __name__ == "__main__":
    unittest.main()
