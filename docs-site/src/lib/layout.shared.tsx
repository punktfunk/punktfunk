import type { BaseLayoutProps } from 'fumadocs-ui/layouts/shared'
import BrandMark from '@/components/BrandMark'
import Wordmark from '@/components/Wordmark'

// Shared chrome (nav title, links) for both the docs layout and the home layout.
// The lens mark + wordmark mirror the Punktfunk marketing site's header.
export function baseOptions(): BaseLayoutProps {
  return {
    nav: {
      title: (
        <>
          <BrandMark className="size-6" />
          <Wordmark className="h-4" />
        </>
      ),
    },
    links: [
      { text: 'Docs', url: '/docs' },
      { text: 'API', url: '/api' },
      { text: 'Website', url: 'https://punktfunk.unom.io' },
      { text: 'Donate', url: 'https://ko-fi.com/punktfunk' },
      { text: 'Source code', url: 'https://git.unom.io/unom/punktfunk' },
      { text: 'Discord', url: 'https://discord.gg/wzEGg9y45z' },
      { text: 'Reddit', url: 'https://www.reddit.com/r/Punktfunk/' },
    ],
  }
}
