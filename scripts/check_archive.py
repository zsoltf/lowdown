"""Check raw release tar members, including metadata hidden by macOS tar."""

import argparse
from pathlib import Path
import tarfile


def check_archive(archive, root, manifest):
    expected = {f"{root}/lowdown"}
    expected.update(f"{root}/{line}" for line in manifest.read_text().splitlines())
    directories = {str(parent) for name in expected for parent in Path(name).parents}
    seen = set()
    with tarfile.open(archive, "r:gz") as package:
        for member in package:
            if any("xattr" in key.lower() for key in member.pax_headers):
                raise ValueError(f"Extended attributes: {member.name}")
            if member.isdir() and member.name.rstrip("/") in directories:
                continue
            if not member.isfile() or member.name not in expected:
                raise ValueError(f"Unexpected archive member: {member.name}")
            if member.name in seen:
                raise ValueError(f"Duplicate archive member: {member.name}")
            seen.add(member.name)
    if seen != expected:
        raise ValueError(f"Missing archive members: {sorted(expected - seen)}")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("archive", type=Path)
    parser.add_argument("root")
    args = parser.parse_args()
    check_archive(args.archive, args.root, Path(__file__).with_name("release-files.txt"))
    print("Raw archive payload verified")
