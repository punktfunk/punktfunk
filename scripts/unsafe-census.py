#!/usr/bin/env python3
"""
Punktfunk unsafe census.

Reproducible, compiler-free census of `unsafe` in every crate a shipped binary links.
Classifies by WHAT THE UNSAFE DOES (operations), not by block count.

Usage:  python3 unsafe-census.py <repo-root> [--json | --baseline <path> | --write-baseline <path>]
        python3 unsafe-census.py --self-test

`--baseline` is the CI ratchet: per crate, the shipped non-FFI operation count must EQUAL the
committed file. A rise fails; so does a baseline left above today's count, because that slack is
where a later rise hides. `--write-baseline` regenerates the file.

Method:
  0. Take the crate scope from `cargo metadata` (see scope_roots), so a new crate is counted.
  1. Strip line/block comments and string/char literals (a hand-rolled scanner).
  2. Resolve each crate's module tree from lib.rs/main.rs, propagating `#[cfg(..)]`
     from `mod x;` declarations down to files, so cfg attribution is structural
     rather than path-name guessing.
  3. Find every `unsafe` token; classify its FORM (fn / impl / trait / extern / block).
  4. For every unsafe REGION (a `unsafe {}` block body, or the whole body of an
     `unsafe fn` in a file carrying `#![allow(unsafe_op_in_unsafe_fn)]`), count
     unsafe OPERATIONS by regex, deduped by source span so nested blocks don't
     double count.
"""
import os, re, sys, json, argparse, collections, subprocess

# ---------------------------------------------------------------- scope

sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), "ci"))
from cargo_graph import closure  # noqa: E402

# The scope is every local crate these shipped binaries link on any target with every feature
# on (no dev-dependencies), plus the cdylibs Windows loads into other processes and the crates
# that build in their own workspaces. `cargo metadata` resolves a target it cannot build for,
# so the Apple and Android closures need no installed toolchain.
SHIPPED = ["punktfunk-host", "punktfunk-encode-worker", "punktfunk-capture-worker",
           "punktfunk-seats", "punktfunk-tray", "pf-seat-audio",
           "punktfunk-cli", "punktfunk-client-linux", "punktfunk-client-windows",
           "punktfunk-client-android", "punktfunk-client-apple", "punktfunk-ffi"]
TRIPLES = ["x86_64-unknown-linux-gnu", "x86_64-pc-windows-msvc", "aarch64-apple-darwin",
           "aarch64-apple-ios", "aarch64-linux-android"]
EXTRA_ROOTS = ["packaging/windows/drivers", "packaging/windows/pf-vkhdr-layer",
               "crates/host/pf-seat-keeper"]


def scope_roots():
    """Crate directories in scope, relative to the repo root (the cwd)."""
    dirs = set()
    for triple in TRIPLES:
        meta = json.loads(subprocess.check_output(
            ["cargo", "metadata", "--format-version", "1", "--locked", "--all-features",
             "--filter-platform", triple], text=True))
        ids = closure(meta, SHIPPED, dev=False)
        dirs |= {os.path.relpath(os.path.dirname(p["manifest_path"])).replace(os.sep, "/")
                 for p in meta["packages"] if p["id"] in ids and p["source"] is None}
    # A crate vendored inside another crate's directory is walked with its parent.
    nested = {d for d in dirs if any(d.startswith(o + "/") for o in dirs)}
    return sorted(dirs - nested) + EXTRA_ROOTS

EXCLUDE_DIR_PARTS = {"target", ".claude", "node_modules", ".git"}

# ---------------------------------------------------------------- lexing

def strip_noise(src: str) -> str:
    """Replace comments and string/char literal contents with spaces, preserving offsets."""
    out = list(src); i = 0; n = len(src)
    while i < n:
        c = src[i]
        if c == '/' and i + 1 < n and src[i+1] == '/':
            j = src.find('\n', i)
            j = n if j < 0 else j
            for k in range(i, j): out[k] = ' '
            i = j
        elif c == '/' and i + 1 < n and src[i+1] == '*':
            depth = 1; j = i + 2
            while j < n and depth:
                if src[j] == '/' and j+1 < n and src[j+1] == '*': depth += 1; j += 2
                elif src[j] == '*' and j+1 < n and src[j+1] == '/': depth -= 1; j += 2
                else: j += 1
            for k in range(i, min(j, n)):
                if out[k] != '\n': out[k] = ' '
            i = j
        elif c == 'r' and i + 1 < n and src[i+1] in '#"':
            j = i + 1; hashes = 0
            while j < n and src[j] == '#': hashes += 1; j += 1
            if j < n and src[j] == '"':
                term = '"' + '#' * hashes
                e = src.find(term, j + 1)
                e = n if e < 0 else e + len(term)
                for k in range(i, e):
                    if out[k] != '\n': out[k] = ' '
                i = e
            else:
                i += 1
        elif c == '"':
            j = i + 1
            while j < n:
                if src[j] == '\\': j += 2; continue
                if src[j] == '"': j += 1; break
                j += 1
            for k in range(i, min(j, n)):
                if out[k] != '\n': out[k] = ' '
            i = j
        elif c == "'":
            # char literal vs lifetime: a char lit is 'x' or '\n' — short and closed
            m = re.match(r"'(\\.|[^\\'])'", src[i:i+6])
            if m:
                for k in range(i, i + m.end()): out[k] = ' '
                i += m.end()
            else:
                i += 1
        else:
            i += 1
    return ''.join(out)

