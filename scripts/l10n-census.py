#!/usr/bin/env python3
"""
Punktfunk text census: the text a person reads that does not come from l10n/messages yet.

Usage:  python3 scripts/l10n-census.py <repo-root> [--check | --update]

It reads the files git lists (tracked, or untracked and not ignored), so a generated `L10n.swift`
or `strings.xml` on disk never counts.

Per surface, it counts the string literals (and JSX text) in code that read as prose. It is a
ratchet for a surface that is half moved to messages: a count may fall, never rise. `--check`
fails when a surface counts more than scripts/ci/l10n-census/<surface>.txt and prints the
literals that its .list does not hold. `--update` rewrites both files. With no flag it prints
the counts. docs/l10n.md says how to move text.

A small tokenizer per language (Rust, Swift, Kotlin, TypeScript) skips comments, test code,
attributes, and the arguments of log, print, panic, assert and error-context calls. What is
left counts when it reads as prose: a capitalized word followed by another word, or at least
three words and placeholders with two words among them. Paths, flags, identifiers and names
from l10n/glossary.json alone do not count. Single-word labels never do. It miscounts at the
edges; it is deterministic, and it falls as text moves.

Tests: python3 scripts/l10n-census-test.py
"""
import argparse
import bisect
import collections
import json
import os
import re
import subprocess
import sys

SURFACES = {
    "kit": ["crates/client/pf-console-ui/src"],
    "core": ["crates/client/pf-client-core/src"],
    "apple": ["clients/apple/Sources", "clients/apple/PunktfunkWidgets"],
    "android": ["clients/android/app/src/main", "clients/android/kit/src/main"],
    "linux": ["clients/linux/src"],
    "windows": ["clients/windows/src"],
    "decky": ["clients/decky/src"],
    "tray": ["crates/install/punktfunk-tray/src"],
    "setup": ["crates/install/punktfunk-setup/src"],
    "setup-win": ["crates/install/punktfunk-setup-win/src"],
}
BASELINE_DIR = "scripts/ci/l10n-census"
LANGS = {".rs": "rust", ".swift": "swift", ".kt": "kotlin", ".ts": "ts", ".tsx": "tsx"}
SKIP_DIRS = {"target", "build", ".build", "node_modules", "tests", "Tests", "test", "androidTest",
             "__tests__"}
# Never translated and not in the glossary: stores, platforms, other products, fonts.
EXTRA_NAMES = ["Steam", "Moonlight", "Sunshine", "Apollo", "Decky", "Apple TV", "macOS", "iOS",
               "iPadOS", "tvOS", "Android", "Fire TV", "Windows", "Linux", "GNOME", "KDE Plasma",
               "Plasma", "Bazzite", "SteamOS", "Flatpak", "Tailscale", "PlayStation", "Nintendo",
               "Switch Pro", "Joy-Con", "Xbox Series", "Xbox One", "Geist", "Geist Mono", "Noto",
               "Segoe Fluent Icons", "Segoe MDL2 Assets", "wlroots", "Sway", "River"]

# ---------------------------------------------------------------- tokenizer

TOKEN = re.compile(r"[A-Za-z_][A-Za-z0-9_]*|\d[\w.]*|\S")
SPACE = re.compile(r"\s*")
IDENT = re.compile(r"[A-Za-z_][A-Za-z0-9_]*")
DQ_BODY = re.compile(r'[^"\\]*(?:\\.[^"\\]*)*', re.S)
SQ_BODY = re.compile(r"[^'\\]*(?:\\.[^'\\]*)*", re.S)
RUST_RAW = re.compile(r'(?:b|c)?r(#*)"')
RUST_PLAIN = re.compile(r'(?:b|c)?"')
RUST_CHAR = re.compile(r"'(?:[^\\']|\\(?:u\{[0-9a-fA-F]+\}|x[0-9a-fA-F]{2}|.))'")
KT_CHAR = re.compile(r"'(?:[^\\']|\\u[0-9a-fA-F]{4}|\\.)'")
KT_STOP = re.compile(r'["\\$]')
KT_RAW_STOP = re.compile(r'"""|\$')
TS_TEMPLATE_STOP = re.compile(r"[`\\$]")
TS_REGEX = re.compile(r"/(?:[^/\\\[\n]|\\.|\[(?:[^\]\\\n]|\\.)*\])+/[a-z]*")
SWIFT_OPEN = re.compile(r'(#*)("""|")')
JSX_RUN = re.compile(r"[^<{}]*")
COMMENT_MARK = re.compile(r"/\*|\*/")
# After these a `/` divides; anywhere else in TypeScript it opens a regex.
VALUE_END = {")", "]", "}"}
KEYWORDS_BEFORE_VALUE = {"return", "typeof", "case", "in", "of", "delete", "void", "throw", "new"}
ESCAPE = re.compile(r"\\(?:u\{([0-9a-fA-F]{1,6})\}|u([0-9a-fA-F]{4})|(.))", re.S)


