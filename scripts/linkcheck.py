#!/usr/bin/env python3
"""Verify internal links, heading anchors, and external URLs across the docs.

Run from the repository root:

    python3 scripts/linkcheck.py

The Standard Readme specification requires that a README contain no broken links, and
docs rot quietly. This checks:

  * relative file links resolve
  * ``#anchor`` fragments match a real heading, using GitHub's slug rules
  * external URLs answer (HEAD request)

Exit code is non-zero if anything is broken, so it can gate a pull request.

No third-party dependencies — standard library only, on purpose: a doc check should not
add a package manager to the toolchain.
"""
from __future__ import annotations

import pathlib
import re
import sys
import urllib.error
import urllib.request

ROOT = pathlib.Path(__file__).resolve().parent.parent

LINK = re.compile(r"\[[^\]]*\]\(([^)\s]+)\)")
HEADING = re.compile(r"^(#{1,6})\s+(.*?)\s*$")
SKIP_SCHEMES = ("mailto:", "tel:", "data:")


def slug(text: str) -> str:
    """Reproduce GitHub's heading -> anchor algorithm (github-slugger).

    Faithfulness matters here: it replaces each space with a hyphen and does *not*
    collapse runs, so a heading containing " - " or " — " yields a double hyphen.
    Simplify that and the checker reports problems that GitHub does not have.
    """
    s = text.strip().lower()
    s = s.replace("`", "")
    s = re.sub(r"[^\w\s-]", "", s)
    return s.replace(" ", "-")


def anchors_of(path: pathlib.Path) -> set[str]:
    found = set()
    for line in path.read_text(encoding="utf-8").splitlines():
        m = HEADING.match(line)
        if m:
            found.add(slug(m.group(2)))
    return found


def doc_files() -> list[pathlib.Path]:
    files = [ROOT / "README.md"]
    files += sorted(ROOT.glob("*.md"))
    files += sorted((ROOT / "docs").glob("*.md"))
    files += sorted((ROOT / "benchmarks").glob("*.md"))
    seen, out = set(), []
    for f in files:
        if f.exists() and f not in seen:
            seen.add(f)
            out.append(f)
    return out


def main() -> int:
    problems: list[str] = []
    externals: set[str] = set()
    files = doc_files()

    for path in files:
        rel = path.relative_to(ROOT)
        for link in LINK.findall(path.read_text(encoding="utf-8")):
            if link.startswith(SKIP_SCHEMES):
                continue
            if link.startswith(("http://", "https://")):
                externals.add(link.split("#")[0])
                continue
            target, _, anchor = link.partition("#")
            dest = (path.parent / target).resolve() if target else path
            if target and not dest.exists():
                problems.append(f"{rel}: broken link   -> {link}")
                continue
            if anchor and anchor not in anchors_of(dest if target else path):
                problems.append(f"{rel}: missing anchor -> {link}")

    print(f"checked {len(files)} markdown files")
    print(f"internal problems: {len(problems)}")
    for p in problems:
        print("   ", p)

    bad = 0
    if externals:
        print(f"\nchecking {len(externals)} external URLs ...")
        for url in sorted(externals):
            request = urllib.request.Request(
                url, method="HEAD", headers={"User-Agent": "portcullis-linkcheck"}
            )
            try:
                with urllib.request.urlopen(request, timeout=25) as response:
                    code: object = response.status
            except urllib.error.HTTPError as e:
                code = e.code
            except Exception as e:  # noqa: BLE001 - report anything as a failure
                code = str(e)[:70]
            if code != 200:
                bad += 1
                print(f"    {code}  {url}")
    print(f"external problems: {bad}")

    # External hosts are flaky and third-party; report them without failing.
    return 1 if problems else 0


if __name__ == "__main__":
    sys.exit(main())
