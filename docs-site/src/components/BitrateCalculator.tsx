// PyroWave bitrate for a mode at a chosen bits per pixel, and what each link leaves of it. The
// formula mirrors the host's `pyrowave_pin_kbps` (crates/punktfunk-host/src/native.rs): 4:4:4
// ×1.625, 10-bit ×1.15, clamped to [0.5 Mbps, 8 Gbps]. A link that cannot carry the pin gets
// `min(pin, 0.7 × delivered)` (`abr::probe::wall_ceiling_kbps`).
import { useId, useState, type ReactNode } from 'react'

type Preset = { label: string; w: number; h: number }

const RES_PRESETS: Preset[] = [
  { label: '1280 × 800 — Steam Deck', w: 1280, h: 800 },
  { label: '1920 × 1080 — 1080p', w: 1920, h: 1080 },
  { label: '2560 × 1440 — 1440p', w: 2560, h: 1440 },
  { label: '3440 × 1440 — ultrawide', w: 3440, h: 1440 },
  { label: '3840 × 2160 — 4K', w: 3840, h: 2160 },
  { label: '5120 × 1440 — super-ultrawide', w: 5120, h: 1440 },
]

const FPS_PRESETS = [30, 60, 90, 120, 144, 240]

// Practical payload ceilings, a bit under line rate.
const LINKS = [
  { label: 'Gigabit', mbps: 940 },
  { label: '2.5 GbE', mbps: 2350 },
  { label: '5 GbE', mbps: 4700 },
  { label: '10 GbE', mbps: 9400 },
]

const DEFAULT_BPP = 1.6
const WALL_SHARE = 0.7
const MIN_MBPS = 0.5
const MAX_MBPS = 8000

function pinMbps(pxPerSec: number, bpp: number, chroma444: boolean, tenBit: boolean): number {
  let b = bpp
  if (chroma444) b *= 1.625
  if (tenBit) b *= 1.15
  return Math.min(Math.max((pxPerSec * b) / 1e6, MIN_MBPS), MAX_MBPS)
}

const fmtRate = (mbps: number) =>
  mbps >= 1000 ? `${(mbps / 1000).toFixed(2)} Gbps` : `${Math.round(mbps)} Mbps`

const control =
  'h-9 w-full rounded-lg border border-fd-border bg-fd-background px-3 text-sm text-fd-foreground outline-none transition-colors focus-visible:border-fd-ring focus-visible:ring-2 focus-visible:ring-fd-ring/30'

function Field({
  label,
  htmlFor,
  aside,
  className = '',
  children,
}: {
  label: string
  htmlFor?: string
  aside?: ReactNode
  className?: string
  children: ReactNode
}) {
  return (
    <div className={className}>
      <div className="mb-1.5 flex h-5 items-center justify-between gap-2">
        <label
          htmlFor={htmlFor}
          className="text-xs font-medium uppercase leading-none tracking-wide text-fd-muted-foreground"
        >
          {label}
        </label>
        {aside}
      </div>
      {children}
    </div>
  )
}

function Select({
  id,
  value,
  onChange,
  children,
}: {
  id: string
  value: string
  onChange: (value: string) => void
  children: ReactNode
}) {
  return (
    <div className="relative">
      <select
        id={id}
        value={value}
        onChange={(e) => onChange(e.target.value)}
        className={`${control} cursor-pointer appearance-none pr-9`}
      >
        {children}
      </select>
      <svg
        aria-hidden
        viewBox="0 0 16 16"
        className="pointer-events-none absolute right-3 top-1/2 size-4 -translate-y-1/2 text-fd-muted-foreground"
        fill="none"
        stroke="currentColor"
        strokeWidth="1.5"
        strokeLinecap="round"
        strokeLinejoin="round"
      >
        <path d="m4 6 4 4 4-4" />
      </svg>
    </div>
  )
}

function Segmented<T extends string>({
  label,
  value,
  options,
  onChange,
}: {
  label: string
  value: T
  options: { value: T; label: string }[]
  onChange: (value: T) => void
}) {
  return (
    <div
      role="radiogroup"
      aria-label={label}
      className="flex h-9 gap-0.5 rounded-lg border border-fd-border bg-fd-background p-0.5"
    >
      {options.map((o) => {
        const active = o.value === value
        return (
          <button
            key={o.value}
            type="button"
            role="radio"
            aria-checked={active}
            onClick={() => onChange(o.value)}
            className={`flex-1 whitespace-nowrap rounded-md px-2 text-sm font-medium transition-colors focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-fd-ring/50 ${
              active
                ? 'bg-fd-primary text-fd-background shadow-sm'
                : 'text-fd-muted-foreground hover:bg-fd-accent hover:text-fd-foreground'
            }`}
          >
            {o.label}
          </button>
        )
      })}
    </div>
  )
}