def unescape(text):
    """Escapes as the reader sees them; a line break or tab becomes a space."""
    def one(m):
        code = m.group(1) or m.group(2)
        if code:
            cp = int(code, 16)
            return chr(cp) if cp < 0xD800 or 0xDFFF < cp <= 0x10FFFF else " "
        return " " if m.group(3) in "nrt0" else m.group(3)
    return ESCAPE.sub(one, text)


def block_comment_end(src, i, nested):
    if not nested:
        j = src.find("*/", i + 2)
        return len(src) if j < 0 else j + 2
    depth = 0
    for m in COMMENT_MARK.finditer(src, i):
        depth += 1 if m.group() == "/*" else -1
        if depth == 0:
            return m.end()
    return len(src)


def swift_string(src, i, hashes, triple):
    """Body of a Swift string whose opening delimiter ends at i: (end, text)."""
    close = ('"""' if triple else '"') + hashes
    esc = "\\" + hashes
    out, j = [], i
    while True:
        k_close, k_esc = src.find(close, j), src.find(esc, j)
        if k_close < 0:
            return len(src), "".join(out) + src[j:]
        if 0 <= k_esc < k_close:
            out.append(src[j:k_esc])
            p = k_esc + len(esc)
            if src.startswith("(", p):
                _, j = lex(src, "swift", p + 1, stop=True)
                out.append("{}")
            else:
                out.append(src[k_esc:p + 1])
                j = p + 1
            continue
        out.append(src[j:k_close])
        return k_close + len(close), "".join(out)


def kotlin_string(src, i, triple):
    out, j = [], i
    stop = KT_RAW_STOP if triple else KT_STOP
    while True:
        m = stop.search(src, j)
        if not m:
            return len(src), "".join(out) + src[j:]
        k, t = m.start(), m.group()
        out.append(src[j:k])
        if t == "\\":
            out.append(src[k:k + 2])
            j = k + 2
        elif t == "$":
            name = IDENT.match(src, k + 1)
            if src.startswith("{", k + 1):
                _, j = lex(src, "kotlin", k + 2, stop=True)
                out.append("{}")
            elif name:
                out.append("{}")
                j = name.end()
            else:
                out.append("$")
                j = k + 1
        else:
            return m.end(), "".join(out)


def ts_template(src, i):
    out, j = [], i
    while True:
        m = TS_TEMPLATE_STOP.search(src, j)
        if not m:
            return len(src), "".join(out) + src[j:]
        k, t = m.start(), m.group()
        out.append(src[j:k])
        if t == "\\":
            out.append(src[k:k + 2])
            j = k + 2
        elif t == "$" and src.startswith("{", k + 1):
            _, j = lex(src, "ts", k + 2, stop=True)
            out.append("{}")
        elif t == "$":
            out.append("$")
            j = k + 1
        else:
            return k + 1, "".join(out)


def jsx_tag_ends(toks):
    """Whether the `>` about to be read closes a JSX tag, judged from the tokens before it."""
    depth = 0
    for k in range(len(toks) - 1, max(-1, len(toks) - 400), -1):
        kind, t, _ = toks[k]
        if kind != "p":
            continue
        if t in ")]}":
            depth += 1
        elif t in "([{":
            if depth == 0:
                return False
            depth -= 1
        elif t == ";" and depth == 0:
            return False
        elif t == "<" and depth == 0:
            nxt = toks[k + 1] if k + 1 < len(toks) else None
            if nxt is not None and nxt[0] != "id" and nxt[1] not in "/>":
                return False
            prev = toks[k - 1] if k else None
            return (prev is None or (prev[0] == "p" and prev[1] not in ")]")
                    or (prev[0] == "id" and prev[1] == "return"))
    return False


