#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-3.0-or-later

"""Build the bundled spell database for the damage-meter tooltips.

Fetches the fully hotfix-merged SpellName, Spell and SpellMisc tables from the
wago.tools db2-find API, joins them into a compact data/spells/spells.json
(name/description/icon per spell), and downloads + decodes the icon textures
(BLP1/BLP2 -> 24x24 PNG) into data/spells/icons/.

The API pages at 25 rows/page, so the table dumps are cached on disk and only
missing pages are refetched on a re-run. Everything generated under
data/spells/ is meant to be committed.

Usage:
    python3 scripts/fetch-spell-data.py [build]
    (build defaults to the latest retail build; pass e.g. 12.1.0.69382 to pin)
"""

import concurrent.futures
import json
import os
import re
import struct
import sys
import tempfile
import threading
import time
import urllib.request

import xml.etree.ElementTree as ET

from PIL import Image

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
SPELLS_DIR = os.path.join(ROOT, "data", "spells")
ICON_DIR = os.path.join(SPELLS_DIR, "icons")
GREL = os.path.join(ROOT, "data", "spells.gresource.xml")
SPELLS_PREFIX = "/io/github/JohanWes/WarcraftRustcorder/spells"
CACHE = os.environ.get("SPELLDB_CACHE", os.path.join(ROOT, "scripts", ".cache-spelldb"))

UA = {"User-Agent": "warcraft-rustcorder-spelldb/1.0 (spell database builder)"}
WORKERS = 48
ICON_SIZE = 24
ICON_COLORS = 64

# wago.tools API routes.
BUILDS_URL = "https://wago.tools/api/builds/wow/latest"
FILES_URL = "https://wago.tools/api/files?version={build}&format=json"
FIND_URL = "https://wago.tools/api/db2-find/{table}?version={build}&page={page}"
CASC_URL = "https://wago.tools/api/casc/{fdid}?version={build}"

# Table -> page count. A starting guess only: fetch_pages prefers the API's
# reported last page, so a stale value costs one extra probe instead of
# truncating the dump.
PAGES = {"SpellName": 16558, "Spell": 16558, "SpellMisc": 16702}

_LOCK = threading.Lock()


def http_bytes(url, tries=4):
    for attempt in range(tries):
        try:
            req = urllib.request.Request(url, headers=UA)
            with urllib.request.urlopen(req, timeout=60) as resp:
                return resp.read()
        except Exception:
            if attempt == tries - 1:
                raise
            time.sleep(1.5 * (attempt + 1))


def http_json(url, tries=4):
    return json.loads(http_bytes(url, tries))


def resolve_build(requested):
    if requested:
        return requested
    return http_json(BUILDS_URL)["version"]


def fetch_pages(table, build, total_pages):
    """Fetch every page of one table into CACHE, resuming from what exists."""
    os.makedirs(CACHE, exist_ok=True)
    # The API reports its own last page; prefer it over the static guess so
    # a table that has grown since PAGES was written never truncates the
    # dump silently.
    try:
        meta = http_json(FIND_URL.format(table=table, build=build, page=1))
        total_pages = max(total_pages, int(meta.get("last_page") or 0))
    except Exception:
        pass

    def fetch_page(page):
        out = os.path.join(CACHE, f"{table}.{page:06d}.jsonl")
        if os.path.exists(out) and os.path.getsize(out) > 0:
            return 0
        data = http_json(FIND_URL.format(table=table, build=build, page=page))
        rows = data.get("data", [])
        with open(out, "w") as fh:
            for row in rows:
                fh.write(json.dumps(row) + "\n")
        return len(rows)

    existing = len(
        [f for f in os.listdir(CACHE) if f.startswith(table + ".")]
    )
    print(f"{table}: {existing}/{total_pages} pages cached")
    with concurrent.futures.ThreadPoolExecutor(max_workers=WORKERS) as pool:
        futures = {pool.submit(fetch_page, p): p for p in range(1, total_pages + 1)}
        rows = 0
        for fut in concurrent.futures.as_completed(futures):
            rows += fut.result()
            if rows and rows % 50000 == 0:
                print(f"  {table} {rows} rows...")


