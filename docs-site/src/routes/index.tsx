import { createFileRoute, Link } from '@tanstack/react-router'
import { HomeLayout } from 'fumadocs-ui/layouts/home'
import BrandMark from '@/components/BrandMark'
import Wordmark from '@/components/Wordmark'
import { baseOptions } from '@/lib/layout.shared'

export const Route = createFileRoute('/')({ component: Home })

function Home() {
  return (
    <HomeLayout {...baseOptions()}>
      <main className="flex flex-1 flex-col items-center justify-center gap-6 px-4 py-24 text-center">
        <BrandMark className="size-20 drop-shadow-[0_8px_30px_rgba(108,91,243,0.45)]" />
        <Wordmark className="h-12 md:h-14" />
        <p className="max-w-xl text-fd-muted-foreground">
          Low-latency desktop and game streaming with first-class Linux and Windows
          hosts — a shared Rust protocol core with native clients on every platform.
        </p>
        <Link
          to="/docs/$"
          params={{ _splat: '' }}
          className="rounded-lg bg-brand px-5 py-2.5 font-medium text-white transition-colors hover:bg-brand/90"
        >
          Read the docs
        </Link>
        <nav className="flex gap-4 text-sm text-fd-muted-foreground">
          {[
            ['Quick Start', 'quickstart'],
            ['Troubleshooting', 'troubleshooting'],
            ['Reference', 'support-matrix'],
            ['Developers', 'developers'],
          ].map(([label, slug]) => (
            <Link
              key={slug}
              to="/docs/$"
              params={{ _splat: slug }}
              className="underline-offset-4 hover:text-fd-foreground hover:underline"
            >
              {label}
            </Link>
          ))}
        </nav>
      </main>
    </HomeLayout>
  )
}
