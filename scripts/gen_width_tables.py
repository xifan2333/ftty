#!/usr/bin/env python3
"""Generate src/width/tables.rs from the python-wcwidth reference tables.

Run with the pinned reference implementation available on PYTHONPATH:

    pip install wcwidth==0.9.3
    python3 scripts/gen_width_tables.py

The generated tables are Unicode interval lists consumed by the Rust implementation of
the python-wcwidth `wcswidth()` specification (Unicode 18.0.0 at the time of writing).
"""

from __future__ import annotations

from pathlib import Path

import wcwidth
from wcwidth._constants import (
    _AMBIGUOUS_TABLE,
    _CATEGORY_MC_TABLE,
    _LATEST_VERSION,
    _WIDE_EASTASIAN_TABLE,
    _ZERO_WIDTH_TABLE,
)
from wcwidth.table_grapheme import EXTENDED_PICTOGRAPHIC, ISC_INVISIBLE_STACKER, ISC_VIRAMA
from wcwidth.table_vs15 import VS15_WIDE_TO_NARROW
from wcwidth.table_vs16 import VS16_NARROW_TO_WIDE


def merged(*tables: tuple[tuple[int, int], ...]) -> tuple[tuple[int, int], ...]:
    ranges = sorted((lo, hi) for table in tables for lo, hi in table)
    result: list[tuple[int, int]] = []
    for lo, hi in ranges:
        if result and lo <= result[-1][1] + 1:
            result[-1] = (result[-1][0], max(result[-1][1], hi))
        else:
            result.append((lo, hi))
    return tuple(result)


TABLES = {
    "ZERO_WIDTH": _ZERO_WIDTH_TABLE,
    "WIDE_EASTASIAN": _WIDE_EASTASIAN_TABLE,
    "AMBIGUOUS_EASTASIAN": _AMBIGUOUS_TABLE,
    "CATEGORY_MC": _CATEGORY_MC_TABLE,
    "VS16_NARROW_TO_WIDE": VS16_NARROW_TO_WIDE["9.0.0"],
    "VS15_WIDE_TO_NARROW": VS15_WIDE_TO_NARROW["9.0.0"],
    "EXTENDED_PICTOGRAPHIC": EXTENDED_PICTOGRAPHIC,
    "VIRAMA": merged(ISC_VIRAMA, ISC_INVISIBLE_STACKER),
}


def emit(name: str, ranges: tuple[tuple[int, int], ...]) -> list[str]:
    lines = [f"pub(crate) const {name}: &[(u32, u32)] = &["]
    for lo, hi in ranges:
        lines.append(f"    (0x{lo:X}, 0x{hi:X}),")
    lines.append("];")
    return lines


def main() -> None:
    output = Path(__file__).resolve().parent.parent / "src" / "width" / "tables.rs"
    output.parent.mkdir(parents=True, exist_ok=True)
    lines = [
        "//! Generated from the python-wcwidth reference implementation. Do not edit.",
        f"//! wcwidth {wcwidth.__version__}; Unicode {_LATEST_VERSION}.",
        "",
    ]
    for name, ranges in TABLES.items():
        lines.extend(emit(name, ranges))
        lines.append("")
    output.write_text("\n".join(lines))


if __name__ == "__main__":
    main()
