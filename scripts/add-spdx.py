#!/usr/bin/env python3
# SPDX-License-Identifier: MIT
"""Add `SPDX-License-Identifier: MIT` to every source file that lacks one.

Comment style: `//` for .rs, `#` for shell/yaml. Shebangs are preserved
(header inserted on line 2 when present). Files already containing the
SPDX line are skipped.

Run from repo root:  python3 scripts/add-spdx.py
"""
import os
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
# `examples/` hosts test-fixture debugees. Their source line numbers
# are encoded in `tests/debugger/*.rs` as breakpoint targets, so a
# header on line 1 would shift every breakpoint by 1 and require
# every test to be re-numbered. Until the harness moves to
# symbolic markers, fixtures stay header-less.
EXCLUDE_DIRS = {
    ".git",
    "target",
    "node_modules",
    "website",
    "extension",
    "examples",
    "examples/target",
}
EXTS = {
    ".rs": "// SPDX-License-Identifier: MIT",
    ".sh": "# SPDX-License-Identifier: MIT",
    ".yml": "# SPDX-License-Identifier: MIT",
    ".yaml": "# SPDX-License-Identifier: MIT",
    ".py": "# SPDX-License-Identifier: MIT",
}


def should_skip(path: Path) -> bool:
    rel = path.relative_to(ROOT).parts
    for part in rel:
        if part in EXCLUDE_DIRS:
            return True
    # examples/target sub-paths
    if "examples" in rel and "target" in rel:
        return True
    return False


def add_header(path: Path, header: str) -> bool:
    text = path.read_text(encoding="utf-8")
    if "SPDX-License-Identifier" in text.splitlines()[0:5].__str__():
        return False  # already present in the first few lines
    if "SPDX-License-Identifier" in text:
        return False  # somewhere later — leave alone
    lines = text.splitlines(keepends=True)
    if lines and lines[0].startswith("#!"):
        new = lines[0] + header + "\n" + "".join(lines[1:])
    else:
        new = header + "\n" + text
    path.write_text(new, encoding="utf-8")
    return True


def main() -> int:
    changed = 0
    for path in ROOT.rglob("*"):
        if not path.is_file():
            continue
        if should_skip(path):
            continue
        ext = path.suffix
        if ext not in EXTS:
            continue
        if add_header(path, EXTS[ext]):
            changed += 1
            print(f"  + {path.relative_to(ROOT)}")
    print(f"\nAdded SPDX header to {changed} file(s).")
    return 0


if __name__ == "__main__":
    sys.exit(main())