def strip_comments_only(src: str) -> str:
    out = list(src); i = 0; n = len(src)
    while i < n:
        if src[i] == '/' and i+1 < n and src[i+1] == '/':
            j = src.find('\n', i); j = n if j < 0 else j
            for k in range(i, j): out[k] = ' '
            i = j
        elif src[i] == '/' and i+1 < n and src[i+1] == '*':
            depth = 1; j = i+2
            while j < n and depth:
                if src[j] == '/' and j+1 < n and src[j+1] == '*': depth += 1; j += 2
                elif src[j] == '*' and j+1 < n and src[j+1] == '/': depth -= 1; j += 2
                else: j += 1
            for k in range(i, min(j, n)):
                if out[k] != '\n': out[k] = ' '
            i = j
        else:
            i += 1
    return ''.join(out)

def match_brace(s: str, open_idx: int):
    """open_idx points at '{'. Return index just past matching '}'."""
    depth = 0; i = open_idx; n = len(s)
    while i < n:
        if s[i] == '{': depth += 1
        elif s[i] == '}':
            depth -= 1
            if depth == 0: return i + 1
        i += 1
    return n

# ---------------------------------------------------------------- module tree / cfg

MOD_RE = re.compile(
    r'((?:#\s*\[[^\]]*\]\s*)*)'          # attributes
    r'(?:pub(?:\s*\([^)]*\))?\s+)?'      # visibility
    r'mod\s+([A-Za-z_][A-Za-z0-9_]*)\s*;'
)
CFG_RE = re.compile(r'#\s*\[\s*cfg\s*\(')

def extract_cfgs(attr_blob: str):
    """Return list of raw cfg predicate strings from an attribute blob."""
    res = []
    for m in CFG_RE.finditer(attr_blob):
        start = attr_blob.index('(', m.end() - 1)
        depth = 0; i = start
        while i < len(attr_blob):
            if attr_blob[i] == '(': depth += 1
            elif attr_blob[i] == ')':
                depth -= 1
                if depth == 0: break
            i += 1
        res.append(attr_blob[start+1:i])
    return res

FEATURES = ["nvenc", "qsv", "vulkan-encode", "pyrowave"]

def classify_cfg(preds):
    """Return (platform, features) from a list of cfg predicate strings."""
    blob = " ".join(preds)
    plat = None
    if re.search(r'\bwindows\b|target_os\s*=\s*"windows"', blob): plat = "windows"
    if re.search(r'target_os\s*=\s*"linux"', blob):
        plat = "linux" if plat is None else "multi"
    if re.search(r'target_os\s*=\s*"(macos|ios|tvos)"|\btarget_vendor\s*=\s*"apple"', blob):
        plat = "apple" if plat is None else "multi"
    feats = set()
    for f in FEATURES:
        if re.search(r'feature\s*=\s*"%s"' % re.escape(f), blob): feats.add(f)
    return plat, feats

