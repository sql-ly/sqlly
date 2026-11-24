#!/usr/bin/env python3
"""Rebuild the browser build's interface fonts from the upstream OFL Noto releases.

The browser has no system fonts, so every character the interface draws must
come from a font this directory provides. Two subsets are embedded in the wasm
binary for every visitor; three regional faces are fetched only when the
interface language needs them (see `apps/browser/src/fonts.rs`):

    NotoSans-Regular-subset.ttf     embedded  Latin, Greek, Cyrillic (Russian UI face)
    NotoSansSC-Regular-subset.ttf   embedded  default UI face: Latin + GB 2312 level 1
    NotoSansJP-Regular-subset.ttf   lazy (ja)     kana + JIS X 0208
    NotoSansKR-Regular-subset.ttf   lazy (ko)     all 11,172 Hangul syllables
    NotoSansTC-Regular-subset.ttf   lazy (zh-TW)  Big5 frequently used hanzi

Each subset also carries every character its languages' catalogs use (from
`crates/sqlly-i18n/translations/*.json`). Characters outside these sets (a rare
ideograph a translator types, CJK in query results) fall back to the
browser's own fonts through gpui's Canvas fallback.

Re-run after a catalog gains characters (the `font_coverage` test in
`apps/browser/tests` fails until you do), then re-graft the UI symbols:

    python3 build-fonts.py [--source <dir>]
    python3 build-symbols.py

`--source` names a directory holding the upstream files under their release
names (see SOURCES); without it they are downloaded from the pinned commits.
Requires `fonttools` (`pip install fonttools`).
"""

import io
import json
import sys
import urllib.request
from pathlib import Path

from fontTools.subset import Options, Subsetter
from fontTools.ttLib import TTFont
from fontTools.varLib import instancer

HERE = Path(__file__).resolve().parent
CATALOGS = HERE / "../../../crates/sqlly-i18n/translations"

GOOGLE_FONTS = "https://github.com/google/fonts/raw"
# Upstream release files. The CJK faces are the google/fonts variable builds
# (Noto Sans CJK 2.004), pinned to the commit that last touched each family
# and instanced at Regular (wght 400) here.
SOURCES = {
    "NotoSans-Regular.ttf": "https://github.com/notofonts/notofonts.github.io/raw/main/fonts/NotoSans/hinted/ttf/NotoSans-Regular.ttf",
    "NotoSansSC[wght].ttf": f"{GOOGLE_FONTS}/a85815a42757630ce188fdad368c2dfc444d4773/ofl/notosanssc/NotoSansSC%5Bwght%5D.ttf",
    "NotoSansJP[wght].ttf": f"{GOOGLE_FONTS}/66a36c8c94b1a5d992ee4e7f392fccfe4945767c/ofl/notosansjp/NotoSansJP%5Bwght%5D.ttf",
    "NotoSansKR[wght].ttf": f"{GOOGLE_FONTS}/b38c5c93af322c45f633e17ac440ec1e6c94d489/ofl/notosanskr/NotoSansKR%5Bwght%5D.ttf",
    "NotoSansTC[wght].ttf": f"{GOOGLE_FONTS}/3be1884c48c3e45b52ecc725676a08f87776373e/ofl/notosanstc/NotoSansTC%5Bwght%5D.ttf",
}

LATIN_CATALOGS = ["en", "de", "es", "fr", "pt-BR"]


def span(lo: int, hi: int) -> set[int]:
    return set(range(lo, hi + 1))


# Shared by every face: ASCII, Latin-1, Latin Extended-A, general punctuation,
# the symbol blocks from superscripts through dingbats (arrows, box drawing,
# geometric shapes, ...; whatever the upstream face draws natively), CJK
# symbols and punctuation, and the halfwidth/fullwidth forms.
COMMON = (
    span(0x20, 0x7E) | span(0xA0, 0x17F) | span(0x2000, 0x206F) | span(0x2070, 0x27BF)
    | span(0x3000, 0x303F) | span(0xFF00, 0xFFEF)
)


def encodable(codec: str, accept=lambda cp, raw: True) -> set[int]:
    """BMP characters a legacy double-byte charset encodes in one code."""
    found = set()
    for cp in range(0x80, 0x10000):
        try:
            raw = chr(cp).encode(codec)
        except UnicodeEncodeError:
            continue
        if len(raw) == 2 and accept(cp, raw):
            found.add(cp)
    return found


def catalog_chars(*languages: str) -> set[int]:
    found: set[int] = set()

    def walk(node):
        if isinstance(node, str):
            found.update(ord(ch) for ch in node)
        elif isinstance(node, dict):
            for value in node.values():
                walk(value)
        elif isinstance(node, list):
            for value in node:
                walk(value)

    for language in languages:
        walk(json.loads((CATALOGS / f"{language}.json").read_text(encoding="utf-8")))
    return {cp for cp in found if cp >= 0x20}