def lex(src, lang, i=0, stop=False):
    """Tokens of `src` from `i`: (kind, text, offset), kind one of id, num, p, str, jsx.

    With `stop`, it reads one interpolation and returns after the bracket that closes it.
    Returns (tokens, end)."""
    toks, n, depth = [], len(src), 0
    rust, swift, kotlin = lang == "rust", lang == "swift", lang == "kotlin"
    ts = lang in ("ts", "tsx")
    while True:
        i = SPACE.match(src, i).end()
        if i >= n:
            return toks, n
        c = src[i]
        if c == "/" and src.startswith("//", i):
            j = src.find("\n", i)
            i = n if j < 0 else j
            continue
        if c == "/" and src.startswith("/*", i):
            i = block_comment_end(src, i, not ts)
            continue
        if rust and c in "brc\"":
            m = RUST_RAW.match(src, i)
            if m:
                close = '"' + m.group(1)
                j = src.find(close, m.end())
                j = n if j < 0 else j
                toks.append(("str", src[m.end():j], i))
                i = j + len(close)
                continue
            m = RUST_PLAIN.match(src, i)
            if m:
                body = DQ_BODY.match(src, m.end())
                toks.append(("str", unescape(body.group()), i))
                i = body.end() + 1
                continue
        if rust and c == "'":
            m = RUST_CHAR.match(src, i)
            i = m.end() if m else i + 1
            continue
        if swift and c in '#"':
            m = SWIFT_OPEN.match(src, i)
            if m:
                j, text = swift_string(src, m.end(), m.group(1), m.group(2) == '"""')
                toks.append(("str", text if m.group(1) else unescape(text), i))
                i = j
                continue
        if kotlin and c == '"':
            triple = src.startswith('"""', i)
            j, text = kotlin_string(src, i + (3 if triple else 1), triple)
            toks.append(("str", text if triple else unescape(text), i))
            i = j
            continue
        if kotlin and c == "'":
            m = KT_CHAR.match(src, i)
            if m:
                i = m.end()
                continue
        if ts and c in "'\"":
            body = (DQ_BODY if c == '"' else SQ_BODY).match(src, i + 1)
            toks.append(("str", unescape(body.group()), i))
            i = body.end() + 1
            continue
        if ts and c == "`":
            j, text = ts_template(src, i + 1)
            toks.append(("str", unescape(text), i))
            i = j
            continue
        if ts and c == "/":
            prev = toks[-1] if toks else None
            divides = prev is not None and (
                prev[0] in ("num", "str") or (prev[0] == "p" and prev[1] in VALUE_END)
                or (prev[0] == "id" and prev[1] not in KEYWORDS_BEFORE_VALUE))
            m = None if divides else TS_REGEX.match(src, i)
            if m:
                i = m.end()
                continue
        if lang == "tsx" and c == ">" and src[i - 1:i] not in ("=", "-") and jsx_tag_ends(toks):
            toks.append(("p", ">", i))
            run = JSX_RUN.match(src, i + 1)
            if re.search(r"[A-Za-z]", run.group()):
                toks.append(("jsx", run.group().strip(), i + 1))
                i = run.end()
            else:
                i += 1
            continue
        m = TOKEN.match(src, i)
        t = m.group()
        kind = "id" if (t[0].isalpha() or t[0] == "_") else "num" if t[0].isdigit() else "p"
        if stop and kind == "p":
            if t in "([{":
                depth += 1
            elif t in ")]}":
                if depth == 0:
                    return toks, m.end()
                depth -= 1
        toks.append((kind, t, i))
        i = m.end()

# ---------------------------------------------------------------- what is not UI text

RUST_MACROS = {"trace", "debug", "info", "warn", "error", "event", "span", "trace_span",
               "debug_span", "info_span", "warn_span", "error_span", "bail", "anyhow", "ensure",
               "format_err", "panic", "unreachable", "todo", "unimplemented", "assert",
               "assert_eq", "assert_ne", "debug_assert", "debug_assert_eq", "debug_assert_ne",
               "println", "print", "eprintln", "eprint", "dbg"}