def build_module_map(crate_src_root, entry_files):
    """Walk mod declarations from entry files; return {abspath: (platform, frozenset(features))}."""
    result = {}
    queue = []
    for e in entry_files:
        if os.path.exists(e): queue.append((e, None, frozenset()))
    seen = set()
    while queue:
        path, plat, feats = queue.pop()
        if path in seen: continue
        seen.add(path)
        result[path] = (plat, feats)
        try:
            src = strip_comments_only(open(path, encoding='utf-8', errors='replace').read())
        except OSError:
            continue
        d = os.path.dirname(path)
        stem = os.path.splitext(os.path.basename(path))[0]
        # child modules live next to lib.rs/main.rs/mod.rs, else in a dir named after the file
        childdir = d if stem in ("lib", "main", "mod") else os.path.join(d, stem)
        for m in MOD_RE.finditer(src):
            attrs, name = m.group(1), m.group(2)
            cp, cf = classify_cfg(extract_cfgs(attrs))
            nplat = plat if cp is None else (cp if plat is None or plat == cp else "multi")
            nfeat = feats | cf
            pm = re.search(r'#\s*\[\s*path\s*=\s*"([^"]+)"', attrs)
            cands = []
            if pm:
                cands.append(os.path.normpath(os.path.join(d if stem in ("lib","main","mod") else childdir, pm.group(1))))
                cands.append(os.path.normpath(os.path.join(d, pm.group(1))))
            cands += [os.path.join(childdir, name + ".rs"),
                      os.path.join(childdir, name, "mod.rs")]
            for cand in cands:
                if os.path.exists(cand):
                    queue.append((cand, nplat, nfeat))
                    break
    return result

# ---------------------------------------------------------------- operation classification

OPS = [
    # (category, regex)
    ("transmute",        re.compile(r'\btransmute(?:_copy)?\s*(?:::\s*<)?')),
    ("slice_from_raw",   re.compile(r'\bfrom_raw_parts(?:_mut)?\s*\(')),
    ("ptr_read_write",   re.compile(r'\b(?:ptr\s*::\s*)?(?:read|write)(?:_volatile|_unaligned)?\s*\(|\bcopy_nonoverlapping\s*\(|\bptr\s*::\s*copy\s*\(')),


    ("from_raw_owning",  re.compile(r'\b(?:Box|Arc|Rc|CString|OwnedHandle|OwnedFd|File|Weak)\s*::\s*from_raw\w*\s*\(|\bfrom_raw_(?:fd|handle|socket)\s*\(')),
    ("cstr_from_ptr",    re.compile(r'\bC(?:Str|String)\s*::\s*from_ptr\s*\(')),
    ("assume_init",      re.compile(r'\bassume_init\w*\s*\(')),
    ("set_len",          re.compile(r'\.\s*set_len\s*\(')),
    ("get_unchecked",    re.compile(r'\bget_unchecked\w*\s*\(')),
    ("static_mut",       re.compile(r'\bstatic\s+mut\b')),
    ("union_field",      re.compile(r'\.\s*Anonymous\b|\.\s*u\s*\.\s*\w')),
    ("env_set_var",      re.compile(r'\benv\s*::\s*(?:set_var|remove_var)\s*\(')),
]

# Third-party FFI namespaces seen in this tree. A call whose callee path is rooted
# in one of these, or a bare SCREAMING/PascalCase Win32 name, counts as an FFI call.
FFI_ROOTS = r'(?:libc|ash|vk|sys|ffi|gl|egl|cuda|cu|nvenc|amf|vpl|mfx|pw|spa|drm|av|ffmpeg|windows|Foundation|wdk|nt|ntdef|wdf|iddcx|hid|usb|xkb|opus|pyrowave)'
FFI_CALL = re.compile(
    r'\b(?:'
    r'(?:self\s*\.\s*\w+\s*\.\s*)?' + FFI_ROOTS + r'\s*(?:::|\.)\s*\w+\s*\('   # ns::fn( / ns.fn(
    r'|(?:[A-Z][a-z0-9]+){2,}\w*\s*\('                                        # PascalCase Win32
    r'|[A-Z][A-Z0-9_]{3,}\s*\('                                               # SCREAMING_CASE
    r'|(?:cu|vk|egl|gl|av|Wdf|Idd|Hid|Nv|mfx|amf|pw_|spa_|opus_|xkb_)\w+\s*\(' # api prefixes
    r')'
)
# a fn-pointer call through a dlopen'd symbol table: (self.api.foo)(..) / (f.bar)(..)
FNPTR_CALL = re.compile(r'\(\s*(?:self\s*\.\s*)?(?:\w+\s*\.\s*)+\w+\s*\)\s*\(')

# A name this file binds to a `windows::core::Owned<T>`: `let h = Owned::new(..)`, a `h: Owned<T>`
# field or a `h: &Owned<T>` parameter. `*h` on one is `Deref`, a safe call that copies the handle
# out; the FFI call that takes it is the unsafe operation, and it is counted as one.
OWNED_NAME = re.compile(
    r'\blet\s+(?:mut\s+)?(\w+)\s*=\s*(?:unsafe\s*\{\s*)?(?:\w+\s*::\s*)*Owned\s*::\s*new\s*\('
    r'|\b(\w+)\s*:\s*&?\s*(?:\w+\s*::\s*)*Owned\s*<')

