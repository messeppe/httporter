"""Put the SPDX licence line at the top of every Rust and Slint source file.

    python -I tools/spdx_headers.py           # add the line where it is missing
    python -I tools/spdx_headers.py --check   # change nothing; fail if a file lacks it

The line is `// SPDX-License-Identifier: MIT OR Apache-2.0`, the first line of every `.rs`
and `.slint` file under crates/ and tools/, followed by a blank line. Skipped: vendor/
(third-party code keeps its own licence), target/ folders, and generated files (a
`@generated` or "automatically generated" marker in the first five lines).

A file whose first line is a different SPDX line is never rewritten: it is reported, and
`--check` fails, so code under another licence is a decision someone writes down here.
Standard library only. Line endings and a UTF-8 byte-order mark are kept as they are.
"""

import argparse
import pathlib
import sys

ROOT = pathlib.Path(__file__).resolve().parent.parent
HEADER = "// SPDX-License-Identifier: MIT OR Apache-2.0"
SPDX_PREFIX = "// SPDX-License-Identifier:"
ROOTS = ("crates", "tools")
EXTENSIONS = {".rs", ".slint"}
SKIP_DIRS = {"vendor", "target", ".git"}
GENERATED_MARKERS = ("@generated", "automatically generated")
BOM = "\ufeff"

# Files under ROOTS that keep a different SPDX line on purpose: path -> reason.
OTHER_LICENCE = {}


def sources():
    for top in ROOTS:
        base = ROOT / top
        if not base.is_dir():
            continue
        for path in sorted(base.rglob("*")):
            relative = path.relative_to(ROOT)
            if any(part in SKIP_DIRS for part in relative.parts):
                continue
            if path.is_file() and path.suffix in EXTENSIONS:
                yield path


def is_generated(text):
    head = "\n".join(text.splitlines()[:5]).lower()
    return any(marker.lower() in head for marker in GENERATED_MARKERS)


def classify(text):
    """'ok', 'missing', 'other' (a different SPDX line) or 'generated'."""
    body = text[1:] if text.startswith(BOM) else text
    first = body.split("\n", 1)[0].rstrip("\r")
    if first == HEADER:
        return "ok"
    if is_generated(body):
        return "generated"
    if first.startswith(SPDX_PREFIX):
        return "other"
    return "missing"


def with_header(text):
    bom = BOM if text.startswith(BOM) else ""
    body = text[len(bom):]
    newline = "\r\n" if "\r\n" in body else "\n"
    if not body.strip():
        return bom + HEADER + newline
    separator = newline if body.startswith(("\n", "\r\n")) else newline + newline
    return bom + HEADER + separator + body


def main():
    parser = argparse.ArgumentParser(description=__doc__.split("\n", 1)[0])
    parser.add_argument("--check", action="store_true", help="change nothing; fail if a file lacks the line")
    args = parser.parse_args()

    missing, other, added, checked = [], [], [], 0
    for path in sources():
        relative = path.relative_to(ROOT).as_posix()
        # newline="" keeps CRLF / LF exactly as stored.
        with open(path, encoding="utf-8", newline="") as handle:
            text = handle.read()
        state = classify(text)
        if state == "generated":
            continue
        checked += 1
        if state == "other" and relative not in OTHER_LICENCE:
            other.append(relative)
        elif state == "missing":
            if args.check:
                missing.append(relative)
            else:
                with open(path, "w", encoding="utf-8", newline="") as handle:
                    handle.write(with_header(text))
                added.append(relative)

    for relative in other:
        print(f"spdx_headers: {relative}: a different SPDX line; add it to OTHER_LICENCE if intended", file=sys.stderr)
    if args.check:
        for relative in missing:
            print(f"spdx_headers: missing: {relative}", file=sys.stderr)
        if missing:
            print(
                f"spdx_headers: {len(missing)} of {checked} files lack the line; "
                "run `python -I tools/spdx_headers.py` to add it",
                file=sys.stderr,
            )
        if missing or other:
            return 1
        print(f"spdx_headers: ok, {checked} files")
        return 0

    print(f"spdx_headers: added the line to {len(added)} of {checked} files")
    return 1 if other else 0


if __name__ == "__main__":
    sys.exit(main())
