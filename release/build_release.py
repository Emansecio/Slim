#!/usr/bin/env python3
"""Build the deterministic Slim Windows release archive."""

from __future__ import annotations

import hashlib
import json
from pathlib import Path
import zipfile


VERSION = "0.1.0"
ARCHIVE_NAME = f"slim-{VERSION}-windows-x64.zip"
ARCHIVE_ENTRY = "slim.exe"


def sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        for chunk in iter(lambda: stream.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest().upper()


def write_archive(executable: Path, archive: Path) -> None:
    data = executable.read_bytes()
    info = zipfile.ZipInfo(ARCHIVE_ENTRY, date_time=(1980, 1, 1, 0, 0, 0))
    info.create_system = 0
    info.create_version = 20
    info.extract_version = 20
    info.flag_bits = 0
    info.compress_type = zipfile.ZIP_DEFLATED
    info.comment = b""
    info.internal_attr = 0
    info.external_attr = 0
    with zipfile.ZipFile(
        archive,
        mode="w",
        compression=zipfile.ZIP_DEFLATED,
        compresslevel=9,
        strict_timestamps=True,
    ) as output:
        output.writestr(info, data)


def main() -> None:
    root = Path(__file__).resolve().parent.parent
    executable = root / "target" / "release" / ARCHIVE_ENTRY
    release_dir = root / "release"
    archive = release_dir / ARCHIVE_NAME
    manifest = release_dir / "manifest.json"
    sums = release_dir / "SHA256SUMS.txt"

    if not executable.is_file():
        raise SystemExit(f"missing release executable: {executable}")
    release_dir.mkdir(parents=True, exist_ok=True)

    write_archive(executable, archive)
    executable_hash = sha256(executable)
    archive_hash = sha256(archive)

    manifest_data = {
        "name": "slim",
        "version": VERSION,
        "platform": "windows-x64-msvc",
        "binary": "target/release/slim.exe",
        "zip": f"release/{ARCHIVE_NAME}",
        "archive_entries": [ARCHIVE_ENTRY],
        "sha256": {
            ARCHIVE_ENTRY: executable_hash,
            ARCHIVE_NAME: archive_hash,
        },
    }
    manifest.write_text(
        json.dumps(manifest_data, ensure_ascii=False, indent=2) + "\n",
        encoding="utf-8",
        newline="\n",
    )
    sums.write_text(
        f"{executable_hash}  target/release/slim.exe\n"
        f"{archive_hash}  release/{ARCHIVE_NAME}\n",
        encoding="utf-8",
        newline="\n",
    )

    print(json.dumps(manifest_data, ensure_ascii=False, indent=2))


if __name__ == "__main__":
    main()
