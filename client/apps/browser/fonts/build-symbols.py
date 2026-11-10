#!/usr/bin/env python3
"""Merge the symbol glyphs the SQLLY UI draws into the browser subset fonts.

The desktop app leans on the OS font stack to render the geometric shapes,
arrows and keyboard-modifier glyphs that pepper the UI (tree twisties `>`,
close crosses, plan warnings, keybinding hints, ...). The browser build has no
system fallback: only the fonts we embed are ever consulted, so any codepoint
missing from `NotoSans*-Regular-subset.ttf` renders as tofu (a `X` box). See
`SYMBOL_CODEPOINTS` below for the exact set the UI uses.

This script pulls those glyphs from the upstream OFL Noto fonts and merges them
into the two committed subsets in place, so the primary UI family ("Noto Sans
SC") and its Latin fallback ("Noto Sans") both cover every symbol directly --
no dependency on cosmic-text's per-glyph fallback order.

Re-run it whenever the UI starts drawing a new symbol codepoint (add it to
SYMBOL_CODEPOINTS first) or when the subset fonts are regenerated:

    python3 build-symbols.py

Requires `fonttools` (`pip install fonttools`). Donors are fetched from GitHub
unless local copies are given with `--donor <path>` (repeatable; tried before
the URLs), which also makes the script work offline:

    python3 build-symbols.py --donor ~/fonts/NotoSansSymbols2-Regular.ttf

`python3 build-symbols.py --check` fetches nothing: it scans the UI sources for
symbol codepoints (literal, or Rust `\\u{..}` escapes, in the General Punctuation
through Dingbats ranges) and fails if either subset font lacks one.
"""

import io
import re
import sys
import urllib.request
from pathlib import Path

from fontTools.merge import Merger
from fontTools.pens.recordingPen import DecomposingRecordingPen
from fontTools.pens.ttGlyphPen import TTGlyphPen
from fontTools.subset import Subsetter, Options
from fontTools.ttLib import TTFont

HERE = Path(__file__).resolve().parent

# Codepoints the SQLLY UI renders as literal text (not SVG icons). Keep this in
# sync with the non-ASCII characters used in `apps/gpui/src/**` UI strings.
SYMBOL_CODEPOINTS = [
    0x2013, 0x2014,                                      # en/em dash
    0x2019, 0x201C, 0x201D,                              # curly quotes
    0x2022, 0x2026, 0x2039, 0x203A,                      # bullet, ellipsis, single angle quotes
    0x2070, 0x2074, 0x2075, 0x2076, 0x2077, 0x2078, 0x2079,  # superscript digits
    0x20AC,                                              # euro
    0x2190, 0x2191, 0x2192, 0x2193, 0x2194,             # arrows
    0x21A9, 0x21BB, 0x21C4, 0x21C5, 0x21D2, 0x21E2,     # return / refresh / swap / implies
    0x21E7,                                              # shift key
    0x2212, 0x2248, 0x2260, 0x2261, 0x2264, 0x2265,     # math operators
    0x22EE, 0x22EF,                                      # vertical / midline ellipsis
    0x2303, 0x2318, 0x2325, 0x232B, 0x238B,             # control / command / option / delete / escape
    0x2500,                                              # box drawing horizontal
    0x25A1, 0x25A3, 0x25B6, 0x25B8, 0x25BD, 0x25BE,     # squares / triangles
    0x25C0, 0x25C2, 0x25C6, 0x25CF,                      # triangles / diamond / circle
    0x2605, 0x2606, 0x2637, 0x26A0,                      # stars / trigram / warning
    0x2713, 0x2715, 0x2717,                             # check / crosses
    0x29C9,                                              # joined squares
    0x270E,                                              # pencil (rename result set / card)
    0x2922,                                              # north-east/south-west arrow ("Pop out" dialogs)
    0x2315,                                              # telephone recorder (search-field magnifier)
    0x25B2, 0x25BC,                                      # up/down triangles (pane divider collapse strips)
    0x25CB,                                              # white circle (unselected radio)
    0x2502,                                              # box drawing vertical (status bar separator)
]

# Donor OFL Noto fonts, in priority order. Each needed codepoint is taken from
# the first donor that contains it.
NOTO_BASE = "https://github.com/notofonts/notofonts.github.io/raw/main/fonts"
DONORS = [
    f"{NOTO_BASE}/NotoSans/hinted/ttf/NotoSans-Regular.ttf",
    f"{NOTO_BASE}/NotoSansSymbols2/hinted/ttf/NotoSansSymbols2-Regular.ttf",
    f"{NOTO_BASE}/NotoSansSymbols/hinted/ttf/NotoSansSymbols-Regular.ttf",
    f"{NOTO_BASE}/NotoSansMath/hinted/ttf/NotoSansMath-Regular.ttf",
]

