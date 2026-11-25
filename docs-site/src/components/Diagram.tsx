// A diagram rendered at build time from a ```mermaid fence (source.config.ts). The SVG keeps
// its natural size, so its text stays readable; a diagram wider than the column scrolls.
export default function Diagram({ svg, caption }: { svg: string; caption: string }) {
  return (
    <figure className="not-prose my-6">
      <div
        role="img"
        aria-label={caption}
        className="overflow-x-auto rounded-xl border bg-fd-card px-2 py-1 [&>svg]:mx-auto [&>svg]:block [&>svg]:max-w-none"
        // Our own markup, generated from the page's source during the build.
        dangerouslySetInnerHTML={{ __html: svg }}
      />
      <figcaption className="mt-2 text-center text-sm text-fd-muted-foreground">{caption}</figcaption>
    </figure>
  )
}