def owned_names(src):
    return frozenset(a or b for a, b in OWNED_NAME.findall(src))

class _DerefRx:
    """`*expr` in DEREF position (not multiplication, not a `*mut`/`*const` type)."""
    STAR = re.compile(r'\*')
    OPERAND = re.compile(r'\*\s*(?:self\s*\.\s*)?(\w+)\b(?!\s*(?:\.|\(|\[|::))')
    def finditer(self, body, owned=frozenset()):
        n = len(body)
        for m in self.STAR.finditer(body):
            i = m.start()
            # a raw-pointer TYPE, not a deref
            if re.match(r'\*\s*(?:mut|const)\b', body[i:i+10]):
                continue
            # look back over whitespace to the previous significant char
            j = i - 1
            while j >= 0 and body[j] in ' \t\r\n':
                j -= 1
            prev = body[j] if j >= 0 else '{'
            # identifier / literal / closing bracket before `*` => multiplication
            if prev.isalnum() or prev in '_)]"\'':
                continue
            # `**` chain: the inner star already handled
            if prev == '*':
                continue
            # what follows must start an expression
            if not re.match(r'\*\s*(?:\(|\*|[A-Za-z_])', body[i:i+6]):
                continue
            operand = self.OPERAND.match(body, i)
            if operand and operand.group(1) in owned:
                continue
            yield type('M', (), {'start': lambda s, i=i: i, 'end': lambda s, i=i: i+1})()

DEREF_RX = _DerefRx()

# `x.as_ref()` / `x.as_mut()` as the WHOLE of a tiny unsafe block == ptr::as_ref (the raw-pointer
# one). `Option::as_ref` deeper inside a block is safe and must not be counted.
PTR_AS_REF = re.compile(r'^\s*(?:&\s*)?[\w.]+\s*\.\s*as_(?:ref|mut)\s*\(\s*\)\s*$')

def count_ops(body: str, owned=frozenset()):
    """Return Counter of unsafe operations in a region body (already noise-stripped). `owned`
    is the file's `Owned<T>` names, whose `*name` is not a raw deref."""
    c = collections.Counter()
    spans = []
    def take(cat, rx):
        for m in (rx.finditer(body, owned) if rx is DEREF_RX else rx.finditer(body)):
            if any(s <= m.start() < e for s, e in spans): continue
            spans.append((m.start(), m.end()))
            c[cat] += 1
    # order matters: specific before generic
    for cat, rx in OPS:
        take(cat, rx)
    take("ptr_deref", DEREF_RX)
    take("ptr_as_ref", PTR_AS_REF)
    take("ffi_call_fnptr", FNPTR_CALL)
    take("ffi_call", FFI_CALL)
    return c

# ---------------------------------------------------------------- per-file census

UNSAFE_TOK = re.compile(r'\bunsafe\b')
FENCE_ALLOW = re.compile(r'#!\s*\[\s*allow\s*\(\s*unsafe_op_in_unsafe_fn')
FORBID = re.compile(r'#!\s*\[\s*forbid\s*\(\s*unsafe_code')
REPRC = re.compile(r'#\s*\[\s*repr\s*\(\s*C')
# NOTE: the `const _: () = { ... };` BLOCK form is as common in this tree as the direct
# `const _: () = assert!(...)` form. A regex matching only the direct form counts files that
# guard their layout through a block as defenceless. Keep both alternatives.
LAYOUT_ASSERT = re.compile(
    r'const\s+_\s*:\s*\(\)\s*=\s*(?:assert!|\{)'
    r'|assert_eq!\s*\(\s*(?:std\s*::\s*mem\s*::\s*)?size_of'
    r'|offset_of!'
    r'|assert!\s*\(\s*(?:std\s*::\s*mem\s*::\s*)?size_of'
)
SAFETY_COMMENT = re.compile(r'//\s*SAFETY|//!\s*SAFETY|/\*\s*SAFETY')

TESTMOD = re.compile(r'#\s*\[\s*cfg\s*\(\s*test\s*\)\s*\]\s*(?:pub\s+)?mod\s+\w+\s*\{')

def test_spans(src):
    out = []
    for m in TESTMOD.finditer(src):
        ob = src.index('{', m.end() - 1)
        out.append((ob, match_brace(src, ob)))
    return out