RUST_CALLS = {"context", "with_context", "expect", "expect_err", "wrap_err", "wrap_err_with"}
BARE_CALLS = {
    "swift": {"print", "debugPrint", "dump", "NSLog", "os_log", "fatalError", "precondition",
              "preconditionFailure", "assert", "assertionFailure", "Logger"},
    "kotlin": {"print", "println", "error", "require", "requireNotNull", "check",
               "checkNotNull", "assert", "TODO"},
    "ts": set(), "tsx": set(), "rust": set(),
}
# A receiver that is a logger: `log.info(`, `Log.w(`, `sessionLog.debug(`, `console.warn(`.
LOGGER = re.compile(r"^(?:log|logger|Log|Logger|Timber|console|tracing)$|[a-z0-9](?:Log|Logger)$")


def call_path(toks, j, lang):
    """The callee path that ends at toks[j], outermost first: `self.log.info` -> [self, log, info]."""
    parts = []
    while j >= 0 and toks[j][0] == "id":
        parts.append(toks[j][1])
        j -= 1
        if lang == "rust" and j >= 1 and toks[j][1] == ":" and toks[j - 1][1] == ":":
            j -= 2
        elif j >= 0 and toks[j][1] == ".":
            j -= 1
            while j >= 0 and toks[j][1] in ("?", "!"):
                j -= 1
        else:
            break
    return parts[::-1]


def opens_excluded_call(toks, k, lang):
    """Whether the bracket at toks[k] holds the arguments of a log, panic or error call."""
    j, macro, bracket = k - 1, False, toks[k][1]
    if j < 0:
        return False
    if lang == "rust" and toks[j][1] == "!":
        macro, j = True, j - 1
    elif bracket == "{" and lang == "kotlin" and toks[j][1] == ")":
        depth = 0
        while j >= 0:
            depth += {")": 1, "(": -1}.get(toks[j][1], 0) if toks[j][0] == "p" else 0
            if depth == 0:
                break
            j -= 1
        j -= 1
    elif bracket != "(":
        return False
    path = call_path(toks, j, lang)
    if not path:
        return False
    if any(LOGGER.search(p) for p in path[:-1]):
        return True
    if lang == "rust":
        return path[-1] in (RUST_MACROS if macro else RUST_CALLS)
    return len(path) == 1 and path[0] in BARE_CALLS[lang]


def rust_attribute(toks, k):
    """At the `[` of a Rust attribute: (is an attribute, marks test code, is inner)."""
    inner = k >= 2 and toks[k - 1][1] == "!" and toks[k - 2][1] == "#"
    if not (inner or (k >= 1 and toks[k - 1][1] == "#")):
        return False, False, False
    ids = []
    for kind, t, _ in toks[k + 1:k + 12]:
        if t in ("]", "=") or (t == ")" and ids[:1] != ["cfg"]):
            break
        if kind == "id":
            ids.append(t)
    test = ids[-1:] == ["test"] or ids[:2] == ["cfg", "test"] or ids[:3] == ["cfg", "all", "test"]
    if ids[:1] == ["cfg"] and "not" in ids:
        test = False
    return True, test, inner


def strings(src, lang):
    """The string and JSX tokens a person could read: (offset, text)."""
    toks, _ = lex(src, lang)
    out, stack, skip = [], [], None
    for k, (kind, t, off) in enumerate(toks):
        hidden = skip is not None or (stack and stack[-1])
        if kind in ("str", "jsx"):
            if not hidden:
                out.append((off, t))
        elif kind == "id":
            if lang == "swift" and t == "Preview" and k and toks[k - 1][1] == "#":
                skip = len(stack)
        elif kind == "p" and t in "([{":
            ex = hidden or opens_excluded_call(toks, k, lang)
            if t == "[" and lang == "rust":
                attr, test, inner = rust_attribute(toks, k)
                if test and inner:
                    return []
                if test and skip is None:
                    skip = len(stack)
                ex = ex or attr
            if t == "{" and skip == len(stack):
                ex, skip = True, None
            stack.append(ex)
        elif kind == "p" and t in ")]}":
            if stack:
                stack.pop()
            if skip is not None and len(stack) < skip:
                skip = None
        elif kind == "p" and t in ";," and skip == len(stack):
            skip = None
    return out

