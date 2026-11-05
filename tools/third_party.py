"""Write THIRD-PARTY-NOTICES.txt: everything in httporter.exe that HTTPorter did not write.

    python -I tools/third_party.py                 # write THIRD-PARTY-NOTICES.txt at the repo root
    python -I tools/third_party.py --output FILE   # write it somewhere else (package.ps1 does)
    python -I tools/third_party.py --check         # write nothing; fail if a crate has no licence text

Standard library only. Needs `cargo` on PATH (or in %USERPROFILE%\\.cargo\\bin) and the
crate sources in the cargo registry, which any `cargo metadata` or build downloads.

What it lists:
- every crate in the normal and build dependency closure of `httporter-app` for
  x86_64-pc-windows-msvc, as the release build resolves it: name, version, licence
  expression, source, and the texts of its LICENSE* / LICENCE* / COPYING* / NOTICE* /
  UNLICENSE* / COPYRIGHT* files and LICENSES/ folder. Identical texts are printed once,
  with the crates that use them;
- the vendored, patched `parley` (vendor/parley);
- the Inter font (OFL-1.1, crates/httporter-app/ui/fonts/OFL.txt);
- for Slint, only the text of the licence HTTPorter takes (Slint Royalty-free 2.0).

The closure comes from `cargo tree`, which resolves features per package and target like
the real build; `cargo metadata --filter-platform` supplies each crate's details. (Its own
resolve unifies features across the workspace and dev-dependencies, and over-reports.)

A crate whose published package ships no licence file must be in FALLBACKS below, after
someone has read its upstream licence. `--check` fails on any other crate without a text.
"""

import argparse
import hashlib
import json
import os
import pathlib
import re
import shutil
import subprocess
import sys
import textwrap

ROOT = pathlib.Path(__file__).resolve().parent.parent
TARGET = "x86_64-pc-windows-msvc"
APP = "httporter-app"
DEFAULT_OUTPUT = ROOT / "THIRD-PARTY-NOTICES.txt"
INTER_OFL = ROOT / "crates" / "httporter-app" / "ui" / "fonts" / "OFL.txt"

LICENCE_PREFIXES = ("LICENSE", "LICENCE", "COPYING", "NOTICE", "UNLICENSE", "COPYRIGHT")
LICENCE_DIRS = ("licenses", "license")  # compared lower-case

# Slint is "GPL-3.0-only OR LicenseRef-Slint-Royalty-free-2.0 OR LicenseRef-Slint-Software-3.0".
# HTTPorter takes the royalty-free option, so the texts of the other two are left out.
SLINT_CHOSEN = "LicenseRef-Slint-Royalty-free-2.0"
SLINT_NOT_TAKEN = ("GPL-3.0-only", "LicenseRef-Slint-Software-3.0")

VENDORED = {
    "parley": (
        "Vendored in vendor/parley of the HTTPorter source and modified there (each change is "
        'marked "HTTPorter patch"): the dictionary word and line segmenters moved behind a '
        "feature nothing enables, and a deprecation lint allowed. Upstream: "
        "https://github.com/linebender/parley"
    ),
}

# Crates whose published package ships no licence file. Each entry was checked against the
# crate's upstream repository: the licence HTTPorter takes (from the crate's expression),
# the upstream copyright lines, and any extra notice the source needs. Texts are built from
# LICENSE-MIT / LICENSE-APACHE at the repo root and the constants at the end of this file.
FALLBACKS = {
    # AccessKit: "MIT OR Apache-2.0" (accesskit_winit: Apache-2.0); parts are derived from
    # Chromium's accessibility code, under Chromium's BSD licence (LICENSE.chromium upstream).
    "accesskit": ("Apache-2.0", [], ["chromium"]),
    "accesskit_consumer": ("Apache-2.0", [], ["chromium"]),
    "accesskit_windows": ("Apache-2.0", [], ["chromium"]),
    "accesskit_winit": ("Apache-2.0", [], []),
    # BSL-1.0 (upstream LICENSE, Boost Software License 1.0).
    "clipboard-win": ("BSL-1.0", [], []),
    # lyon: "MIT OR Apache-2.0", upstream LICENSE-MIT.
    "lyon_algorithms": ("MIT", ["Copyright (c) 2013 Nicolas Silva"], []),
    "lyon_extra": ("MIT", ["Copyright (c) 2013 Nicolas Silva"], []),
    "lyon_geom": ("MIT", ["Copyright (c) 2013 Nicolas Silva"], []),
    "lyon_path": ("MIT", ["Copyright (c) 2013 Nicolas Silva"], []),
    # MIT, upstream LICENSE.
    "rspolib": ("MIT", ["Copyright (c) 2023-2025 Álvaro Mondéjar Rubio"], []),
    "taffy": ("MIT", ["Copyright (c) 2018 Visly Inc.", "Copyright (c) 2026 Taffy Authors"], []),
}