# ⚠ The cache MUST hold a strong reference to `src`. Keying on `id(src)` alone is a real bug:
# CPython recycles object addresses, so once the previous file's source string is collected a
# NEW file's string can be allocated at the SAME address and silently hit the stale entry,
# which makes `test_ops` — and therefore the shipped-code metric — vary run to run. Storing the
# string beside its spans keeps the address un-recyclable for exactly as long as the entry is
# live, which is what makes `id()` a sound key here.
_TS_CACHE = {}
def in_test_mod(src, idx):
    key = id(src)
    hit = _TS_CACHE.get(key)
    if hit is None or hit[0] is not src:
        _TS_CACHE.clear()
        hit = (src, test_spans(src))
        _TS_CACHE[key] = hit
    return any(a <= idx < b for a, b in hit[1])

# --- raw handles not owned by a Drop type -------------------------------------------------
STRUCT_RE = re.compile(r'\bstruct\s+([A-Za-z_]\w*)(?:<[^>{;]*>)?\s*(\{|\()')
RAWFIELD = re.compile(r':\s*(?:\*\s*(?:mut|const)\s|HANDLE\b|HWND\b|HGLOBAL\b|HDC\b|HMODULE\b|HHOOK\b|HBITMAP\b|RawFd\b|RawHandle\b|c_int\b)')
DROP_RE = re.compile(r'\bimpl\s*(?:<[^>]*>)?\s*Drop\s+for\s+([A-Za-z_]\w*)')

def handle_ownership(src):
    """Rust-side OWNER types holding a raw handle. `#[repr(C)]` mirrors are plain FFI data,
    not owners, so they are excluded — they belong to the repr(C)-mirror metric instead."""
    drops = set(DROP_RE.findall(src))
    owned, unowned = [], []
    for m in STRUCT_RE.finditer(src):
        name, opener = m.group(1), m.group(2)
        if opener != '{':
            continue
        head = src[max(0, m.start() - 260):m.start()]
        if re.search(r'#\s*\[\s*repr\s*\(\s*C', head.split('struct')[-1] if 'struct' in head else head):
            continue
        ob = src.index('{', m.end() - 1)
        body = src[ob:match_brace(src, ob)]
        if RAWFIELD.search(body):
            (owned if name in drops else unowned).append(name)
    return owned, unowned

def census_file(path, plat, feats):
    raw = open(path, encoding='utf-8', errors='replace').read()
    src = strip_noise(raw)
    rec = {
        "path": path, "platform": plat, "features": sorted(feats),
        "forms": collections.Counter(),
        "ops": collections.Counter(),
        "fence_allow": bool(FENCE_ALLOW.search(raw)),
        "forbid": bool(FORBID.search(raw)),
        "repr_c": len(REPRC.findall(raw)),
        "layout_asserts": len(LAYOUT_ASSERT.findall(raw)),
        "safety_comments": len(SAFETY_COMMENT.findall(raw)),
        "handles_owned": [], "handles_unowned": [],
        "regions": [],
        "test_blocks": 0, "test_ops": collections.Counter(),
    }
    rec["handles_owned"], rec["handles_unowned"] = handle_ownership(src)
    if not UNSAFE_TOK.search(src):
        return rec
    covered = []  # spans of already-counted region bodies
    owned = owned_names(src)
    for m in UNSAFE_TOK.finditer(src):
        i = m.end()
        rest = src[i:i+40]
        if re.match(r'\s*fn\b', rest):
            rec["forms"]["unsafe_fn"] += 1
            continue
        if re.match(r'\s*impl\b', rest):
            # Send / Sync / other
            tail = src[i:i+200]
            if re.search(r'\bSend\b', tail): rec["forms"]["unsafe_impl_send"] += 1
            elif re.search(r'\bSync\b', tail): rec["forms"]["unsafe_impl_sync"] += 1
            else: rec["forms"]["unsafe_impl_other"] += 1
            continue
        if re.match(r'\s*trait\b', rest):
            rec["forms"]["unsafe_trait"] += 1; continue
        if re.match(r'\s*extern\b', rest):
            rec["forms"]["unsafe_extern_block"] += 1; continue
        if re.match(r'\s*\{', rest):
            ob = src.index('{', i)
            end = match_brace(src, ob)
            rec["forms"]["unsafe_block"] += 1
            _is_test = in_test_mod(src, ob)
            if _is_test: rec["test_blocks"] += 1
            if any(s <= ob < e for s, e in covered):
                rec["forms"]["unsafe_block_nested"] += 1
                continue
            covered.append((ob, end))
            body = src[ob+1:end-1]
            ops = count_ops(body, owned)
            rec["ops"].update(ops)
            if _is_test: rec["test_ops"].update(ops)
            rec["regions"].append({"line": src[:ob].count('\n')+1,
                                   "ops": dict(ops), "len": end-ob})
            continue
        rec["forms"]["unsafe_other"] += 1
    # unsafe-fn bodies in fenced files: whole body is an implicit unsafe region
    if rec["fence_allow"]:
        for m in re.finditer(r'\bunsafe\s+(?:extern\s+"[^"]*"\s+)?fn\b', src):
            ob = src.find('{', m.end())
            if ob < 0: continue
            end = match_brace(src, ob)
            body = src[ob+1:end-1]
            # subtract what explicit inner unsafe blocks already counted
            inner = [(s, e) for s, e in covered if ob <= s < end]
            keep = body
            for s, e in sorted(inner, reverse=True):
                keep = keep[:s-ob-1] + " " * (e-s) + keep[e-ob-1:]
            ops = count_ops(keep, owned)
            rec["ops"].update(ops)
            rec["forms"]["fenced_fn_body"] += 1
    return rec

