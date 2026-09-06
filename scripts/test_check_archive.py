"""Release-only regressions; no network, credentials, or product runtime."""

import io
from pathlib import Path
import tarfile
import tempfile
import unittest

from check_archive import check_archive


class ArchiveTests(unittest.TestCase):
    def validate(self, extras=(), headers=None, omit=False):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            manifest = root / "manifest.txt"
            manifest.write_text("LICENSE\n")
            archive = root / "package.tar.gz"
            with tarfile.open(archive, "w:gz", format=tarfile.PAX_FORMAT) as package:
                names = ["release/lowdown"]
                if not omit:
                    names.append("release/LICENSE")
                for name in names + list(extras):
                    member = tarfile.TarInfo(name)
                    member.size = 4
                    member.pax_headers = headers or {}
                    package.addfile(member, io.BytesIO(b"test"))
            check_archive(archive, "release", manifest)

    def test_clean_payload(self):
        self.validate()

    def test_rejects_appledouble(self):
        with self.assertRaisesRegex(ValueError, "Unexpected archive member"):
            self.validate(extras=["release/._lowdown"])

    def test_rejects_xattr_pax_header(self):
        with self.assertRaisesRegex(ValueError, "Extended attributes"):
            self.validate(headers={"SCHILY.xattr.com.apple.provenance": "test"})

    def test_rejects_duplicates(self):
        with self.assertRaisesRegex(ValueError, "Duplicate archive member"):
            self.validate(extras=["release/lowdown"])

    def test_rejects_missing_payload(self):
        with self.assertRaisesRegex(ValueError, "Missing archive members"):
            self.validate(omit=True)


if __name__ == "__main__":
    unittest.main()
