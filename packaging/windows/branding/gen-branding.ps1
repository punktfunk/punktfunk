<#
.SYNOPSIS
  Generate the punktfunk host installer icon.

.DESCRIPTION
  Renders the punktfunk brand mark - the two overlapping circles ("lens") from
  web/src/components/brand-mark.tsx (the canonical flattened geometry, shared with the Apple icon,
  the marketing site and the docs) into:

    punktfunk.ico        multi-size icon (16..256; only the 128 and 256 entries are PNG-compressed,
                         the rest classic DIB). Shipped in {app} and used for the Apps & Features
                         entry; the tray's state icons are drawn separately, not by this script.

  Outputs are COMMITTED next to this script (like include/punktfunk_core.h, generated-but-checked-in);
  re-run only when the brand changes. Everything is drawn 4x supersampled and downscaled
  (System.Drawing regions/clips do not antialias), so edges stay clean at every size.

.EXAMPLE
  pwsh -File packaging/windows/branding/gen-branding.ps1
#>
[CmdletBinding()]
param([string]$OutDir = $PSScriptRoot)
$ErrorActionPreference = 'Stop'
Add-Type -AssemblyName System.Drawing

# --- brand constants (colors from brand-mark.tsx; tile background from the MSIX assets) -------
$colLight = [System.Drawing.Color]::FromArgb(255, 0xA7, 0x9F, 0xF8)   # large circle
$colDeep = [System.Drawing.Color]::FromArgb(255, 0x6C, 0x5B, 0xF3)   # small circle
$colHi = [System.Drawing.Color]::FromArgb(255, 0xD2, 0xC9, 0xFB)   # lens overlap highlight
$colTile = [System.Drawing.Color]::FromArgb(255, 0x1C, 0x15, 0x30)   # brand tile background

# Mark geometry in the 1000-unit viewbox of brand-mark.tsx: two r=194.41 circles at (403.04,597.26)
# (light, behind) and (597.81,402.85) (deep, in front), their intersection filled as the highlight.
$R = 194.41
$c1x = 403.037; $c1y = 597.262
$c2x = 597.8075; $c2y = 402.8525
# Mark bounding box -> center/span, so callers can place it by center + size.
$bbMinX = $c1x - $R; $bbMaxX = $c2x + $R
$bbMinY = $c2y - $R; $bbMaxY = $c1y + $R
$markCx = ($bbMinX + $bbMaxX) / 2.0
$markCy = ($bbMinY + $bbMaxY) / 2.0
$markSpan = $bbMaxX - $bbMinX   # ~= $bbMaxY - $bbMinY (square to within 0.4 units)

# Draw the mark onto $g centered at ($cx,$cy) with bounding-box size $size (device pixels).
function Draw-Mark([System.Drawing.Graphics]$g, [double]$cx, [double]$cy, [double]$size) {
    $s = $size / $markSpan
    function ellRect([double]$ecx, [double]$ecy) {
        $r = $R * $s
        [System.Drawing.RectangleF]::new(
            [float]($cx + ($ecx - $markCx) * $s - $r), [float]($cy + ($ecy - $markCy) * $s - $r),
            [float](2 * $r), [float](2 * $r))
    }
    $r1 = ellRect $c1x $c1y
    $r2 = ellRect $c2x $c2y
    $b = New-Object System.Drawing.SolidBrush($colLight)
    $g.FillEllipse($b, $r1); $b.Dispose()
    $b = New-Object System.Drawing.SolidBrush($colDeep)
    $g.FillEllipse($b, $r2); $b.Dispose()
    # Highlight = intersection: clip to circle 1, fill circle 2. The clip edge is not antialiased,
    # but every caller renders 4x supersampled and downscales, which smooths it.
    $p1 = New-Object System.Drawing.Drawing2D.GraphicsPath
    $p1.AddEllipse($r1)
    $g.SetClip($p1)
    $b = New-Object System.Drawing.SolidBrush($colHi)
    $g.FillEllipse($b, $r2); $b.Dispose()
    $g.ResetClip(); $p1.Dispose()
}

# New 32bpp canvas + antialiased Graphics.
function New-Canvas([int]$w, [int]$h) {
    $bmp = New-Object System.Drawing.Bitmap($w, $h, [System.Drawing.Imaging.PixelFormat]::Format32bppArgb)
    $g = [System.Drawing.Graphics]::FromImage($bmp)
    $g.SmoothingMode = [System.Drawing.Drawing2D.SmoothingMode]::AntiAlias
    $g.TextRenderingHint = [System.Drawing.Text.TextRenderingHint]::AntiAlias
    @($bmp, $g)
}

# Downscale $src to $w x $h (high-quality bicubic) - the supersample resolve.
function Resize-Bitmap([System.Drawing.Bitmap]$src, [int]$w, [int]$h) {
    $dst = New-Object System.Drawing.Bitmap($w, $h, [System.Drawing.Imaging.PixelFormat]::Format32bppArgb)
    $g = [System.Drawing.Graphics]::FromImage($dst)
    $g.InterpolationMode = [System.Drawing.Drawing2D.InterpolationMode]::HighQualityBicubic
    $g.PixelOffsetMode = [System.Drawing.Drawing2D.PixelOffsetMode]::HighQuality
    $g.DrawImage($src, (New-Object System.Drawing.Rectangle(0, 0, $w, $h)),
        0, 0, $src.Width, $src.Height, [System.Drawing.GraphicsUnit]::Pixel)
    $g.Dispose()
    $dst
}

$SS = 4   # supersample factor

