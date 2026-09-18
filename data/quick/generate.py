#!/usr/bin/env python3
"""Regenerates the character tables `ioexplorer-quick` embeds.

    python3 data/quick/generate.py [SOURCE_DIR]

Reads, from SOURCE_DIR (downloaded there first when missing):

  emoji-test.txt     https://unicode.org/Public/emoji/latest/emoji-test.txt
  UnicodeData.txt    https://unicode.org/Public/UCD/latest/ucd/UnicodeData.txt
  annotations.json   CLDR English annotations, from cldr-json

and writes, next to this script:

  emoji.tsv    group, subgroup, emoji, emoji version, skin tones (1/0), name,
               keywords — fully-qualified emoji only, without their skin-tone
               variants, which the picker derives from the base.
  unicode.tsv  code point (hex), name, keywords — every assigned character
               that can stand on its own in the grid.
"""

import json
import re
import sys
import urllib.request
from pathlib import Path

SOURCES = {
    "emoji-test.txt": "https://unicode.org/Public/emoji/latest/emoji-test.txt",
    "UnicodeData.txt": "https://unicode.org/Public/UCD/latest/ucd/UnicodeData.txt",
    "annotations.json": "https://raw.githubusercontent.com/unicode-org/cldr-json/main/"
    "cldr-json/cldr-annotations-full/annotations/en/annotations.json",
}

TONES = [0x1F3FB, 0x1F3FC, 0x1F3FD, 0x1F3FE, 0x1F3FF]
VS16 = 0xFE0F

# Categories nobody picks from a grid: controls, format characters,
# surrogates, private use, separators, and combining marks, which render as a
# dotted circle on their own.
SKIPPED_CATEGORIES = {"Cc", "Cf", "Cs", "Co", "Cn", "Zl", "Zp", "Zs", "Mn", "Me"}

# Blocks with thousands of near-identical entries whose names are only a code
# point ("CJK UNIFIED IDEOGRAPH-4E00"). They arrive as First/Last ranges in
# UnicodeData.txt and are skipped with them; these catch the few listed singly.
SKIPPED_NAME = re.compile(r"^(CJK COMPATIBILITY IDEOGRAPH|TANGUT COMPONENT|KHITAN SMALL SCRIPT|"
                          r"NUSHU CHARACTER|VARIATION SELECTOR|TAG )")


def fetch(directory: Path) -> None:
    directory.mkdir(parents=True, exist_ok=True)
    for name, url in SOURCES.items():
        path = directory / name
        if not path.exists():
            print(f"downloading {url}", file=sys.stderr)
            urllib.request.urlretrieve(url, path)


def clean(text: str) -> str:
    return " ".join(text.replace("\t", " ").split())


def load_keywords(path: Path) -> dict[str, list[str]]:
    annotations = json.loads(path.read_text())["annotations"]["annotations"]
    return {glyph: entry.get("default", []) for glyph, entry in annotations.items()}


def keywords_for(glyph: str, name: str, keywords: dict[str, list[str]]) -> str:
    words = keywords.get(glyph) or keywords.get(glyph.replace(chr(VS16), "")) or []
    name_words = set(name.lower().split())
    # Only what the name does not already say, to keep the table small.
    return clean(" ".join(word for word in words if word.lower() not in name_words))


def emoji(directory: Path, keywords: dict[str, list[str]]) -> list[str]:
    group = subgroup = ""
    qualified: set[str] = set()
    entries = []
    line_re = re.compile(r"^([0-9A-F ]+?)\s*;\s*([\w-]+)\s*#\s*\S+\s+E(\d+\.\d+)\s+(.*)$")

    for line in (directory / "emoji-test.txt").read_text().splitlines():
        if line.startswith("# group:"):
            group = line.split(":", 1)[1].strip()
            continue
        if line.startswith("# subgroup:"):
            subgroup = line.split(":", 1)[1].strip()
            continue
        match = line_re.match(line)
        if not match or match.group(2) != "fully-qualified":
            continue
        points = [int(point, 16) for point in match.group(1).split()]
        glyph = "".join(map(chr, points))
        qualified.add(glyph)
        if group == "Component" or any(point in TONES for point in points):
            continue
        entries.append((group, subgroup, glyph, match.group(3), match.group(4)))

    rows = []
    for group, subgroup, glyph, version, name in entries:
        rows.append("\t".join([
            group,
            subgroup,
            glyph,
            version,
            "1" if has_tones(glyph, qualified) else "0",
            clean(name),
            keywords_for(glyph, name, keywords),
        ]))
    return rows


def with_tone(glyph: str, tone: int) -> str:
    """The rule the picker applies: the modifier follows the first code point,
    replacing the emoji-presentation selector that may be there."""
    rest = glyph[1:]
    if rest.startswith(chr(VS16)):
        rest = rest[1:]
    return glyph[0] + chr(tone) + rest


def has_tones(glyph: str, qualified: set[str]) -> bool:
    return all(with_tone(glyph, tone) in qualified for tone in TONES)


def unicode(directory: Path, keywords: dict[str, list[str]]) -> list[str]:
    rows = []
    for line in (directory / "UnicodeData.txt").read_text().splitlines():
        fields = line.split(";")
        code, name, category = fields[0], fields[1], fields[2]
        if category in SKIPPED_CATEGORIES or name.startswith("<") or SKIPPED_NAME.match(name):
            continue
        glyph = chr(int(code, 16))
        rows.append("\t".join([code, name, keywords_for(glyph, name, keywords)]))
    return rows


def main() -> None:
    here = Path(__file__).resolve().parent
    directory = Path(sys.argv[1]) if len(sys.argv) > 1 else here / ".sources"
    fetch(directory)
    keywords = load_keywords(directory / "annotations.json")

    for name, rows in [("emoji.tsv", emoji(directory, keywords)),
                       ("unicode.tsv", unicode(directory, keywords))]:
        (here / name).write_text("\n".join(rows) + "\n")
        print(f"{name}: {len(rows)} entries", file=sys.stderr)


if __name__ == "__main__":
    main()
