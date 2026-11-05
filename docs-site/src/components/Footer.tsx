import { getRouteApi } from '@tanstack/react-router'
import { FooterView } from '@unom/app-ui/footer'

const rootApi = getRouteApi('__root__')

// Footer markup is shared with the marketing site via @unom/app-ui so the two
// stay in sync. It themes itself through @unom/style tokens, which the docs map
// onto their Fumadocs surfaces. Root-relative links target the website's
// English pages, the docs' own language (the docs don't host /legal/* etc.).
const SITE_URL = 'https://punktfunk.unom.io/en'
const resolveHref = (to: string) =>
  to.startsWith('/') ? `${SITE_URL}${to}` : to

export default function Footer() {
  const { footer } = rootApi.useLoaderData()

  return (
    <FooterView
      sections={footer?.sections}
      tagline={footer?.tagline}
      socials={footer?.socials}
      socialsLabel="Socials"
      resolveHref={resolveHref}
      className="border-t border-fd-border"
    />
  )
}