export default function BitrateCalculator() {
  const id = useId()
  const [preset, setPreset] = useState('1')
  const [cw, setCw] = useState(1920)
  const [ch, setCh] = useState(1080)
  const [fps, setFps] = useState(60)
  const [chroma, setChroma] = useState<'420' | '444'>('420')
  const [depth, setDepth] = useState<'8' | '10'>('8')
  const [bpp, setBpp] = useState(DEFAULT_BPP)

  const custom = preset === 'custom'
  const p = RES_PRESETS[Number(preset)] ?? RES_PRESETS[1]!
  const w = custom ? cw : p.w
  const h = custom ? ch : p.h

  const pxPerSec = w > 0 && h > 0 && fps > 0 ? w * h * fps : 0
  const pin = pxPerSec > 0 ? pinMbps(pxPerSec, bpp, chroma === '444', depth === '10') : 0
  const frameKB = fps > 0 ? (pin * 1e6) / 8 / fps / 1000 : 0

  return (
    <div className="not-prose my-6 rounded-xl border border-fd-border bg-fd-card p-5 text-fd-card-foreground">
      <div className="grid gap-4 sm:grid-cols-2">
        <Field label="Resolution" htmlFor={`${id}-res`} className="sm:col-span-2">
          <Select id={`${id}-res`} value={preset} onChange={setPreset}>
            {RES_PRESETS.map((r, i) => (
              <option key={r.label} value={String(i)}>
                {r.label}
              </option>
            ))}
            <option value="custom">Custom…</option>
          </Select>
        </Field>

        {custom && (
          <>
            <Field label="Width" htmlFor={`${id}-w`}>
              <input
                id={`${id}-w`}
                className={control}
                type="number"
                inputMode="numeric"
                min={128}
                value={cw}
                onChange={(e) => setCw(Math.max(0, Number(e.target.value)))}
              />
            </Field>
            <Field label="Height" htmlFor={`${id}-h`}>
              <input
                id={`${id}-h`}
                className={control}
                type="number"
                inputMode="numeric"
                min={128}
                value={ch}
                onChange={(e) => setCh(Math.max(0, Number(e.target.value)))}
              />
            </Field>
          </>
        )}

        <Field label="Frame rate" htmlFor={`${id}-fps`}>
          <Select id={`${id}-fps`} value={String(fps)} onChange={(v) => setFps(Number(v))}>
            {FPS_PRESETS.map((f) => (
              <option key={f} value={f}>
                {f} fps
              </option>
            ))}
          </Select>
        </Field>

        <Field
          label="Bits per pixel"
          htmlFor={`${id}-bpp`}
          aside={
            <span className="flex items-center gap-2 text-sm leading-none tabular-nums">
              {bpp !== DEFAULT_BPP && (
                <button
                  type="button"
                  onClick={() => setBpp(DEFAULT_BPP)}
                  className="text-xs text-fd-muted-foreground underline-offset-2 hover:text-fd-foreground hover:underline"
                >
                  reset
                </button>
              )}
              <span className="font-semibold text-fd-foreground">{bpp.toFixed(2)}</span>
            </span>
          }
        >
          <div className="flex h-9 items-center">
            <input
              id={`${id}-bpp`}
              type="range"
              min={0.25}
              max={4}
              step={0.05}
              value={bpp}
              onChange={(e) => setBpp(Number(e.target.value))}
              className="w-full cursor-pointer rounded-full accent-fd-primary focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-fd-ring/50"
            />
          </div>
        </Field>

        <Field label="Chroma">
          <Segmented
            label="Chroma"
            value={chroma}
            onChange={setChroma}
            options={[
              { value: '420', label: '4:2:0' },
              { value: '444', label: '4:4:4' },
            ]}
          />
        </Field>

        <Field label="Bit depth">
          <Segmented
            label="Bit depth"
            value={depth}
            onChange={setDepth}
            options={[
              { value: '8', label: '8-bit' },
              { value: '10', label: '10-bit / HDR' },
            ]}
          />
        </Field>
      </div>

      <div className="mt-5 flex flex-wrap items-baseline gap-x-4 gap-y-1 border-t border-fd-border pt-5">
        <div className="text-3xl font-bold tabular-nums text-fd-primary">≈ {fmtRate(pin)}</div>
        <div className="text-sm tabular-nums text-fd-muted-foreground">
          {frameKB >= 1000 ? `${(frameKB / 1000).toFixed(2)} MB` : `${Math.round(frameKB)} KB`}{' '}
          per frame
        </div>
      </div>

      <div className="mt-4 grid grid-cols-[4.5rem_1fr_auto] items-center gap-x-3 gap-y-2.5">
        {LINKS.map((l) => {
          const rate = Math.min(pin, l.mbps * WALL_SHARE)
          const kept = pin > 0 ? rate / pin : 0
          const full = kept >= 1
          return (
            <div key={l.label} className="contents">
              <span className="text-xs font-medium text-fd-muted-foreground">{l.label}</span>
              <div className="h-2 overflow-hidden rounded-full bg-fd-muted-foreground/15">
                <div
                  className={`h-full rounded-full bg-fd-primary ${full ? '' : 'opacity-50'}`}
                  style={{ width: `${Math.min(100, kept * 100)}%` }}
                />
              </div>
              <span
                className={`w-28 text-right text-xs tabular-nums ${
                  full ? 'text-fd-muted-foreground' : 'font-semibold text-fd-foreground'
                }`}
              >
                {(bpp * kept).toFixed(2)} bits/pixel
              </span>
            </div>
          )
        })}
      </div>

      <p className="mt-4 text-xs leading-relaxed text-fd-muted-foreground">
        Each bar is what an Automatic session keeps on that link. A link that can't carry the rate
        with room to spare gets 70 % of what it delivered in the test before the first frame. Link
        figures assume a payload ceiling a bit under line rate.
      </p>
    </div>
  )
}
