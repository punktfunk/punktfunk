import { useEffect, useMemo, useState } from 'react'
import { createFileRoute, Link } from '@tanstack/react-router'
import { ApiReferenceReact } from '@scalar/api-reference-react'
// @scalar/api-reference-react@0.9.78's entry does NOT import its own stylesheet
// (and doesn't inject it at runtime), so we must ship it ourselves or the
// reference renders unstyled. Load it as a route-scoped <link> (same pattern as
// the root app.css), so it's present for SSR + the client-side Vue mount.
import scalarCss from '@scalar/api-reference-react/style.css?url'
import BrandMark from '@/components/BrandMark'
import Wordmark from '@/components/Wordmark'

export const Route = createFileRoute('/api/')({
  component: ApiReference,
  head: () => ({
    meta: [
      { title: 'Punktfunk — Management API Reference' },
      {
        name: 'description',
        content:
          'Interactive reference for the Punktfunk host management REST API (OpenAPI).',
      },
    ],
    links: [{ rel: 'stylesheet', href: scalarCss }],
  }),
})

// The full Punktfunk theme rolled out onto Scalar — the same dark-violet (and
// light-lavender) product chrome as the docs/management console.
//
// IMPORTANT: Scalar toggles `.light-mode` / `.dark-mode` on `document.body`,
// and it renders our `customCss` *before* its own built-in theme preset in the
// SAME <style> tag. A bare `.dark-mode { … }` therefore has equal specificity
// to the preset that comes after it and LOSES (you get Scalar's stock #0f0f0f
// gray). Scoping to `body.dark-mode` / `body.light-mode` (specificity 0,1,1)
// beats both the linked base sheet and the in-component preset, so our palette
// wins regardless of source order. Scalar ignores unknown custom-property
// names, so this stays forward-safe.
const SCALAR_CSS = `
body.light-mode,
body.dark-mode {
  --scalar-font: 'Geist Variable', ui-sans-serif, system-ui, sans-serif;
  --scalar-font-code: ui-monospace, 'SFMono-Regular', Menlo, Consolas, monospace;
  --scalar-radius: 0.5rem;
  --scalar-radius-lg: 0.75rem;
  --scalar-radius-xl: 0.875rem;
}

/* ── Dark — the violet-tinted app-icon chrome (bg #141019 / cards #1c1530). ── */
body.dark-mode {
  --scalar-background-1: #141019;
  --scalar-background-2: #1c1530;
  --scalar-background-3: #221a36;
  --scalar-background-accent: #6c5bf32e;
  --scalar-border-color: #2a2148;

  --scalar-color-1: #f4f2fb;
  --scalar-color-2: #b7b1c9;
  --scalar-color-3: #8a85a0;
  --scalar-color-accent: #a79ff8;

  --scalar-link-color: #a79ff8;
  --scalar-link-color-hover: #c8c0fb;

  --scalar-button-1: #6c5bf3;
  --scalar-button-1-color: #ffffff;
  --scalar-button-1-hover: #5d4ee0;

  --scalar-sidebar-background-1: #17121f;
  --scalar-sidebar-color-1: #e9e6f4;
  --scalar-sidebar-color-2: #9a94ad;
  --scalar-sidebar-color-active: #c8c0fb;
  --scalar-sidebar-item-hover-background: #6c5bf31f;
  --scalar-sidebar-item-hover-color: #f4f2fb;
  --scalar-sidebar-item-active-background: #6c5bf333;
  --scalar-sidebar-border-color: #241c3d;
  --scalar-sidebar-search-background: #1c1530;
  --scalar-sidebar-search-border-color: #2a2148;
  --scalar-sidebar-search-color: #9a94ad;
  --scalar-sidebar-indent-border: #2a2148;
  --scalar-sidebar-indent-border-active: #6c5bf3;
  --scalar-sidebar-indent-border-hover: #463a78;

  --scalar-header-background-1: #141019;
  --scalar-header-color-1: #f4f2fb;
  --scalar-header-border-color: #2a2148;

  --scalar-scrollbar-color: #2a2148;
  --scalar-scrollbar-color-active: #463a78;

  --scalar-color-green: #4ade80;
  --scalar-color-red: #f87171;
  --scalar-color-yellow: #fbbf24;
  --scalar-color-blue: #60a5fa;
  --scalar-color-orange: #fb923c;
  --scalar-color-purple: #a79ff8;
}

/* ── Light — the lavender docs surface (bg #f0ebff / white content). ── */
body.light-mode {
  --scalar-background-1: #ffffff;
  --scalar-background-2: #f6f2ff;
  --scalar-background-3: #ece6fb;
  --scalar-background-accent: #6c5bf31a;
  --scalar-border-color: #e4dcf7;

  --scalar-color-1: #1b1430;
  --scalar-color-2: #4a4368;
  --scalar-color-3: #6f6a86;
  --scalar-color-accent: #6c5bf3;

  --scalar-link-color: #6c5bf3;
  --scalar-link-color-hover: #5d4ee0;

  --scalar-button-1: #6c5bf3;
  --scalar-button-1-color: #ffffff;
  --scalar-button-1-hover: #5d4ee0;

  --scalar-sidebar-background-1: #f6f2ff;
  --scalar-sidebar-color-1: #1b1430;
  --scalar-sidebar-color-2: #6f6a86;
  --scalar-sidebar-color-active: #5d4ee0;
  --scalar-sidebar-item-hover-background: #6c5bf314;
  --scalar-sidebar-item-hover-color: #1b1430;
  --scalar-sidebar-item-active-background: #6c5bf322;
  --scalar-sidebar-border-color: #e4dcf7;
  --scalar-sidebar-search-background: #ffffff;
  --scalar-sidebar-search-border-color: #e4dcf7;
  --scalar-sidebar-search-color: #6f6a86;
  --scalar-sidebar-indent-border: #e4dcf7;
  --scalar-sidebar-indent-border-active: #6c5bf3;
  --scalar-sidebar-indent-border-hover: #c9bdf0;

  --scalar-header-background-1: #ffffff;
  --scalar-header-color-1: #1b1430;
  --scalar-header-border-color: #e4dcf7;

  --scalar-scrollbar-color: #d9d0f2;
  --scalar-scrollbar-color-active: #bcb0ec;

  --scalar-color-green: #16a34a;
  --scalar-color-red: #dc2626;
  --scalar-color-yellow: #d97706;
  --scalar-color-blue: #2563eb;
  --scalar-color-orange: #ea580c;
  --scalar-color-purple: #6c5bf3;
}
`