def iter_rows(table):
    files = sorted(f for f in os.listdir(CACHE) if f.startswith(table + "."))
    for f in files:
        with open(os.path.join(CACHE, f)) as fh:
            for line in fh:
                line = line.strip()
                if line:
                    yield json.loads(line)


CONDITIONAL = re.compile(r"\$[?](?:(?!\$\?)[^\[\]])*\[[^\]]*\][^\[\]$]*\[[^\]]*\]")
CONDITIONAL_CHAIN = re.compile(r"(?:\$\?[^$\[\]]+\[[^\]]*\])+")
BRACKET = re.compile(r"\[([^\]]*)\]")
TOKEN = re.compile(r"\$[a-zA-Z0-9_]+")
MATH = re.compile(r"\$[*/]?[0-9]*;[a-zA-Z0-9_]+")
WRAPPER = re.compile(r"\|[a-zA-Z][0-9a-fA-F]{0,8}")
INLINE_ICON = re.compile(r"\|T[^|]*\|t")
LINK = re.compile(r"\|H[^|]*\|h[^|]*\|h")
EXPRESSION = re.compile(r"\$\{[^{}]*\}")
SCALE_VAR = re.compile(r"\$<[^>]*>")
SPELL_REF = re.compile(r"\$@[a-zA-Z]+\d*")
SPELL_DESC_REF = re.compile(r"\$@spelldesc(\d+)", re.IGNORECASE)
# Any leftover template syntax marks the text as unrenderable; junk stubs
# ("Stunned.", "fdsa") are dropped by the length floor.
LEFTOVER_SYNTAX = re.compile(r"[$|{}\[\]]")
MIN_DESC_LEN = 12

# A few current encounter damage records have no client description at all.
# These concise fallbacks come from their 12.1 Wowhead spell records; values
# remain X placeholders like the descriptions rendered from client templates.
# https://www.wowhead.com/spell=1269284/fel-steps
# https://www.wowhead.com/spell=1299133/ferocious-leap
# https://www.wowhead.com/spell=1298933/savage-smash
DESCRIPTION_FALLBACKS = {
    1269284: "Inflicts X Fire damage to enemies within X yards.",
    1299133: (
        "Inflicts X Physical damage to an enemy, then inflicts X Physical "
        "damage every X sec for X sec."
    ),
    1298933: (
        "Inflicts X Physical damage to enemies within X yards of the impact, "
        "then inflicts X Physical damage every X sec for X sec."
    ),
}


def clean_description(text):
    """Render a WoW tooltip description as static text.

    Value templates cannot be evaluated without a live game client, so every
    one becomes the placeholder X: `$?cond[a][b]` keeps its first branch,
    `${...}` groups (scale math) and `$<var>` markers collapse to X,
    `$@spelldesc123` references are dropped, `|n` becomes a space and other
    `|c...|r` wrappers plus inline `|T...|t` icons are stripped. Returns ""
    when the result would still show template debris or is a junk stub.
    """
    if not text:
        return ""
    text = text.replace("\r\n", "|n").replace("\n", "|n")
    text = INLINE_ICON.sub("", text)
    text = text.replace("|n", " ")
    # Expressions may nest: resolve innermost groups first, so
    # `${$?a[X][Y]}` collapses in one X instead of leaving debris.
    while True:
        stripped = EXPRESSION.sub("X", text)
        if stripped == text:
            break
        text = stripped
    # Salvage malformed data: an unterminated ${ or a stray }.
    text = text.replace("${", "").replace("}", "")
    text = SPELL_REF.sub("", text)
    while True:
        match = CONDITIONAL.search(text)
        if not match:
            break
        branch = BRACKET.search(match.group(0))
        text = text[: match.start()] + (branch.group(1) if branch else "") + text[match.end() :]
    # Difficulty alternatives can be encoded as adjacent one-branch
    # conditions (`$?diff1[a]$?diff8[b]`); retain the first readable branch.
    text = CONDITIONAL_CHAIN.sub(
        lambda match: (BRACKET.search(match.group(0)) or match).group(1),
        text,
    )
    # Glued conditional chains ($?c1[a]?c2[b][] or longer) resolve to their
    # first branch; any bracket groups left over are unresolvable chain
    # tails, dropped whole along with their bare condition markers (?a123456).
    text = text.replace("[]", "")
    text = re.sub(r"\[[^\]]*\]", "", text)
    text = re.sub(r"\?[a-z]\d+", "", text)
    # $l/$L<singular>:<plural>; picks the plural: values render as "X".
    text = re.sub(r"\$[lL][^:;]*:([^:;]*);", r"\1", text)
    text = SCALE_VAR.sub("X", text)
    text = MATH.sub("X", text)
    text = TOKEN.sub("X", text)
    text = LINK.sub("", text)
    text = WRAPPER.sub("", text)
    text = re.sub(r"\s+", " ", text).strip()
    if len(text) < MIN_DESC_LEN or LEFTOVER_SYNTAX.search(text):
        return ""
    return text