# ---------------------------------------------------------------- what reads as prose

PLACEHOLDER = re.compile(r"\{[^{}]*\}|%(?:\d+\$)?[-+ 0#]*\d*(?:\.\d+)?(?:ll|l|h)?[sdfiuxX@]")
CODE = re.compile(
    r"::|://|=|&&|\|\||</|/>|\{\"|\":|\\|(?:^|\s)--?[A-Za-z]|^\s*[~/]|[A-Za-z0-9]_[A-Za-z0-9]"
    r"|\b[a-z-]+:\s[^;]*;|\.(?:rs|toml|json|desktop|service|conf|so|dll|exe|sh|py|png|svg|plist"
    r"|xml|txt)\b")
WORD_RE = r"[A-Za-z]+(?:[-'’][A-Za-z]+)*"
CAPITALIZED_PAIR = re.compile(r"\b[A-Z][a-z]+(?:[-'’][A-Za-z]+)*[,:;]?\s+[\"'“‘(]?[A-Za-z]")
WORD = re.compile(r"[\"'“‘(]*(" + WORD_RE + r")[.,:;!?…)\"'”’]*")


def name_pattern(names):
    alts = "|".join(re.escape(n) for n in sorted(set(names), key=len, reverse=True))
    return re.compile(r"(?<![A-Za-z0-9])(?:" + alts + r")(?![A-Za-z0-9])")


def is_prose(text, names):
    """A capitalized word followed by another word; or three words and placeholders, two of
    them words, not starting lowercase (that is the operator register). Not code, not only
    names."""
    t = PLACEHOLDER.sub(" {} ", text.replace("{{", "").replace("}}", ""))
    if CODE.search(t) or not re.search(r"[A-Za-z]", names.sub(" ", t)):
        return False
    if CAPITALIZED_PAIR.search(t):
        return True
    words = [m.group(1) for m in map(WORD.fullmatch, t.split()) if m]
    words = [w for w in words if len(w) > 1 or w in ("a", "A", "I")]
    lead = t.lstrip(" \"'“‘(—–-…·")
    return (len(words) >= 2 and len(words) + t.count("{}") >= 3
            and not lead[:1].islower())


def literals(src, lang, names):
    """(line, text) for each literal in `src` that the census counts."""
    newlines = [m.start() for m in re.finditer("\n", src)]
    out = []
    for off, raw in strings(src, lang):
        text = " ".join(raw.split())
        if is_prose(text, names):
            out.append((bisect.bisect_left(newlines, off) + 1, text))
    return out

# ---------------------------------------------------------------- the tree

CFG_TEST_MOD = re.compile(r"#\[cfg\(test\)\]\s*(?:pub(?:\([^)]*\))?\s+)?mod\s+(\w+)\s*;")


def listed(repo):
    """Paths relative to `repo` that git tracks or would add, so an ignored generated file never
    counts. `None` outside a checkout, where the file system is walked instead."""
    try:
        out = subprocess.run(
            ["git", "-C", repo, "ls-files", "-z", "--cached", "--others", "--exclude-standard"],
            capture_output=True, check=True).stdout
    except (OSError, subprocess.CalledProcessError):
        return None
    return sorted(set(out.decode("utf-8", "replace").split("\0")) - {""})


