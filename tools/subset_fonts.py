"""Subset the Inter weights the app embeds, to keep the exe small (S8).

Keeps Basic Latin, Latin-1, Latin Extended-A, General Punctuation, a few arrows and
symbols, and every non-ASCII character found in the UI text (`ui/**/*.slint` and the
`copy.rs` files). Other scripts (Cyrillic, Greek, CJK, ...) fall back to a system font.

Re-run after adding UI text with a character outside those ranges (`--check` says so):

    python -m pip install --user fonttools
    python tools/subset_fonts.py          # rewrite crates/httporter-app/ui/fonts/Inter-*.ttf
    python tools/subset_fonts.py --check  # only verify that the UI text is covered

The full Inter 4.001 files are read from git history (FULL_FONTS_REV), so the subset can
be widened without a download. OFL.txt stays next to the fonts.
"""

import argparse
import io
import pathlib
import subprocess
import sys

from fontTools import subset
from fontTools.ttLib import TTFont

ROOT = pathlib.Path(__file__).resolve().parent.parent
FONTS = pathlib.Path("crates/httporter-app/ui/fonts")
WEIGHTS = ["Regular", "Medium", "SemiBold", "Bold"]
# The commit that added the full (unsubsetted) Inter files.
FULL_FONTS_REV = "34c3c2426cd582f8925247e5c0ef7dad02bc3950"

KEEP_RANGES = [
    (0x0000, 0x007F),  # Basic Latin
    (0x0080, 0x00FF),  # Latin-1 Supplement
    (0x0100, 0x017F),  # Latin Extended-A
    (0x2000, 0x206F),  # General Punctuation
    (0x20AC, 0x20AC),  # euro sign
    (0x2190, 0x2193),  # arrows
    (0x2713, 0x2713),  # check mark
]
# Defaults (kern, liga, calt, ccmp, locl, mark, ...) plus tabular and case-sensitive forms.
EXTRA_FEATURES = ["tnum", "case"]


def ui_characters():
    """Every non-ASCII character in the UI text, mapped to the files that use it."""
    app = ROOT / "crates/httporter-app"
    files = sorted((app / "ui").rglob("*.slint")) + sorted((ROOT / "crates").rglob("copy.rs"))
    found = {}
    for path in files:
        if "target" in path.parts:
            continue
        for ch in path.read_text(encoding="utf-8"):
            if ord(ch) > 0x7F:
                found.setdefault(ord(ch), set()).add(path.relative_to(ROOT).as_posix())
    return found


def full_font(weight):
    spec = f"{FULL_FONTS_REV}:{(FONTS / f'Inter-{weight}.ttf').as_posix()}"
    data = subprocess.run(
        ["git", "show", spec], cwd=ROOT, capture_output=True, check=True
    ).stdout
    return data


def options():
    opts = subset.Options()
    opts.layout_features = opts.layout_features + EXTRA_FEATURES
    # Medium and SemiBold name their family "Inter" only in IDs 16/17; keep every name.
    opts.name_IDs = ["*"]
    opts.hinting = True
    opts.recalc_timestamp = False
    return opts


def wanted(ui):
    codes = set(ui)
    for low, high in KEEP_RANGES:
        codes.update(range(low, high + 1))
    return codes


def check(ui):
    """Exit status 1 when a UI character that Inter has is missing from a committed subset."""
    missing = {}
    for weight in WEIGHTS:
        have = TTFont(io.BytesIO(full_font(weight))).getBestCmap()
        kept = TTFont(ROOT / FONTS / f"Inter-{weight}.ttf").getBestCmap()
        for code in ui:
            if code in have and code not in kept:
                missing.setdefault(code, []).append(weight)
    for code, weights in sorted(missing.items()):
        print(f"missing U+{code:04X} {chr(code)} in {', '.join(weights)}: {sorted(ui[code])}")
    return 1 if missing else 0


def main():
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--check", action="store_true", help="only verify the committed fonts")
    args = parser.parse_args()

    ui = ui_characters()
    if args.check:
        return check(ui)

    codes = wanted(ui)
    for weight in WEIGHTS:
        font = TTFont(io.BytesIO(full_font(weight)), recalcTimestamp=False)
        have = font.getBestCmap()
        if weight == WEIGHTS[0]:
            for code in sorted(c for c in ui if c not in have):
                print(f"note: Inter has no U+{code:04X} {chr(code)}; a system font draws it")
        subsetter = subset.Subsetter(options=options())
        subsetter.populate(unicodes=sorted(codes & set(have)))
        subsetter.subset(font)
        out = ROOT / FONTS / f"Inter-{weight}.ttf"
        font.save(out)
        print(f"{out.relative_to(ROOT).as_posix()}: {out.stat().st_size} bytes")
    return check(ui)


if __name__ == "__main__":
    sys.exit(main())