function ApiReference() {
  // Follow the docs' own light/dark switch and hide Scalar's own toggle, so the
  // Fumadocs toggle stays the single source of truth. Fumadocs drives next-themes
  // with `attribute: "class"`, which writes the resolved theme as a class on
  // <html> — we read THAT class directly rather than next-themes' useTheme().
  // The class is the authoritative, already-resolved signal (system → light/dark
  // included) and, unlike the React context, can't be desynced when bridging into
  // Scalar's separate Vue app. Start dark so SSR and the first client render agree
  // (no hydration mismatch); the docs default to the system theme, and the observer
  // then syncs to the live class, tracking the docs toggle AND OS changes.
  const [isDark, setIsDark] = useState(true)
  useEffect(() => {
    const root = document.documentElement
    const sync = () => setIsDark(root.classList.contains('dark'))
    sync()
    const observer = new MutationObserver(sync)
    observer.observe(root, { attributes: true, attributeFilter: ['class'] })
    return () => observer.disconnect()
  }, [])

  // Scalar pollutes global scope and never cleans up: it appends a persistent
  // <style id="scalar-style"> to <head> that includes a *global*
  // `body { background-color: var(--scalar-background-1) }`, adds its #scalar-refs
  // teleport target, and toggles .dark-mode/.light-mode on <body>. After a
  // client-side route change (no reload) that residue bleeds into the next page —
  // the docs body kept painting Scalar's bg instead of --color-fd-background, so
  // the docs looked gray until a hard reload. Strip it when /api unmounts so
  // leaving the page restores the same DOM a fresh load has; Scalar re-injects a
  // fresh instance on re-entry.
  useEffect(
    () => () => {
      document.getElementById('scalar-style')?.remove()
      document.getElementById('scalar-refs')?.remove()
      document.body.classList.remove('dark-mode', 'light-mode')
    },
    [],
  )

  // A fresh object on each theme flip so the React wrapper's
  // `updateConfiguration` effect fires and Scalar swaps the body mode class.
  const configuration = useMemo(
    () => ({
      url: '/openapi.json',
      darkMode: isDark,
      hideDarkModeToggle: true,
      metaData: { title: 'Punktfunk Management API' },
      hideDownloadButton: false,
      customCss: SCALAR_CSS,
    }),
    [isDark],
  )

  return (
    <div className="flex min-h-screen flex-col">
      {/* Slim branded bar so the reference stays inside the Punktfunk identity
          and links back into the docs. */}
      <header className="flex h-14 shrink-0 items-center justify-between border-b border-fd-border px-4 md:px-6">
        <Link
          to="/docs/$"
          params={{ _splat: '' }}
          aria-label="Punktfunk documentation"
          className="flex items-center gap-2 no-underline"
        >
          <BrandMark className="size-6" />
          <Wordmark className="h-4" />
          <span className="ml-2 hidden text-sm text-fd-muted-foreground sm:inline">
            Management API
          </span>
        </Link>
        <nav className="flex items-center gap-4 text-sm">
          <Link
            to="/docs/$"
            params={{ _splat: '' }}
            className="text-fd-muted-foreground transition-colors hover:text-fd-foreground"
          >
            ← Docs
          </Link>
          <a
            href="/openapi.json"
            className="text-fd-muted-foreground transition-colors hover:text-fd-foreground"
          >
            openapi.json
          </a>
        </nav>
      </header>

      {/* Scalar mounts a Vue app client-side in a useEffect (SSR-safe: the
          server renders an empty container, the browser hydrates the reference). */}
      <div className="min-h-0 flex-1">
        <ApiReferenceReact configuration={configuration} />
      </div>
    </div>
  )
}