def resolve_description_refs(raw_by_id):
    """Expand `$@spelldesc<ID>` links before cleaning tooltip templates.

    Damage/aura records commonly delegate their text to the cast spell. The
    client tables keep that as a reference rather than copying the prose; if
    it is stripped as generic template syntax, otherwise valid encounter
    tooltips become empty. Cycles and missing targets resolve to an empty
    fragment instead of recursing forever.
    """
    resolved = {}

    def resolve(spell_id, visiting):
        if spell_id in resolved:
            return resolved[spell_id]
        if spell_id in visiting:
            return ""
        visiting.add(spell_id)
        text = raw_by_id.get(spell_id, "")
        text = SPELL_DESC_REF.sub(
            lambda match: resolve(int(match.group(1)), visiting),
            text,
        )
        visiting.remove(spell_id)
        resolved[spell_id] = text
        return text

    return {spell_id: resolve(spell_id, set()) for spell_id in raw_by_id}


# ---------------------------------------------------------------------------
# BLP1/BLP2 decoding (the WoW icon texture format; gdk-pixbuf cannot read it).


def _expand565(color):
    r = (color >> 11) & 0x1F
    g = (color >> 5) & 0x3F
    b = color & 0x1F
    return ((r << 3) | (r >> 2), (g << 2) | (g >> 4), (b << 3) | (b >> 2))