# ---------------------------------------------------------------- ratchet

METRIC = "shipped_nonffi_unsafe_ops"

def baseline_crates(recs):
    """The baseline: per crate root, the unsafe operations that are neither an FFI call nor
    inside `#[cfg(test)]`. A crate at zero is left out. The file carries no total: that would
    be one line every count-changing PR edits, so any two of them would conflict."""
    per = collections.Counter()
    for r in recs:
        for k, v in r["ops"].items():
            if not k.startswith("ffi_call"):
                per[r["crate_root"]] += v - r["test_ops"].get(k, 0)
    return {k: per[k] for k in sorted(per) if per[k]}

def baseline_diff(today, base):
    """(crate, today, baseline) for each crate whose count differs."""
    rows = [(c, today.get(c, 0), base.get(c, 0)) for c in sorted(set(today) | set(base))]
    return [r for r in rows if r[1] != r[2]]

def check_baseline(today, path):
    with open(path, encoding="utf-8") as f:
        rows = baseline_diff(today, json.load(f)["crates"])
    if not rows:
        print(f"unsafe-census: {sum(today.values())} shipped non-FFI unsafe ops, baseline matches")
        return 0
    print(f"{'crate':<40}{'today':>7}{'baseline':>10}{'delta':>7}")
    for c, t, b in rows:
        print(f"{c:<40}{t:>7}{b:>10}{t - b:>+7}")
    for c, t, b in rows:
        if t > b:
            print(f"::error::non-FFI unsafe ops rose in {c} ({b} -> {t}). Remove the op, or if "
                  "the rise is deliberate regenerate the baseline and say why in the PR.")
        else:
            print(f"::error::baseline stale for {c} ({b} -> {t}). Regenerate the baseline.")
    print("regenerate: python3 scripts/unsafe-census.py . --write-baseline "
          "scripts/ci/unsafe-census-baseline.json")
    return 1

def self_test():
    assert baseline_diff({"a": 2}, {"a": 2}) == []
    assert baseline_diff({"a": 3}, {"a": 2}) == [("a", 3, 2)]
    assert baseline_diff({"a": 1}, {"a": 2}) == [("a", 1, 2)]
    assert baseline_diff({"a": 2, "new": 1}, {"a": 2}) == [("new", 1, 0)]
    assert baseline_diff({}, {"gone": 4}) == [("gone", 0, 4)]
    recs = [{"crate_root": "a", "ops": {"ptr_deref": 3, "ffi_call": 5}, "test_ops": {"ptr_deref": 1}},
            {"crate_root": "b", "ops": {"ffi_call": 2}, "test_ops": {}}]
    assert baseline_crates(recs) == {"a": 2}
    owned = owned_names("let h = unsafe { Owned::new(raw) }; fn f(t: &Owned<HANDLE>) {}")
    assert owned == {"h", "t"}
    assert count_ops("Close(*h); Query(*t, *p);", owned)["ptr_deref"] == 1
    assert count_ops("Close(*h); Query(*t, *p);")["ptr_deref"] == 3
    print("unsafe-census self-test ok")
    return 0

# ---------------------------------------------------------------- driver

def walk_rs(root):
    for dirpath, dirnames, files in os.walk(root):
        dirnames[:] = [d for d in dirnames if d not in EXCLUDE_DIR_PARTS]
        for f in files:
            if f.endswith('.rs'):
                yield os.path.join(dirpath, f)