def find_cargo():
    cargo = shutil.which("cargo")
    if cargo:
        return cargo
    home = pathlib.Path(os.environ.get("USERPROFILE", pathlib.Path.home()))
    for name in ("cargo.exe", "cargo"):
        candidate = home / ".cargo" / "bin" / name
        if candidate.exists():
            return str(candidate)
    sys.exit("third_party: cargo not found on PATH or in %USERPROFILE%\\.cargo\\bin")


def run_cargo(cargo, args):
    result = subprocess.run(
        [cargo, *args], cwd=ROOT, stdout=subprocess.PIPE, stderr=subprocess.PIPE
    )
    if result.returncode != 0:
        sys.stderr.write(result.stderr.decode("utf-8", "replace"))
        sys.exit(f"third_party: cargo {' '.join(args)} failed (exit {result.returncode})")
    return result.stdout.decode("utf-8")


def normalise(text):
    text = text.lstrip("\ufeff").replace("\r\n", "\n").replace("\r", "\n")
    lines = [line.rstrip() for line in text.split("\n")]
    while lines and not lines[0]:
        lines.pop(0)
    while lines and not lines[-1]:
        lines.pop()
    return "\n".join(lines)


def read_text(path):
    return normalise(path.read_bytes().decode("utf-8", "replace"))


def is_slint(package):
    return SLINT_CHOSEN in (package.get("license") or "")


def licence_files(package):
    """(label, path) for each licence file in the crate's source folder."""
    folder = pathlib.Path(package["manifest_path"]).parent
    found = []
    for entry in sorted(folder.iterdir()):
        if entry.is_file() and entry.name.upper().startswith(LICENCE_PREFIXES):
            found.append(entry)
        elif entry.is_dir() and entry.name.lower() in LICENCE_DIRS:
            found.extend(sorted(p for p in entry.iterdir() if p.is_file()))
    declared = package.get("license_file")
    if declared and (folder / declared).is_file():
        found.append(folder / declared)
    if is_slint(package):
        found = [p for p in found if not p.stem.startswith(SLINT_NOT_TAKEN)]
    labels, unique = set(), []
    for path in found:
        label = path.relative_to(folder).as_posix() if path.is_relative_to(folder) else path.name
        if label not in labels:
            labels.add(label)
            unique.append((label, path))
    return unique


def mit_text(copyright_lines):
    body = read_text(ROOT / "LICENSE-MIT").split("\n")
    # LICENSE-MIT is "MIT License", blank, our copyright line, blank, then the permission text.
    start = next(i for i, line in enumerate(body) if line.startswith("Permission is hereby granted"))
    return "\n".join(["MIT License", "", *copyright_lines, "", *body[start:]])


def fallback_texts(name):
    licence, copyright_lines, extras = FALLBACKS[name]
    texts = []
    if licence == "MIT":
        texts.append(("MIT (standard text, upstream copyright)", mit_text(copyright_lines)))
    elif licence == "Apache-2.0":
        texts.append(("Apache-2.0 (standard text)", read_text(ROOT / "LICENSE-APACHE")))
    elif licence == "BSL-1.0":
        texts.append(("BSL-1.0 (standard text)", normalise(BSL_1_0)))
    else:
        sys.exit(f"third_party: FALLBACKS[{name!r}] names {licence}, which has no text here")
    for extra in extras:
        label, text = EXTRA_NOTICES[extra]
        texts.append((label, normalise(text)))
    return licence, texts


def closure(cargo):
    tree = run_cargo(
        cargo,
        [
            "tree", "--locked", "-p", APP, "-e", "normal,build", "--target", TARGET,
            "--prefix", "none", "--no-dedupe", "--format", "{p}",
        ],
    )
    keys = set()
    for line in tree.splitlines():
        match = re.match(r"^(\S+) v(\S+)", line.strip())
        if match:
            keys.add((match[1], match[2]))
    return keys


def collect(cargo):
    metadata = json.loads(
        run_cargo(cargo, ["metadata", "--format-version", "1", "--locked", "--filter-platform", TARGET])
    )
    workspace = set(metadata["workspace_members"])
    by_key = {(p["name"], p["version"]): p for p in metadata["packages"]}
    app = next(p for p in metadata["packages"] if p["id"] in workspace and p["name"] == APP)
    crates = []
    for key in sorted(closure(cargo)):
        package = by_key.get(key)
        if package is None:
            sys.exit(f"third_party: {key[0]} {key[1]} is in cargo tree but not in cargo metadata")
        if package["id"] not in workspace:
            crates.append(package)
    return app["version"], crates