def _bc1_colors(data, off):
    c0, c1 = struct.unpack_from("<HH", data, off)
    r0, g0, b0 = _expand565(c0)
    r1, g1, b1 = _expand565(c1)
    if c0 > c1:
        return [
            (r0, g0, b0, 255),
            (r1, g1, b1, 255),
            ((2 * r0 + r1) // 3, (2 * g0 + g1) // 3, (2 * b0 + b1) // 3, 255),
            ((r0 + 2 * r1) // 3, (g0 + 2 * g1) // 3, (b0 + 2 * b1) // 3, 255),
        ]
    return [
        (r0, g0, b0, 255),
        (r1, g1, b1, 255),
        ((r0 + r1) // 2, (g0 + g1) // 2, (b0 + b1) // 2, 255),
        (0, 0, 0, 0),
    ]


def _bc23_colors(data, off):
    c0, c1 = struct.unpack_from("<HH", data, off)
    r0, g0, b0 = _expand565(c0)
    r1, g1, b1 = _expand565(c1)
    return [
        (r0, g0, b0, 255),
        (r1, g1, b1, 255),
        ((2 * r0 + r1) // 3, (2 * g0 + g1) // 3, (2 * b0 + b1) // 3, 255),
        ((r0 + 2 * r1) // 3, (g0 + 2 * g1) // 3, (b0 + 2 * b1) // 3, 255),
    ]


def _decode_block(data, off, fmt):
    # Block layouts: BC1 is [colors(4)][indices(4)]; BC2/BC3 are
    # [alpha(8)][colors(4)][indices(4)] — the indices always follow the
    # color endpoints, never the alpha block.
    if fmt == "bc1":
        indices = int.from_bytes(data[off + 4 : off + 8], "little")
        colors = _bc1_colors(data, off)
        return [colors[(indices >> (2 * i)) & 3] for i in range(16)]
    indices = int.from_bytes(data[off + 12 : off + 16], "little")
    colors = _bc23_colors(data, off + 8)
    if fmt == "bc2":
        alpha = data[off : off + 8]
        out = []
        for i in range(16):
            r, g, b, _ = colors[(indices >> (2 * i)) & 3]
            # 16 4-bit alphas, pixel i = nibble i (low nibble first);
            # expand 0..15 to 0..255 by nibble replication.
            nib = (alpha[i // 2] >> (4 * (i % 2))) & 0xF
            out.append((r, g, b, (nib << 4) | nib))
        return out
    # bc3
    a0, a1 = data[off], data[off + 1]
    alpha_idx = int.from_bytes(data[off + 2 : off + 8], "little")
    if a0 > a1:
        aramp = [
            a0,
            a1,
            (6 * a0 + 1 * a1) // 7,
            (5 * a0 + 2 * a1) // 7,
            (4 * a0 + 3 * a1) // 7,
            (3 * a0 + 4 * a1) // 7,
            (2 * a0 + 5 * a1) // 7,
            (1 * a0 + 6 * a1) // 7,
        ]
    else:
        aramp = [a0, a1, (4 * a0 + 1 * a1) // 5, (3 * a0 + 2 * a1) // 5,
                 (2 * a0 + 3 * a1) // 5, (1 * a0 + 4 * a1) // 5, 0, 255]
    out = []
    for i in range(16):
        r, g, b, _ = colors[(indices >> (2 * i)) & 3]
        a = aramp[(alpha_idx >> (3 * i)) & 7]
        out.append((r, g, b, a))
    return out


def decode_blp(data):
    if data[:4] not in (b"BLP1", b"BLP2"):
        raise ValueError("not a BLP file")
    encoding = data[8]
    alpha_depth = data[9]
    alpha_encoding = data[10]
    width, height = struct.unpack_from("<II", data, 12)
    offsets = struct.unpack_from("<16I", data, 20)
    sizes = struct.unpack_from("<16I", data, 84)
    raw = data[offsets[0] : offsets[0] + sizes[0]]

    if encoding == 3:  # raw BGRA
        pixels = bytearray()
        for i in range(0, len(raw), 4):
            pixels += bytes((raw[i + 2], raw[i + 1], raw[i], raw[i + 3]))
        return Image.frombytes("RGBA", (width, height), bytes(pixels))

    if encoding == 1:  # paletted
        palette = struct.unpack_from("<256I", data, 148)
        pixels = bytearray()
        for i in range(width * height):
            p = palette[raw[i]]
            a = 255
            if alpha_depth == 1:
                a = 255 if (raw[width * height + i // 8] >> (i % 8)) & 1 else 0
            elif alpha_depth == 4:
                nib = raw[width * height + i // 2]
                a = ((nib & 0x0F) << 4) if i % 2 == 0 else (nib & 0xF0)
            elif alpha_depth == 8:
                a = raw[width * height + i]
            pixels += bytes(((p >> 16) & 0xFF, (p >> 8) & 0xFF, p & 0xFF, a))
        return Image.frombytes("RGBA", (width, height), bytes(pixels))

    if encoding != 2:
        raise ValueError(f"unsupported BLP encoding {encoding}")

    if alpha_depth <= 1:
        fmt = "bc1"
    elif alpha_encoding == 7:
        fmt = "bc3"
    else:
        fmt = "bc2"
    bsize = 8 if fmt == "bc1" else 16
    bw, bh = (width + 3) // 4, (height + 3) // 4
    # Assemble row-major: frombytes reads the buffer as full image rows,
    # so each block's pixels must land at their (x, y), not be appended
    # block by block.
    buf = bytearray(width * height * 4)
    for by in range(bh):
        for bx in range(bw):
            block = _decode_block(raw, (by * bw + bx) * bsize, fmt)
            for py in range(4):
                y = by * 4 + py
                if y >= height:
                    break
                for px in range(4):
                    x = bx * 4 + px
                    if x < width:
                        i = (y * width + x) * 4
                        buf[i] = block[py * 4 + px][0]
                        buf[i + 1] = block[py * 4 + px][1]
                        buf[i + 2] = block[py * 4 + px][2]
                        buf[i + 3] = block[py * 4 + px][3]
    return Image.frombytes("RGBA", (width, height), bytes(buf))


# ---------------------------------------------------------------------------


def manifest(build):
    """fdid -> file path for one build, cached."""
    cache = os.path.join(CACHE, f"files.{build}.json")
    if os.path.exists(cache):
        with open(cache) as fh:
            return json.load(fh)
    data = http_json(FILES_URL.format(build=build))
    with open(cache, "w") as fh:
        json.dump(data, fh)
    return data


def icon_names_for(fdids, files):
    """Map FileDataID -> icon basename for the given fdids, from the manifest."""
    result = {}
    for fdid in fdids:
        path = files.get(str(fdid))
        if path and path.startswith("interface/icons/"):
            result[fdid] = os.path.basename(path)[: -len(".blp")]
    return result


def fetch_icons(fdids, build, files):
    """Download + decode each requested icon FileDataID once, into ICON_DIR."""
    os.makedirs(ICON_DIR, exist_ok=True)
    fdids = [f for f in set(fdids) if f]
    print(f"unique icons to fetch: {len(fdids)}")

    def write_icon(img, out):
        """Store a compact indexed PNG sized just above the 20 px row use."""
        img = img.convert("RGBA").resize((ICON_SIZE, ICON_SIZE), Image.LANCZOS)
        img = img.quantize(
            colors=ICON_COLORS,
            method=Image.Quantize.FASTOCTREE,
            dither=Image.Dither.NONE,
        )
        img.save(out, optimize=True)

    def fetch_one(fdid):
        path = files.get(str(fdid))
        if not path or not path.startswith("interface/icons/"):
            return None
        name = os.path.basename(path)[: -len(".blp")]
        out = os.path.join(ICON_DIR, name + ".png")
        if os.path.exists(out) and os.path.getsize(out) > 0:
            # Re-optimize existing assets when the size/palette policy changes
            # without downloading their source BLP again.
            with Image.open(out) as img:
                if img.size != (ICON_SIZE, ICON_SIZE) or img.mode != "P":
                    write_icon(img, out)
            return (fdid, name)
        try:
            blp = http_bytes(CASC_URL.format(fdid=fdid, build=build))
            img = decode_blp(blp)
        except Exception as exc:
            print(f"  skip icon {fdid} ({path}): {exc}")
            return None
        write_icon(img, out)
        return (fdid, name)

    results = {}
    with concurrent.futures.ThreadPoolExecutor(max_workers=WORKERS) as pool:
        for got in pool.map(fetch_one, fdids):
            if got:
                results[got[0]] = got[1]
    print(f"icons written: {len(results)}")
    return results


def spells_json(spell_id_to_name, spell_id_to_desc, spell_id_to_icon, icon_fdid_to_name):
    """Build a name-keyed map: {name: [description, icon_basename]}.

    Scope: every named spell with raw tooltip source text and an icon in the
    game manifest. Do not infer whether something is a spell from its icon name:
    current class abilities such as Voltaic Blaze, Deathstalker's Mark and
    Goremaw's Bite deliberately use `inv_` filenames. Rank variants share
    their base spell's entry; every distinct name is kept as a lookup key so
    meter rows resolve regardless of which spelling the log used. Where
    several spells share a name, the variant whose cleaned description
    survived best wins; a junk description still resolves the icon, just
    without tooltip text.
    """
    BASE_RANK = re.compile(r"\s*\([^)]*\)\s*$")

    by_name = {}
    for sid, name in spell_id_to_name.items():
        desc = spell_id_to_desc.get(sid)
        fdid = spell_id_to_icon.get(sid, 0)
        icon = icon_fdid_to_name.get(fdid)
        if not name or desc is None or not icon:
            continue
        base = BASE_RANK.sub("", name).strip()
        canonical = base == name
        # Prefer the no-rank spelling, the best surviving description, then
        # the newest spell id.
        prio = (0 if canonical else 1, -len(desc), -sid)
        cur = by_name.get(base)
        if cur is None or prio < cur[0]:
            by_name[base] = (prio, desc, icon)
        # Keep the rank spelling as an alias to the same entry.
        if not canonical:
            by_name.setdefault(name, (prio, desc, icon))

    entries = {name: [data[1], data[2]] for name, data in by_name.items()}
    return entries


def update_gresource():
    """Write data/spells.gresource.xml from spells.json plus the icons on disk.

    The file is regenerated whole, so renamed or dropped icons never linger.
    """
    root = ET.Element("gresources")
    root.append(
        ET.Comment(
            " Generated by scripts/fetch-spell-data.py: spell database and icons, "
            "installed as a separate file and mmapped at runtime. "
        )
    )
    resource = ET.SubElement(root, "gresource", prefix=SPELLS_PREFIX)
    ET.SubElement(resource, "file", alias="spells.json").text = "spells/spells.json"
    for f in sorted(os.listdir(ICON_DIR)):
        if f.endswith(".png"):
            ET.SubElement(resource, "file", alias=f).text = f"spells/icons/{f}"
    tree = ET.ElementTree(root)
    ET.indent(tree, space="  ")
    tree.write(GREL, encoding="utf-8", xml_declaration=True)


def main():
    build = resolve_build(sys.argv[1] if len(sys.argv) > 1 else None)
    print(f"build: {build}")
    for table in ("SpellName", "Spell", "SpellMisc"):
        fetch_pages(table, build, PAGES[table])

    print("joining tables...")
    spell_id_to_name = {}
    for row in iter_rows("SpellName"):
        name = row.get("Name_lang") or ""
        if name:
            spell_id_to_name[row["ID"]] = name

    # Keep every spell that has any raw description, even when the cleaned
    # text is junk: its icon still identifies the row, just without tooltip
    # text.
    raw_descriptions = {}
    for row in iter_rows("Spell"):
        raw = row.get("Description_lang") or ""
        if raw:
            raw_descriptions[row["ID"]] = raw
    spell_id_to_desc = {
        spell_id: clean_description(description)
        for spell_id, description in resolve_description_refs(raw_descriptions).items()
    }
    for spell_id, description in DESCRIPTION_FALLBACKS.items():
        if not spell_id_to_desc.get(spell_id):
            spell_id_to_desc[spell_id] = description

    spell_id_to_icon = {}
    for row in iter_rows("SpellMisc"):
        fdid = row.get("SpellIconFileDataID") or 0
        sid = row.get("SpellID") or 0
        if fdid and sid and sid not in spell_id_to_icon:
            spell_id_to_icon[sid] = fdid

    print(
        f"names={len(spell_id_to_name)} descriptions={len(spell_id_to_desc)} "
        f"icons={len(spell_id_to_icon)}"
    )

    files = manifest(build)
    icon_fdid_to_name = icon_names_for(set(spell_id_to_icon.values()), files)

    entries = spells_json(spell_id_to_name, spell_id_to_desc, spell_id_to_icon, icon_fdid_to_name)
    print(f"spells kept: {len(entries)}")

    name_to_fdid = {name: fdid for fdid, name in icon_fdid_to_name.items()}
    needed = {name_to_fdid[icon] for _, icon in entries.values() if icon in name_to_fdid}
    fetch_icons(needed, build, files)
    # Scope changes retire icons; drop anything no entry references anymore.
    referenced = {icon for _, icon in entries.values()}
    for f in os.listdir(ICON_DIR):
        if f.endswith(".png") and f[: -len(".png")] not in referenced:
            os.remove(os.path.join(ICON_DIR, f))

    os.makedirs(SPELLS_DIR, exist_ok=True)
    with open(os.path.join(SPELLS_DIR, "spells.json"), "w") as fh:
        json.dump(entries, fh, ensure_ascii=False, separators=(",", ":"))
    json_size = os.path.getsize(os.path.join(SPELLS_DIR, "spells.json"))
    icon_size = sum(
        os.path.getsize(os.path.join(ICON_DIR, f))
        for f in os.listdir(ICON_DIR)
        if f.endswith(".png")
    )
    print(f"spells.json: {json_size / 1e6:.2f} MB, icons: {icon_size / 1e6:.2f} MB")
    update_gresource()
    print("gresource synced")


if __name__ == "__main__":
    main()