def main():
    if sys.argv[1:] == ["--self-test"]:
        return self_test()
    ap = argparse.ArgumentParser()
    ap.add_argument("repo")
    ap.add_argument("--json", action="store_true")
    # Resolved here, before the chdir into the repo below.
    ap.add_argument("--baseline", type=os.path.abspath, metavar="PATH")
    ap.add_argument("--write-baseline", type=os.path.abspath, metavar="PATH")
    args = ap.parse_args()
    as_json = args.json
    os.chdir(os.path.abspath(args.repo))
    allrecs = []
    for root in scope_roots():
        if not os.path.isdir(root): continue
        # crate entry points (a dir may hold several crates, e.g. packaging/windows/drivers)
        entries = []
        for dirpath, dirnames, files in os.walk(root):
            dirnames[:] = [d for d in dirnames if d not in EXCLUDE_DIR_PARTS]
            if 'Cargo.toml' in files:
                for e in ('src/lib.rs', 'src/main.rs'):
                    p = os.path.join(dirpath, e)
                    if os.path.exists(p): entries.append(p)
        modmap = build_module_map(root, entries)
        for p in walk_rs(root):
            plat, feats = modmap.get(p, (None, frozenset()))
            if p not in modmap:
                # not reached from a crate root. Integration tests, examples, benches and build
                # scripts never ship; anything else is a module the walk missed — mark by path.
                low = p.replace(os.sep, '/')
                if ('/tests/' in low or '/examples/' in low or '/benches/' in low
                        or low.endswith('/build.rs')):
                    continue
                if '/windows' in low or '_windows' in low: plat = 'windows'
                elif '/linux' in low or '_linux' in low: plat = 'linux'
            rec = census_file(p, plat, feats)
            rec["crate_root"] = root
            allrecs.append(rec)
    if args.write_baseline:
        with open(args.write_baseline, "w", encoding="utf-8") as f:
            json.dump({"metric": METRIC, "crates": baseline_crates(allrecs)}, f, indent=2)
            f.write("\n")
        return
    if args.baseline:
        return check_baseline(baseline_crates(allrecs), args.baseline)
    if as_json:
        for r in allrecs:
            r["forms"] = dict(r["forms"]); r["ops"] = dict(r["ops"]); r["test_ops"] = dict(r["test_ops"])
        print(json.dumps(allrecs, indent=1))
        return
    # ---- summary tables
    def agg(recs, key):
        d = collections.defaultdict(lambda: (collections.Counter(), collections.Counter()))
        for r in recs:
            k = key(r)
            d[k][0].update(r["forms"]); d[k][1].update(r["ops"])
        return d

    print("=" * 100)
    print("TABLE 1 — FORMS by crate root")
    print("=" * 100)
    per = agg(allrecs, lambda r: r["crate_root"])
    hdr = ["blk", "nest", "fn", "impSend", "impSync", "impOth", "extBlk", "fencedFn"]
    keys = ["unsafe_block", "unsafe_block_nested", "unsafe_fn", "unsafe_impl_send",
            "unsafe_impl_sync", "unsafe_impl_other", "unsafe_extern_block", "fenced_fn_body"]
    print(f"{'crate':<38}" + "".join(f"{h:>9}" for h in hdr))
    tot = collections.Counter()
    for k in sorted(per):
        f, _ = per[k]; tot.update(f)
        print(f"{k:<38}" + "".join(f"{f[x]:>9}" for x in keys))
    print(f"{'TOTAL':<38}" + "".join(f"{tot[x]:>9}" for x in keys))

    print()
    print("=" * 100)
    print("TABLE 2 — OPERATIONS by crate root (what the unsafe DOES)")
    print("=" * 100)
    opkeys = ["ffi_call", "ffi_call_fnptr", "ptr_deref", "ptr_as_ref", "ptr_read_write",
              "slice_from_raw", "transmute", "from_raw_owning", "cstr_from_ptr",
              "assume_init", "set_len", "get_unchecked", "static_mut", "union_field", "env_set_var"]
    print(f"{'crate':<30}" + "".join(f"{h[:13]:>14}" for h in opkeys))
    topo = collections.Counter()
    for k in sorted(per):
        _, o = per[k]; topo.update(o)
        print(f"{k:<30}" + "".join(f"{o[x]:>14}" for x in opkeys))
    print(f"{'TOTAL':<30}" + "".join(f"{topo[x]:>14}" for x in opkeys))
    tt = collections.Counter()
    for r in allrecs: tt.update(r["test_ops"])
    ffi = topo["ffi_call"] + topo["ffi_call_fnptr"]
    nonffi = sum(topo[x] for x in opkeys if not x.startswith("ffi_call"))
    print(f"\n  FFI-call operations : {ffi}")
    print(f"  NON-FFI operations  : {nonffi}   <-- PRIMARY METRIC CANDIDATE")
    print(f"  total operations    : {ffi+nonffi}   (non-FFI share {100*nonffi/max(1,ffi+nonffi):.1f}%)")
    tffi = tt["ffi_call"] + tt["ffi_call_fnptr"]
    tnon = sum(v for k, v in tt.items() if not k.startswith("ffi_call"))
    print(f"  of which in #[cfg(test)]: ffi={tffi} nonFFI={tnon}")
    print(f"  SHIPPED non-FFI ops : {nonffi - tnon}   <-- PRIMARY METRIC (test code excluded)")

    print()
    print("=" * 100)
    print("TABLE 3 — VISIBILITY: what a Linux/macOS default-feature check CANNOT see")
    print("=" * 100)
    vis = collections.defaultdict(lambda: [0, 0, 0])  # blocks, ffi ops, nonffi ops
    for r in allrecs:
        plat = r["platform"] or "portable"
        feats = r["features"]
        bucket = plat if not feats else f"{plat}+feat({','.join(feats)})"
        vis[bucket][0] += r["forms"]["unsafe_block"]
        vis[bucket][1] += r["ops"]["ffi_call"] + r["ops"]["ffi_call_fnptr"]
        vis[bucket][2] += sum(v for k, v in r["ops"].items() if not k.startswith("ffi_call"))
    print(f"{'cfg bucket':<46}{'blocks':>9}{'ffi ops':>10}{'nonFFI ops':>12}")
    for k in sorted(vis, key=lambda x: -vis[x][0]):
        b, f, nf = vis[k]
        print(f"{k:<46}{b:>9}{f:>10}{nf:>12}")

    print()
    print("=" * 100)
    print("TABLE 4 — repr(C) mirrors and layout asserts")
    print("=" * 100)
    for r in sorted(allrecs, key=lambda r: -r["repr_c"]):
        if r["repr_c"]:
            print(f"{r['repr_c']:>4} repr(C) {r['layout_asserts']:>4} asserts  {r['path']}")

    print()
    print("=" * 100)
    print("TABLE 5 — files with an unsafe_op_in_unsafe_fn FENCE")
    print("=" * 100)
    for r in allrecs:
        if r["fence_allow"]:
            o = r["ops"]
            nf = sum(v for k, v in o.items() if not k.startswith("ffi_call"))
            print(f"  {r['path']:<62} ffi={o['ffi_call']+o['ffi_call_fnptr']:>4} nonFFI={nf:>4}")

    print()
    print("=" * 100)
    print("TABLE 5b — RAW HANDLES NOT OWNED BY A Drop TYPE")
    print("=" * 100)
    nun = nown = 0
    for r in allrecs:
        nown += len(r["handles_owned"]); nun += len(r["handles_unowned"])
    print(f"  structs holding a raw handle/pointer WITH an impl Drop : {nown}")
    print(f"  structs holding a raw handle/pointer WITHOUT impl Drop : {nun}   <-- SECONDARY METRIC")
    for r in sorted(allrecs, key=lambda r: -len(r["handles_unowned"]))[:18]:
        if r["handles_unowned"]:
            print(f"   {len(r['handles_unowned']):>3}  {r['path']}  {r['handles_unowned'][:6]}")

    print()
    print("=" * 100)
    print("TABLE 5c — TEST-ONLY unsafe blocks (excluded from any shipped-code metric)")
    print("=" * 100)
    tb = sum(r["test_blocks"] for r in allrecs)
    print(f"  unsafe blocks inside #[cfg(test)] mod : {tb} of {sum(r['forms']['unsafe_block'] for r in allrecs)}")
    for r in sorted(allrecs, key=lambda r: -r["test_blocks"])[:10]:
        if r["test_blocks"]: print(f"   {r['test_blocks']:>3}  {r['path']}")

    print()
    print("=" * 100)
    print("TABLE 6 — TOP files by NON-FFI operations")
    print("=" * 100)
    scored = []
    for r in allrecs:
        nf = sum(v for k, v in r["ops"].items() if not k.startswith("ffi_call"))
        if nf: scored.append((nf, r))
    for nf, r in sorted(scored, key=lambda t: -t[0])[:30]:
        det = ",".join(f"{k}={v}" for k, v in sorted(r["ops"].items())
                       if not k.startswith("ffi_call"))
        print(f"{nf:>5}  {r['path']}\n         {det}")

if __name__ == "__main__":
    sys.exit(main())
