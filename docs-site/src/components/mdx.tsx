import defaultMdxComponents from 'fumadocs-ui/mdx'
import { Tab, Tabs } from 'fumadocs-ui/components/tabs'
import type { MDXComponents } from 'mdx/types'
import BitrateCalculator from '@/components/BitrateCalculator'
import Diagram from '@/components/Diagram'
import { Install, Installer, Ports } from '@/components/platforms'

export function getMDXComponents(components?: MDXComponents) {
  return {
    ...defaultMdxComponents,
    // Custom components usable in any .md/.mdx without a per-file import.
    BitrateCalculator,
    // What a ```mermaid fence compiles to (source.config.ts); never written by hand.
    Diagram,
    // Install commands / port table quoted from data/platforms.json: <Install platform="debian" />, <Ports />
    Install,
    Installer,
    Ports,
    // Per-platform instructions: <Tabs items={['Linux', 'Windows']}><Tab value="Linux">…
    Tabs,
    Tab,
    ...components,
  } satisfies MDXComponents
}

export const useMDXComponents = getMDXComponents

declare global {
  type MDXProvidedComponents = ReturnType<typeof getMDXComponents>
}
