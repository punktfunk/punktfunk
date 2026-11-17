/// <reference types="vite/client" />
import { createRootRoute, HeadContent, Outlet, Scripts } from '@tanstack/react-router'
import { createServerFn } from '@tanstack/react-start'
import * as React from 'react'
import { RootProvider } from 'fumadocs-ui/provider/tanstack'
import '@fontsource-variable/geist'
import Footer from '@/components/Footer'
import { type Footer as FooterData, findFooter } from '@/lib/cms'
import appCss from '@/styles/app.css?url'

// The footer is shared with the marketing site (one per-tenant CMS collection, see lib/cms.ts).
// Fetch it once at the root, server-side, falling back to null so a CMS hiccup
// never breaks the page.
const getFooter = createServerFn({ method: 'GET' }).handler(
  async (): Promise<FooterData | null> => {
    try {
      return await findFooter()
    } catch {
      return null
    }
  },
)

export const Route = createRootRoute({
  loader: async (): Promise<{ footer: FooterData | null }> => ({
    footer: await getFooter(),
  }),
  head: () => ({
    meta: [
      { charSet: 'utf-8' },
      { name: 'viewport', content: 'width=device-width, initial-scale=1' },
      { name: 'color-scheme', content: 'dark light' },
      { title: 'Punktfunk Docs' },
    ],
    links: [
      { rel: 'stylesheet', href: appCss },
      { rel: 'icon', type: 'image/svg+xml', href: '/favicon.svg' },
    ],
  }),
  component: RootComponent,
})

function RootComponent() {
  return (
    <RootDocument>
      <Outlet />
    </RootDocument>
  )
}

function RootDocument({ children }: { children: React.ReactNode }) {
  return (
    <html lang="en" suppressHydrationWarning>
      <head>
        <HeadContent />
      </head>
      <body className="flex flex-col min-h-screen">
        <RootProvider>
          {children}
          <Footer />
        </RootProvider>
        <Scripts />
      </body>
    </html>
  )
}
