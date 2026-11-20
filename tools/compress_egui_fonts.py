#!/usr/bin/env python3
"""Losslessly store egui 0.32.3's default fonts as Windows XPRESS_HUFF buffers.

Example:
  py tools/compress_egui_fonts.py --fonts-dir <cargo-registry>/epaint_default_fonts-0.32.3/fonts
  py tools/compress_egui_fonts.py --fonts-dir <...> --check

The installed EXE expands these exact bytes with the Windows Compression API.
This utility changes no font table, face index, fallback order, or font license.
"""
from __future__ import annotations

import argparse
import ctypes
import hashlib
import json
import os
from pathlib import Path
import sys

ALGORITHM_XPRESS_HUFF = 4
MAX_BYTES = 1024 * 1024
ROOT = Path(__file__).resolve().parents[1]
OUTPUT = ROOT / "crates" / "snug-app" / "assets" / "fonts"

FONTS = (
    ("Hack-Regular.ttf", "hack.xpress", "Hack-Regular.txt", "15f55cc0c85a2988d2b4b3a8cdb5d77fdfbaf319e1bb5309d725db9818fb7125"),
    ("Ubuntu-Light.ttf", "ubuntu-light.xpress", "UFL.txt", "80307b8da7649aa4ee4d484b232140e3ce1ec0ca093073d3c53c8f5a5ced7a70"),
    ("NotoEmoji-Regular.ttf", "noto-emoji.xpress", "OFL.txt", "415dc6290378574135b64c808dc640c1df7531973290c4970c51fdeb849cb0c5"),
    ("emoji-icon-font.ttf", "emoji-icon.xpress", "emoji-icon-font-mit-license.txt", "3a706840d2f888b83c184f7d41900dbaa0151a806c4d130cc88539d53fedef0a"),
)

NOTO_ATTRIBUTION = (
    "Noto Emoji font attribution\n\n"
    "Font name: Noto Emoji\n"
    "Copyright 2013 Google Inc. All Rights Reserved.\n"
    "Noto is a trademark of Google Inc.\n"
    "Designed by Monotype Design Team.\n\n"
    "This font is distributed under the SIL Open Font License, Version 1.1.\n"
    "See OFL.txt for the complete license text.\n"
).encode("ascii")


def cabinet_api():
    if os.name != "nt":
        raise RuntimeError("XPRESS_HUFF font generation requires Windows Cabinet.dll")
    system_dir = Path(os.environ.get("SystemRoot", r"C:\Windows")) / "System32"
    dll = ctypes.WinDLL(str(system_dir / "cabinet.dll"), use_last_error=True)
    handle = ctypes.c_void_p
    size = ctypes.c_size_t
    dll.CreateCompressor.argtypes = [ctypes.c_uint32, ctypes.c_void_p, ctypes.POINTER(handle)]
    dll.CreateCompressor.restype = ctypes.c_int
    dll.Compress.argtypes = [handle, ctypes.c_void_p, size, ctypes.c_void_p, size, ctypes.POINTER(size)]
    dll.Compress.restype = ctypes.c_int
    dll.CloseCompressor.argtypes = [handle]
    dll.CloseCompressor.restype = ctypes.c_int
    return dll


def compress(dll, plain: bytes) -> bytes:
    if not plain or len(plain) > MAX_BYTES:
        raise ValueError(f"font size out of bounds: {len(plain)}")
    compressor = ctypes.c_void_p()
    if not dll.CreateCompressor(ALGORITHM_XPRESS_HUFF, None, ctypes.byref(compressor)):
        raise ctypes.WinError(ctypes.get_last_error())
    try:
        source = ctypes.create_string_buffer(plain)
        capacity = len(plain) + 65536
        destination = ctypes.create_string_buffer(capacity)
        written = ctypes.c_size_t()
        if not dll.Compress(compressor, source, len(plain), destination, capacity, ctypes.byref(written)):
            raise ctypes.WinError(ctypes.get_last_error())
        if not 0 < written.value <= MAX_BYTES:
            raise ValueError(f"compressed font size out of bounds: {written.value}")
        return destination.raw[:written.value]
    finally:
        dll.CloseCompressor(compressor)


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--fonts-dir", required=True, type=Path,
                        help="fonts/ directory from epaint_default_fonts 0.32.3")
    parser.add_argument("--check", action="store_true", help="verify outputs without modifying them")
    args = parser.parse_args()
    if args.fonts_dir.name != "fonts" or not args.fonts_dir.is_dir():
        parser.error("--fonts-dir must be the existing fonts/ directory from epaint_default_fonts 0.32.3")
    dll = cabinet_api()
    OUTPUT.mkdir(parents=True, exist_ok=True)
    license_dir = OUTPUT / "licenses"
    license_dir.mkdir(exist_ok=True)
    report = []
    failures = []
    for source_name, output_name, license_name, expected_hash in FONTS:
        source = (args.fonts_dir / source_name).read_bytes()
        actual_hash = hashlib.sha256(source).hexdigest()
        if actual_hash != expected_hash:
            failures.append(f"{source_name}: source SHA-256 mismatch ({actual_hash})")
            continue
        packed = compress(dll, source)
        output_path = OUTPUT / output_name
        license_source = args.fonts_dir / license_name
        if not license_source.is_file():
            failures.append(f"missing license file: {license_source}")
            continue
        output_matches = output_path.is_file() and output_path.read_bytes() == packed
        license_output = license_dir / license_name
        license_matches = license_output.is_file() and license_output.read_bytes() == license_source.read_bytes()
        if args.check:
            if not output_matches:
                failures.append(f"generated bytes differ: {output_path}")
            if not license_matches:
                failures.append(f"license is missing or changed: {license_output}")
        else:
            output_path.write_bytes(packed)
            license_output.write_bytes(license_source.read_bytes())
        report.append({"source": source_name, "source_bytes": len(source), "source_sha256": actual_hash,
                       "compressed": output_name, "compressed_bytes": len(packed),
                       "compressed_sha256": hashlib.sha256(packed).hexdigest(),
                       "license": license_name,
                       "license_bytes": license_source.stat().st_size})
    attribution = license_dir / "Noto-Emoji-attribution.txt"
    if args.check:
        if not attribution.is_file() or attribution.read_bytes() != NOTO_ATTRIBUTION:
            failures.append(f"license attribution is missing or changed: {attribution}")
    else:
        attribution.write_bytes(NOTO_ATTRIBUTION)
    if failures:
        print("\n".join(failures), file=sys.stderr)
        return 1
    print(json.dumps({"algorithm": "XPRESS_HUFF", "fonts": report,
                      "source_bytes": sum(item["source_bytes"] for item in report),
                      "compressed_bytes": sum(item["compressed_bytes"] for item in report)},
                     indent=2, ensure_ascii=False))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