def surface_files(repo, roots):
    """Source files under `roots`, sorted, without test files or test-only Rust modules."""
    found = []
    paths = listed(repo)
    for root in roots:
        if paths is None:
            for d, dirs, files in os.walk(os.path.join(repo, root)):
                dirs[:] = sorted(x for x in dirs if x not in SKIP_DIRS and not x.startswith("."))
                found += [os.path.join(d, f) for f in sorted(files)
                          if os.path.splitext(f)[1] in LANGS]
            continue
        for rel in paths:
            dirs = rel[len(root) + 1:].split("/")[:-1]
            if (rel.startswith(root + "/") and os.path.splitext(rel)[1] in LANGS
                    and not any(x in SKIP_DIRS or x.startswith(".") for x in dirs)
                    and os.path.isfile(os.path.join(repo, rel))):
                found.append(os.path.join(repo, rel))
    test_only = set()
    for p in found:
        if p.endswith(".rs"):
            stem = os.path.splitext(os.path.basename(p))[0]
            base = os.path.dirname(p) if stem in ("lib", "main", "mod") else p[:-3]
            with open(p, encoding="utf-8", errors="replace") as f:
                test_only.update(os.path.join(base, m) for m in CFG_TEST_MOD.findall(f.read()))
    return [p for p in found
            if os.path.basename(p) != "tests.rs" and not p.endswith(("Tests.swift", ".test.ts",
                                                                      ".test.tsx"))
            and not any(p == t + ".rs" or p.startswith(t + os.sep) for t in test_only)]


def glossary_names(repo):
    with open(os.path.join(repo, "l10n/glossary.json"), encoding="utf-8") as f:
        return json.load(f).get("never_translate", []) + EXTRA_NAMES


def census(repo):
    """{surface: sorted ["path\ttext"]} and {surface: ["path:line: text"]} for printing."""
    names = name_pattern(glossary_names(repo))
    lists, located = {}, {}
    for surface, roots in SURFACES.items():
        rows = []
        for p in surface_files(repo, roots):
            rel = os.path.relpath(p, repo).replace(os.sep, "/")
            with open(p, encoding="utf-8", errors="replace") as f:
                src = f.read()
            for line, text in literals(src, LANGS[os.path.splitext(p)[1]], names):
                rows.append((rel, line, text))
        lists[surface] = sorted(f"{r}\t{t}" for r, _, t in rows)
        located[surface] = [f"{r}:{n}: {t}" for r, n, t in rows]
    return lists, located


def new_literals(today, stored):
    """The `path\ttext` rows in `today` that `stored` does not hold, counted with repeats."""
    extra = collections.Counter(today) - collections.Counter(stored)
    return sorted(extra.elements())


def read_baseline(repo, surface):
    base = os.path.join(repo, BASELINE_DIR, surface)
    try:
        with open(base + ".txt", encoding="utf-8") as f:
            count = int(f.read().strip())
    except FileNotFoundError:
        count = 0
    try:
        with open(base + ".list", encoding="utf-8") as f:
            stored = f.read().splitlines()
    except FileNotFoundError:
        stored = []
    return count, stored


def main():
    ap = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    ap.add_argument("repo")
    mode = ap.add_mutually_exclusive_group()
    mode.add_argument("--check", action="store_true", help="fail when a surface rose")
    mode.add_argument("--update", action="store_true", help="rewrite the baselines")
    args = ap.parse_args()
    repo = os.path.abspath(args.repo)
    lists, located = census(repo)
    if args.update:
        os.makedirs(os.path.join(repo, BASELINE_DIR), exist_ok=True)
        for surface, rows in lists.items():
            base = os.path.join(repo, BASELINE_DIR, surface)
            with open(base + ".txt", "w", encoding="utf-8") as f:
                f.write(f"{len(rows)}\n")
            with open(base + ".list", "w", encoding="utf-8") as f:
                f.write("".join(r + "\n" for r in rows))
    failed = False
    print(f"{'surface':<12}{'today':>7}{'baseline':>10}")
    for surface, rows in lists.items():
        count, stored = read_baseline(repo, surface)
        print(f"{surface:<12}{len(rows):>7}{count:>10}")
        if not args.check or len(rows) <= count:
            continue
        failed = True
        print(f"::error::{surface}: {len(rows)} literals a person reads, baseline {count}. Move "
              "the new text to l10n/messages (docs/l10n.md). If this surface cannot read "
              "messages yet, run --update and say why in the PR.")
        for row in new_literals(rows, stored)[:40]:
            path, text = row.split("\t", 1)
            where = next((x for x in located[surface] if x.startswith(path + ":")
                          and x.endswith(": " + text)), path)
            print(f"  {where}")
    if args.check and not failed:
        fell = [s for s, rows in lists.items() if len(rows) < read_baseline(repo, s)[0]]
        if fell:
            print(f"fell in {', '.join(fell)}: lock it in with "
                  "python3 scripts/l10n-census.py . --update")
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main())
