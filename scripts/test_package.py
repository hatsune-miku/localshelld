"""Release entry points and archive compatibility, without installing services."""
import os
from pathlib import Path
import shutil
import subprocess
import tarfile
import tempfile
import unittest
import zipfile


ROOT = Path(__file__).resolve().parent.parent


@unittest.skipIf(os.name == "nt", "run shell fixtures on Unix")
class PackageTests(unittest.TestCase):
    def test_unix_entry_point_quotes_its_directory_and_forwards_arguments(self):
        with tempfile.TemporaryDirectory(prefix="trustd-package-") as temp:
            directory = Path(temp) / "space ' quote $ dollar"
            directory.mkdir()
            launcher = directory / "INSTALL.command"
            shutil.copyfile(ROOT / "scripts/package/INSTALL.sh", launcher)
            launcher.chmod(0o755)
            binary = directory / "trustd"
            binary.write_text('#!/bin/sh\nprintf "%s\\n" "$@"\n', encoding="utf-8")
            binary.chmod(0o755)
            result = subprocess.run(
                [str(launcher), "--no-open"], cwd=temp, check=True,
                text=True, capture_output=True,
            )
            self.assertEqual(result.stdout, "maintenance\n--no-open\n")

    @unittest.skipUnless(all(shutil.which(tool) for tool in ("bash", "zip", "tar", "sha256sum")), "packaging tools required")
    def test_release_layout_keeps_update_programs_and_adds_installers(self):
        with tempfile.TemporaryDirectory(prefix="trustd-package-") as temp:
            directory = Path(temp)
            shutil.copytree(ROOT / "scripts/package", directory / "scripts/package")
            shutil.copyfile(ROOT / "scripts/package-release.sh", directory / "scripts/package-release.sh")
            for file in ("README.md", "LICENSE-MIT"):
                shutil.copyfile(ROOT / file, directory / file)
            platforms = {
                "windows-x86_64": ("trustd-system.exe", "INSTALL.cmd"),
                "macos-aarch64": ("trustd-root", "INSTALL.command"),
                "linux-x86_64": ("trustd-root", "INSTALL.sh"),
            }
            programs = {}
            for platform, (privileged, _) in platforms.items():
                source = directory / "binaries" / f"trustd-{platform}"
                source.mkdir(parents=True)
                suffix = ".exe" if platform.startswith("windows") else ""
                programs[platform] = [f"{role}{suffix}" for role in ("trustd", "trustd-user", "trustd-stdio")] + [privileged]
                for name in programs[platform]:
                    (source / name).write_bytes(b"fixture program")
                (source / "release-version.txt").write_text("trustd 0.1.0-test\n", encoding="utf-8")
            subprocess.run(
                ["bash", "scripts/package-release.sh"], cwd=directory, check=True,
                env={**os.environ, "LOCALSHELLD_RELEASE_TAG": "v0.1.0-test"},
                capture_output=True, text=True,
            )
            for platform, (_, launcher) in platforms.items():
                prefix = f"trustd-{platform}"
                if platform.startswith("windows"):
                    with zipfile.ZipFile(directory / "assets" / f"{prefix}.zip") as archive:
                        names = archive.namelist()
                else:
                    with tarfile.open(directory / "assets" / f"{prefix}.tar.gz") as archive:
                        names = archive.getnames()
                        self.assertTrue(archive.getmember(f"{prefix}/{launcher}").mode & 0o111)
                        self.assertNotIn(b"\r", archive.extractfile(f"{prefix}/{launcher}").read())
                for name in [*programs[platform], launcher, "START-HERE.txt", "README.md", "LICENSE-MIT"]:
                    self.assertIn(f"{prefix}/{name}", names)
            checksums = (directory / "assets/SHA256SUMS").read_text()
            self.assertEqual(len(checksums.splitlines()), 3)


if __name__ == "__main__":
    unittest.main()