def build(cargo):
    version, crates = collect(cargo)
    texts = {}  # normalised text -> {"n": number, "files": set, "users": list}
    entries, missing = [], []

    def add_text(label, text, user):
        record = texts.setdefault(text, {"files": set(), "users": []})
        record["files"].add(label)
        if user not in record["users"]:
            record["users"].append(user)
        return record

    # Inter first: it is not a crate.
    inter = "Inter 4.001 font (subset), crates/httporter-app/ui/fonts"
    entries.append(
        {
            "title": inter,
            "licence": "OFL-1.1",
            "source": ["https://github.com/rsms/inter"],
            "chosen": None,
            "note": None,
            "records": [add_text("OFL.txt", read_text(INTER_OFL), "Inter 4.001")],
        }
    )

    for package in crates:
        name, ver = package["name"], package["version"]
        user = f"{name} {ver}"
        files = licence_files(package)
        records, chosen, note = [], None, VENDORED.get(name)
        if files:
            records = [add_text(label, read_text(path), user) for label, path in files]
            # A crate that ships one text twice (LICENSE and LICENSE.md) lists it once.
            records = list({id(r): r for r in records}.values())
        elif name in FALLBACKS:
            chosen, fallback = fallback_texts(name)
            records = [add_text(label, text, user) for label, text in fallback]
            note = "The published crate ships no licence file; the text below is its upstream licence."
        else:
            missing.append(package)
        if is_slint(package):
            chosen = SLINT_CHOSEN
        if chosen == package.get("license"):
            chosen = None
        source = []
        if package.get("repository"):
            source.append(package["repository"])
        elif package.get("homepage"):
            source.append(package["homepage"])
        if (package.get("source") or "").startswith("registry+"):
            source.append(f"https://crates.io/crates/{name}/{ver}")
        entries.append(
            {
                "title": user,
                "licence": package.get("license") or "(none declared)",
                "source": source,
                "chosen": chosen,
                "note": note,
                "records": records,
            }
        )

    # Number texts in order of first use, so the file reads top to bottom.
    number = 0
    for entry in entries:
        for record in entry["records"]:
            if "n" not in record:
                number += 1
                record["n"] = number
    return version, entries, texts, missing


def render(version, entries, texts):
    out = []
    w = out.append
    title = f"HTTPorter {version} - third-party notices"
    w(title)
    w("=" * len(title))
    w("")
    w("HTTPorter is free software under the MIT License or the Apache License 2.0, at your")
    w("option (LICENSE-MIT, LICENSE-APACHE). httporter.exe also contains the third-party")
    w("software listed below, each under its own licence, whose texts follow the list.")
    w("")
    w("Made with Slint. HTTPorter's user interface uses Slint (https://slint.dev) by")
    w("SixtyFPS GmbH under the Slint Royalty-free Desktop, Mobile, and Web Applications")
    w("License 2.0; the AboutSlint credit is in Settings > About.")
    w("")
    w(f"Generated by tools/third_party.py from Cargo.lock, for {TARGET}.")
    w(f"{len(entries)} components, {len(texts)} distinct licence texts.")
    w("")
    w("")
    w("1. Components")
    w("-------------")
    w("")
    for entry in entries:
        refs = ", ".join(f"[{r['n']}]" for r in entry["records"]) or "none"
        w(entry["title"])
        w(f"    Licence: {entry['licence']}")
        if entry["chosen"]:
            w(f"    Used under: {entry['chosen']}")
        for i, url in enumerate(entry["source"]):
            w(f"    {'Source: ' if i == 0 else '        '}{url}")
        if entry["note"]:
            w(textwrap.fill(entry["note"], 90, initial_indent="    Note: ", subsequent_indent="          "))
        w(f"    Texts: {refs}")
        w("")
    w("")
    w("2. Licence texts")
    w("----------------")
    for text, record in sorted(texts.items(), key=lambda item: item[1]["n"]):
        w("")
        w("=" * 78)
        w(f"[{record['n']}] {', '.join(sorted(record['files']))}")
        w(textwrap.fill("Used by: " + ", ".join(record["users"]), 78, subsequent_indent="         "))
        w("=" * 78)
        w("")
        w(text)
    w("")
    return "\n".join(out)


