import { renderMermaidSVG } from 'beautiful-mermaid'
import { defineDocs, defineConfig } from 'fumadocs-mdx/config'

// Single docs collection: every .md/.mdx under content/docs/. Frontmatter `title` is
// required (Fumadocs renders it as the page heading); `description` is optional.
export const docs = defineDocs({
  dir: 'content/docs',
})

interface Node {
  type: string
  lang?: string | null
  meta?: string | null
  value?: string
  children?: Node[]
}

// The docs text column at a desktop width. A wider diagram would scroll, so it fails the build.
const COLUMN_PX = 780

// A ```mermaid fence becomes an inline SVG at build time, so a page ships no diagram library.
// Its info string is the caption and the accessible name (docs-site/README.md). Colours are
// the site's own tokens, so a diagram follows light and dark.
function remarkDiagram() {
  const walk = (node: Node) => {
    node.children?.forEach((child, i, all) => {
      if (child.type === 'code' && child.lang === 'mermaid') all[i] = diagram(child)
      else walk(child)
    })
  }
  return walk
}

function diagram(node: Node): Node {
  const caption = node.meta?.trim()
  if (!caption) throw new Error(`a mermaid fence needs a caption:\n${node.value}`)
  const svg = renderMermaidSVG(node.value ?? '', {
    bg: 'var(--color-fd-background)',
    fg: 'var(--color-fd-foreground)',
    accent: 'var(--color-fd-primary)',
    muted: 'var(--color-fd-muted-foreground)',
    transparent: true,
    font: 'Geist Variable',
    padding: 24,
  })
    // The renderer imports its font from Google Fonts. The site serves Geist itself.
    .replace(/^\s*@import url\([^\n]*\n/m, '')
  const width = Number(svg.match(/ width="([\d.]+)"/)?.[1])
  if (!(width <= COLUMN_PX)) {
    throw new Error(`"${caption}" is ${Math.round(width)} px wide; the column is ${COLUMN_PX}. Go top-down or shorten labels.`)
  }
  return {
    type: 'mdxJsxFlowElement',
    name: 'Diagram',
    attributes: [
      { type: 'mdxJsxAttribute', name: 'svg', value: svg },
      { type: 'mdxJsxAttribute', name: 'caption', value: caption },
    ],
    children: [],
  } as Node
}

// Global MDX options (remark/rehype). Default export is read by the fumadocs-mdx vite plugin.
export default defineConfig({
  mdxOptions: {
    remarkPlugins: [remarkDiagram],
  },
})