# --- punktfunk.ico: rounded brand tile at 16..256 --------------------------------------------
# Small sizes are classic 32bpp DIB entries (older shell consumers reject an all-PNG icon); only
# 128/256 use PNG compression (the standard Vista+ layout).
function New-IconTile([int]$sz) {
    $bmp, $g = New-Canvas ($sz * $SS) ($sz * $SS)
    # Rounded-rect tile (22% corner radius - the Windows 11 app-icon look).
    $S = $sz * $SS; $rad = [int]($S * 0.22)
    $path = New-Object System.Drawing.Drawing2D.GraphicsPath
    $path.AddArc(0, 0, 2 * $rad, 2 * $rad, 180, 90)
    $path.AddArc($S - 2 * $rad, 0, 2 * $rad, 2 * $rad, 270, 90)
    $path.AddArc($S - 2 * $rad, $S - 2 * $rad, 2 * $rad, 2 * $rad, 0, 90)
    $path.AddArc(0, $S - 2 * $rad, 2 * $rad, 2 * $rad, 90, 90)
    $path.CloseFigure()
    $b = New-Object System.Drawing.SolidBrush($colTile)
    $g.FillPath($b, $path); $b.Dispose(); $path.Dispose()
    Draw-Mark $g ($S / 2.0) ($S / 2.0) ($S * 0.74)
    $g.Dispose()
    $out = Resize-Bitmap $bmp $sz $sz
    $bmp.Dispose()
    $out
}

# PNG-compressed entry payload (used for the 128/256 entries). The leading comma keeps the byte[]
# a single pipeline object (PowerShell would otherwise unroll it into individual bytes).
function ConvertTo-IconPng([System.Drawing.Bitmap]$tile) {
    $ms = New-Object System.IO.MemoryStream
    $tile.Save($ms, [System.Drawing.Imaging.ImageFormat]::Png)
    return , $ms.ToArray()
}

# Classic ICO DIB entry payload: BITMAPINFOHEADER (height doubled) + bottom-up 32bpp BGRA XOR data
# + an all-zero 1bpp AND mask (32bpp icons carry transparency in the alpha channel).
function ConvertTo-IconDib([System.Drawing.Bitmap]$tile) {
    $s = $tile.Width
    $rect = New-Object System.Drawing.Rectangle(0, 0, $s, $s)
    $data = $tile.LockBits($rect, [System.Drawing.Imaging.ImageLockMode]::ReadOnly,
        [System.Drawing.Imaging.PixelFormat]::Format32bppArgb)
    $px = New-Object byte[] ($data.Stride * $s)
    [System.Runtime.InteropServices.Marshal]::Copy($data.Scan0, $px, 0, $px.Length)
    $tile.UnlockBits($data)
    $maskStride = [int][Math]::Ceiling($s / 32.0) * 4     # 1bpp rows padded to 32 bits
    $ms = New-Object System.IO.MemoryStream
    $w = New-Object System.IO.BinaryWriter($ms)
    $w.Write([uint32]40); $w.Write([int]$s); $w.Write([int]($s * 2))   # biSize, biWidth, biHeight (XOR+AND)
    $w.Write([uint16]1); $w.Write([uint16]32); $w.Write([uint32]0)     # biPlanes, biBitCount, BI_RGB
    $w.Write([uint32]($s * $s * 4 + $maskStride * $s))                 # biSizeImage
    $w.Write([int]0); $w.Write([int]0); $w.Write([uint32]0); $w.Write([uint32]0)
    for ($y = $s - 1; $y -ge 0; $y--) { $w.Write($px, $y * $data.Stride, $s * 4) }   # XOR, bottom-up
    $w.Write((New-Object byte[] ($maskStride * $s)))                   # AND mask: all opaque
    $w.Flush()
    $bytes = $ms.ToArray()
    $w.Dispose(); $ms.Dispose()
    return , $bytes   # leading comma: emit the byte[] as ONE object, not unrolled bytes
}

$icoSizes = 16, 20, 24, 32, 40, 48, 64, 128, 256
$pngs = @(foreach ($s in $icoSizes) {
    $tile = New-IconTile $s
    if ($s -ge 128) { ConvertTo-IconPng $tile } else { ConvertTo-IconDib $tile }
    $tile.Dispose()
})
$ico = New-Object System.IO.MemoryStream
$bw = New-Object System.IO.BinaryWriter($ico)
# ICONDIR
$bw.Write([uint16]0); $bw.Write([uint16]1); $bw.Write([uint16]$icoSizes.Count)
# ICONDIRENTRYs (width/height byte 0 means 256)
$offset = 6 + 16 * $icoSizes.Count
for ($i = 0; $i -lt $icoSizes.Count; $i++) {
    $s = $icoSizes[$i]
    $dim = if ($s -ge 256) { 0 } else { $s }
    $bw.Write([byte]$dim); $bw.Write([byte]$dim)          # width, height
    $bw.Write([byte]0); $bw.Write([byte]0)                # colors, reserved
    $bw.Write([uint16]1); $bw.Write([uint16]32)           # planes, bitcount
    $bw.Write([uint32]$pngs[$i].Length); $bw.Write([uint32]$offset)
    $offset += $pngs[$i].Length
}
foreach ($p in $pngs) { $bw.Write([byte[]]$p) }
$bw.Flush()
$icoPath = Join-Path $OutDir 'punktfunk.ico'
[IO.File]::WriteAllBytes($icoPath, $ico.ToArray())
$bw.Dispose(); $ico.Dispose()
# Self-check: a malformed container here would only surface later as a shell that draws nothing.
$probe = New-Object System.Drawing.Icon($icoPath)
$probe.Dispose()
Write-Host "  wrote $icoPath ($($icoSizes -join ',')) - verified loadable"
Write-Host "==> branding assets generated in $OutDir"