# Local donor files given on the command line (`--donor path`), tried first.
LOCAL_DONORS: list[str] = []

# Where the UI strings live, relative to this directory. `tests.rs` files and
# the FIM prompt tokens (model-facing text, never drawn) are skipped.
UI_SOURCE_ROOTS = [HERE / "../../gpui/src"]
UI_SOURCE_SKIP = {"tests.rs", "fim_completion.rs"}
# Literal characters in these ranges count as UI symbols; natural-language
# sample strings (accents, Greek, CJK) are left to the fonts' own coverage.
SYMBOL_RANGES = [(0x2000, 0x2BFF)]


def ui_symbol_codepoints() -> dict[int, str]:
    """Every symbol codepoint the UI sources draw, mapped to one location."""
    found: dict[int, str] = {}
    for root in UI_SOURCE_ROOTS:
        for path in sorted(root.resolve().rglob("*.rs")):
            if path.name in UI_SOURCE_SKIP or "tests" in path.parts:
                continue
            for lineno, line in enumerate(path.read_text(encoding="utf-8").splitlines(), 1):
                if line.lstrip().startswith("//"):
                    continue
                escaped = [int(m.group(1), 16) for m in re.finditer(r"\\u\{([0-9A-Fa-f]+)\}", line)]
                for cp in escaped + [ord(ch) for ch in line]:
                    if any(lo <= cp <= hi for lo, hi in SYMBOL_RANGES):
                        found.setdefault(cp, f"{path.name}:{lineno}")
    return found


def check() -> int:
    """Fail when a UI symbol is missing from either subset font."""
    cmaps = {t.name: set(TTFont(str(t)).getBestCmap().keys()) for t in TARGETS}
    missing = []
    for cp, where in sorted(ui_symbol_codepoints().items()):
        lacking = [name for name, cmap in cmaps.items() if cp not in cmap]
        if lacking:
            missing.append(f"  U+{cp:04X} {chr(cp)}  ({where}) missing from {', '.join(lacking)}")
    if missing:
        print("UI symbols missing from the browser subset fonts (add them to SYMBOL_CODEPOINTS and rebuild):")
        print("\n".join(missing))
        return 1
    print("every UI symbol is covered by both subset fonts")
    return 0


TARGETS = [
    HERE / "NotoSans-Regular-subset.ttf",
    HERE / "NotoSansSC-Regular-subset.ttf",
]


def fetch(url: str) -> TTFont:
    local = Path(url).expanduser()
    if local.exists():
        print(f"  reading {local}")
        return TTFont(str(local))
    print(f"  fetching {url}")
    with urllib.request.urlopen(url, timeout=90) as resp:
        return TTFont(io.BytesIO(resp.read()))


# Tables the merge needs; everything else (MATH, GSUB/GPOS/GDEF, BASE, colour
# tables, ...) is stripped from donors so the Merger can't choke on them.
KEEP_TABLES = {
    "glyf", "loca", "cmap", "head", "hhea", "hmtx", "maxp",
    "name", "OS/2", "post", "cvt ", "fpgm", "prep", "gasp",
}


def subset_donor(font: TTFont, codepoints: list[int]) -> TTFont:
    """Return a copy of `font` reduced to just `codepoints`, with layout and
    exotic tables dropped so the merge can't drag in conflicting data."""
    opts = Options()
    opts.layout_features = []
    opts.name_IDs = []
    opts.notdef_outline = True
    opts.recalc_bounds = True
    ss = Subsetter(options=opts)
    ss.populate(unicodes=codepoints)
    ss.subset(font)
    for tag in [t for t in font.keys() if t not in KEEP_TABLES]:
        del font[tag]
    return font