def gb2312_level1(cp: int, raw: bytes) -> bool:
    return 0xB0A1 <= int.from_bytes(raw, "big") <= 0xD7FE


def jis_x_0208(cp: int, raw: bytes) -> bool:
    return raw[0] >= 0xA1  # 0x8E introduces half-width katakana, not JIS X 0208


def big5_frequent(cp: int, raw: bytes) -> bool:
    return 0xA440 <= int.from_bytes(raw, "big") <= 0xC67E


def outputs() -> list[tuple[str, str, set[int]]]:
    """(output file, source file, characters) for every face this script writes."""
    return [
        (
            "NotoSans-Regular-subset.ttf",
            "NotoSans-Regular.ttf",
            COMMON | span(0x180, 0x24F) | span(0x370, 0x3FF) | span(0x400, 0x4FF)
            | catalog_chars("ru", *LATIN_CATALOGS),
        ),
        (
            "NotoSansSC-Regular-subset.ttf",
            "NotoSansSC[wght].ttf",
            COMMON | encodable("gb2312", gb2312_level1) | catalog_chars("zh-CN", *LATIN_CATALOGS),
        ),
        (
            "NotoSansJP-Regular-subset.ttf",
            "NotoSansJP[wght].ttf",
            COMMON | span(0x3040, 0x30FF) | encodable("euc_jp", jis_x_0208) | catalog_chars("ja"),
        ),
        (
            "NotoSansKR-Regular-subset.ttf",
            "NotoSansKR[wght].ttf",
            COMMON | span(0x3130, 0x318F) | span(0xAC00, 0xD7A3) | catalog_chars("ko"),
        ),
        (
            "NotoSansTC-Regular-subset.ttf",
            "NotoSansTC[wght].ttf",
            COMMON | span(0x3100, 0x312F) | encodable("big5", big5_frequent) | catalog_chars("zh-TW"),
        ),
    ]


def load(name: str, source_dir: Path | None) -> TTFont:
    if source_dir is not None:
        path = source_dir / name
        print(f"  reading {path}")
        return TTFont(str(path))
    url = SOURCES[name]
    print(f"  fetching {url}")
    with urllib.request.urlopen(url, timeout=300) as response:
        return TTFont(io.BytesIO(response.read()))


def regular(font: TTFont) -> TTFont:
    """Pin a variable face at Regular so fontdb matches it at weight 400."""
    if "fvar" not in font:
        return font
    font = instancer.instantiateVariableFont(font, {"wght": 400}, updateFontNames=True)
    font["OS/2"].usWeightClass = 400
    return font


def subset(font: TTFont, codepoints: set[int]) -> TTFont:
    options = Options()
    options.name_IDs = ["*"]
    options.name_languages = ["*"]
    options.notdef_outline = True
    options.recalc_bounds = True
    subsetter = Subsetter(options=options)
    subsetter.populate(unicodes=sorted(codepoints))
    subsetter.subset(font)
    return font


def main(source_dir: Path | None) -> None:
    for output, source, wanted in outputs():
        print(f"Building {output}...")
        font = subset(regular(load(source, source_dir)), wanted)
        covered = set(font.getBestCmap())
        needed = {cp for cp in wanted & catalog_chars(*languages_for(output)) if cp > 0x20}
        absent = sorted(needed - covered)
        if absent:
            print("  upstream lacks: " + "".join(chr(cp) for cp in absent))
        font.save(HERE / output)
        family = font["name"].getDebugName(1)
        size = (HERE / output).stat().st_size
        print(f"  {family!r}: {len(covered)} characters, {size:,} bytes")
    print("Done. Now run build-symbols.py to re-graft the UI symbols.")


def languages_for(output: str) -> list[str]:
    return {
        "NotoSans-Regular-subset.ttf": ["ru"],
        "NotoSansSC-Regular-subset.ttf": ["zh-CN", *LATIN_CATALOGS],
        "NotoSansJP-Regular-subset.ttf": ["ja"],
        "NotoSansKR-Regular-subset.ttf": ["ko"],
        "NotoSansTC-Regular-subset.ttf": ["zh-TW"],
    }[output]


if __name__ == "__main__":
    args = sys.argv[1:]
    source = None
    if args[:1] == ["--source"] and len(args) == 2:
        source = Path(args[1]).expanduser()
    elif args:
        sys.exit(f"usage: build-fonts.py [--source <dir>]  (got {args})")
    main(source)
