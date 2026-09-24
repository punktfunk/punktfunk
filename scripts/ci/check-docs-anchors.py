"""Dead-anchor check: every /docs/<slug>#<anchor> link in the docs, the console, the setup wizard
and the clients must name a heading on that page. Headings slug like github-slugger (Fumadocs'
own); `## Title [#id]` sets the id by hand. Called from check-docs-links.sh.
"""
import pathlib, re, subprocess, sys

root = pathlib.Path(subprocess.check_output(['git', 'rev-parse', '--show-toplevel'], text=True).strip())
content = root / 'docs-site/content/docs'


def page_slug(path):
    parts = [s for s in path.relative_to(content).with_suffix('').parts if not s.startswith('(')]
    return '/'.join(parts[:-1] if parts and parts[-1] == 'index' else parts)


def slug(text, seen):
    base = re.sub(r'[^\w\- ]', '', text.lower().strip()).replace(' ', '-')
    s, n = base, 0
    while s in seen:
        n += 1
        s = f'{base}-{n}'
    seen.add(s)
    return s


def heading_text(line):
    t = re.sub(r'^#+\s+', '', line).strip()
    t = re.sub(r'`([^`]*)`', r'\1', t)
    t = re.sub(r'\[([^\]]*)\]\([^)]*\)', r'\1', t)
    return re.sub(r'[*_]{1,2}([^*_]+)[*_]{1,2}', r'\1', t)


anchors = {}
for p in [*content.rglob('*.md'), *content.rglob('*.mdx')]:
    seen, fence = set(), False
    for line in p.read_text().splitlines():
        if line.lstrip().startswith('```'):
            fence = not fence
        if fence or not re.match(r'^#{2,6}\s', line):
            continue
        m = re.search(r'\[#([^\]]+)\]\s*$', line)
        if m:
            seen.add(m.group(1))
        else:
            slug(heading_text(line), seen)
    anchors[page_slug(p)] = seen

# The console links with `DocsLink path="<slug>#<id>"`; everything else is a /docs/ URL or path.
link = re.compile(r'(?:/docs/|DocsLink path=")([a-z0-9/-]*)#([A-Za-z0-9_-]+)')
grep = ['git', 'grep', '-nE', r'(/docs/|DocsLink path=")[a-z0-9/-]*#', '--', 'docs-site/content',
        'web/src', 'crates', 'clients', 'packaging', 'scripts', '*.md', ':!*/vendor/*',
        ':!**/tests/golden/**', ':!docs/releases', ':!CHANGELOG.md']
out = subprocess.run(grep, text=True, cwd=root, capture_output=True).stdout
bad = 0
for line in out.splitlines():
    for m in link.finditer(line):
        page, frag = m.group(1).strip('/'), m.group(2)
        if frag not in anchors.get(page, ()):
            print(f'::error::dead docs anchor /docs/{page}#{frag} — {line[:160]}')
            bad = 1
sys.exit(bad)