def main():
    parser = argparse.ArgumentParser(description=__doc__.split("\n", 1)[0])
    parser.add_argument("--check", action="store_true", help="write nothing; fail if a crate has no licence text")
    parser.add_argument("--output", type=pathlib.Path, default=DEFAULT_OUTPUT, help="where to write the notices")
    args = parser.parse_args()

    version, entries, texts, missing = build(find_cargo())
    if missing:
        print("third_party: these crates ship no licence text and are not in FALLBACKS:", file=sys.stderr)
        for package in missing:
            print(
                f"  {package['name']} {package['version']} ({package.get('license')}) "
                f"{package.get('repository') or ''}",
                file=sys.stderr,
            )
        print("Read each one's upstream licence, then add it to FALLBACKS in tools/third_party.py.", file=sys.stderr)
        return 1

    unused = sorted(set(FALLBACKS) - {e["title"].split(" ")[0] for e in entries})
    if unused:
        print(f"third_party: note: FALLBACKS entries no longer needed: {', '.join(unused)}", file=sys.stderr)

    if args.check:
        print(f"third_party: ok, {len(entries)} components, {len(texts)} distinct licence texts")
        return 0

    content = render(version, entries, texts)
    args.output.parent.mkdir(parents=True, exist_ok=True)
    # CRLF: the file is read in Notepad on any Windows 10.
    with open(args.output, "w", encoding="utf-8", newline="\r\n") as handle:
        handle.write(content)
    digest = hashlib.sha256(content.encode("utf-8")).hexdigest()[:12]
    print(f"third_party: wrote {args.output} ({len(entries)} components, {len(texts)} texts, sha256 {digest}...)")
    return 0


# Boost Software License 1.0, as in clipboard-win's upstream LICENSE.
BSL_1_0 = """\
Boost Software License - Version 1.0 - August 17th, 2003

Permission is hereby granted, free of charge, to any person or organization
obtaining a copy of the software and accompanying documentation covered by
this license (the "Software") to use, reproduce, display, distribute,
execute, and transmit the Software, and to prepare derivative works of the
Software, and to permit third-parties to whom the Software is furnished to
do so, all subject to the following:

The copyright notices in the Software and this entire statement, including
the above license grant, this restriction and the following disclaimer,
must be included in all copies of the Software, in whole or in part, and
all derivative works of the Software, unless such copies or derivative
works are solely in the form of machine-executable object code generated by
a source language processor.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE, TITLE AND NON-INFRINGEMENT. IN NO EVENT
SHALL THE COPYRIGHT HOLDERS OR ANYONE DISTRIBUTING THE SOFTWARE BE LIABLE
FOR ANY DAMAGES OR OTHER LIABILITY, WHETHER IN CONTRACT, TORT OR OTHERWISE,
ARISING FROM, OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER
DEALINGS IN THE SOFTWARE.
"""

# AccessKit's upstream LICENSE.chromium (BSD-3-Clause), for the Chromium-derived code.
CHROMIUM_BSD = """\
Copyright 2015 The Chromium Authors. All rights reserved.

Redistribution and use in source and binary forms, with or without
modification, are permitted provided that the following conditions are
met:

   * Redistributions of source code must retain the above copyright
notice, this list of conditions and the following disclaimer.
   * Redistributions in binary form must reproduce the above
copyright notice, this list of conditions and the following disclaimer
in the documentation and/or other materials provided with the
distribution.
   * Neither the name of Google Inc. nor the names of its
contributors may be used to endorse or promote products derived from
this software without specific prior written permission.

THIS SOFTWARE IS PROVIDED BY THE COPYRIGHT HOLDERS AND CONTRIBUTORS
"AS IS" AND ANY EXPRESS OR IMPLIED WARRANTIES, INCLUDING, BUT NOT
LIMITED TO, THE IMPLIED WARRANTIES OF MERCHANTABILITY AND FITNESS FOR
A PARTICULAR PURPOSE ARE DISCLAIMED. IN NO EVENT SHALL THE COPYRIGHT
OWNER OR CONTRIBUTORS BE LIABLE FOR ANY DIRECT, INDIRECT, INCIDENTAL,
SPECIAL, EXEMPLARY, OR CONSEQUENTIAL DAMAGES (INCLUDING, BUT NOT
LIMITED TO, PROCUREMENT OF SUBSTITUTE GOODS OR SERVICES; LOSS OF USE,
DATA, OR PROFITS; OR BUSINESS INTERRUPTION) HOWEVER CAUSED AND ON ANY
THEORY OF LIABILITY, WHETHER IN CONTRACT, STRICT LIABILITY, OR TORT
(INCLUDING NEGLIGENCE OR OTHERWISE) ARISING IN ANY WAY OUT OF THE USE
OF THIS SOFTWARE, EVEN IF ADVISED OF THE POSSIBILITY OF SUCH DAMAGE.
"""

EXTRA_NOTICES = {"chromium": ("LICENSE.chromium (upstream)", CHROMIUM_BSD)}


if __name__ == "__main__":
    sys.exit(main())