def build_symbol_font() -> bytes:
    """Assemble one font holding every needed symbol, taken from the donors in
    priority order (disjoint codepoint assignment -> no cmap conflicts)."""
    remaining = list(SYMBOL_CODEPOINTS)
    donor_subsets: list[TTFont] = []
    for url in LOCAL_DONORS + DONORS:
        if not remaining:
            break
        try:
            donor = fetch(url)
        except OSError as err:
            print(f"    skipping {url}: {err}")
            continue
        have = set(donor.getBestCmap().keys())
        take = [c for c in remaining if c in have]
        if not take:
            continue
        print(f"    taking {len(take)} glyphs: " + " ".join("%04X" % c for c in take))
        donor_subsets.append(subset_donor(donor, take))
        remaining = [c for c in remaining if c not in have]
    if remaining:
        raise SystemExit(
            "no donor covers: " + " ".join("%04X" % c for c in remaining)
        )

    # Merge the per-donor subsets into a single symbol font.
    tmp = []
    for i, f in enumerate(donor_subsets):
        p = HERE / f"._symdonor{i}.ttf"
        f.save(p)
        tmp.append(p)
    merged_path = HERE / "._symbols.ttf"
    Merger().merge([str(p) for p in tmp]).save(merged_path)
    data = merged_path.read_bytes()
    for p in tmp + [merged_path]:
        p.unlink()
    return data


def graft_glyphs(target: TTFont, symbols: TTFont) -> None:
    """Copy every symbol glyph into `target` in place.

    Composites are decomposed to simple contours so we never have to reconcile
    component glyph names between the two fonts -- this sidesteps fontTools'
    whole-table Merger, which chokes on the CJK subset's tables."""
    sym_cmap = symbols.getBestCmap()
    sym_glyphset = symbols.getGlyphSet()
    tgt_glyf = target["glyf"]
    tgt_hmtx = target["hmtx"]
    tgt_cmap = target["cmap"]
    tgt_covered = target.getBestCmap()
    existing = set(target.getGlyphOrder())

    for cp, sym_name in sorted(sym_cmap.items()):
        if cp in tgt_covered:
            continue  # target already covers it; leave its own glyph
        new_name = "uni%04X" % cp
        while new_name in existing:
            new_name += "_sym"
        # Decompose the source glyph into simple contours so we never depend on
        # component glyph names lining up between the two fonts.
        pen = DecomposingRecordingPen(sym_glyphset)
        sym_glyphset[sym_name].draw(pen)
        tt_pen = TTGlyphPen(None)
        pen.replay(tt_pen)
        # `glyf[name] = ...` also appends `name` to the glyf table's glyph
        # order; do not track order ourselves or it double-counts.
        tgt_glyf[new_name] = tt_pen.glyph()
        width, lsb = symbols["hmtx"][sym_name]
        tgt_hmtx[new_name] = (width, lsb)
        # Some subsets (the CJK one) carry a `vmtx` table; every glyph must
        # have an entry or the table's count no longer matches numGlyphs and
        # the font is corrupt. Vertical metrics are irrelevant to the app's
        # horizontal layout, so a unit-em default advance is fine.
        if "vmtx" in target:
            upem = target["head"].unitsPerEm
            target["vmtx"][new_name] = (upem, 0)
        existing.add(new_name)
        for sub in tgt_cmap.tables:
            if sub.isUnicode():
                sub.cmap[cp] = new_name

    # Adopt the glyf table's now-extended order as the font's glyph order.
    target.setGlyphOrder(tgt_glyf.glyphOrder)


def main() -> None:
    print("Building symbol donor font...")
    symbol_bytes = build_symbol_font()
    symbol_path = HERE / "._symbols_merged.ttf"
    symbol_path.write_bytes(symbol_bytes)

    try:
        symbols = TTFont(symbol_path)
        for target in TARGETS:
            print(f"Grafting symbols into {target.name}...")
            font = TTFont(target)
            before = set(font.getBestCmap().keys())
            graft_glyphs(font, symbols)
            after = set(font.getBestCmap().keys())
            # Sanity: latin coverage survived and every symbol landed.
            assert font.getBestCmap().get(ord("m")), "lost 'm'!"
            missing = [c for c in SYMBOL_CODEPOINTS if c not in after]
            assert not missing, "missing " + " ".join("%04X" % c for c in missing)
            font.save(target)
            # Fully decompile the saved file so a malformed per-glyph table
            # (e.g. a short vmtx) fails here, not in the browser.
            verify = TTFont(target)
            for tag in verify.keys():
                _ = verify[tag]
            verify.getGlyphSet()
            print(f"  {len(before)} -> {len(after)} codepoints")
    finally:
        symbol_path.unlink(missing_ok=True)

    print("Done.")


if __name__ == "__main__":
    args = sys.argv[1:]
    while "--donor" in args:
        i = args.index("--donor")
        LOCAL_DONORS.append(args[i + 1])
        del args[i:i + 2]
    if args == ["--check"]:
        sys.exit(check())
    if args:
        sys.exit(f"usage: build-symbols.py [--donor <font.ttf>]... | --check  (got {args})")
    main()
