# Generate Luma app icon, tray icon and .ico (solid system blue + white "L").
# Usage: powershell -NoProfile -File scripts\make-icons.ps1
param()

$ErrorActionPreference = 'Stop'
Add-Type -AssemblyName System.Drawing

$root = Split-Path -Parent $PSScriptRoot
$assets = [System.IO.Path]::Combine($root, 'crates\luma-desktop\assets')
if (-not (Test-Path -LiteralPath $assets)) {
    New-Item -ItemType Directory -Path $assets -Force | Out-Null
}

function New-RoundedPath([double]$x, [double]$y, [double]$w, [double]$h, [double]$r) {
    $path = New-Object System.Drawing.Drawing2D.GraphicsPath
    $d = $r * 2
    $path.AddArc($x, $y, $d, $d, 180, 90)
    $path.AddArc($x + $w - $d, $y, $d, $d, 270, 90)
    $path.AddArc($x + $w - $d, $y + $h - $d, $d, $d, 0, 90)
    $path.AddArc($x, $y + $h - $d, $d, $d, 90, 90)
    $path.CloseFigure()
    return $path
}

function New-LumaIcon([int]$size, [string]$outPath) {
    $bitmap = New-Object System.Drawing.Bitmap $size, $size, ([System.Drawing.Imaging.PixelFormat]::Format32bppArgb)
    $graphics = [System.Drawing.Graphics]::FromImage($bitmap)
    $graphics.SmoothingMode = [System.Drawing.Drawing2D.SmoothingMode]::AntiAlias
    $graphics.Clear([System.Drawing.Color]::Transparent)

    $scale = $size / 256.0
    $background = New-RoundedPath 0 0 $size $size (58 * $scale)
    $blue = New-Object System.Drawing.SolidBrush ([System.Drawing.Color]::FromArgb(255, 0, 113, 227))
    $graphics.FillPath($blue, $background)

    $white = New-Object System.Drawing.SolidBrush ([System.Drawing.Color]::White)
    $barRadius = 10 * $scale

    $vertical = New-RoundedPath (84 * $scale) (72 * $scale) (38 * $scale) (114 * $scale) $barRadius
    $graphics.FillPath($white, $vertical)

    $horizontal = New-RoundedPath (84 * $scale) (148 * $scale) (96 * $scale) (38 * $scale) $barRadius
    $graphics.FillPath($white, $horizontal)

    $bitmap.Save($outPath, [System.Drawing.Imaging.ImageFormat]::Png)
    $graphics.Dispose()
    $bitmap.Dispose()
    Write-Output ("wrote {0}" -f $outPath)
}

$pngPath = [System.IO.Path]::Combine($assets, 'app-icon.png')
$icoPath = [System.IO.Path]::Combine($assets, 'app.ico')

New-LumaIcon 256 $pngPath
New-LumaIcon 64 ([System.IO.Path]::Combine($assets, 'tray.png'))

# Build a single-image .ico (256x256 PNG payload, supported since Vista).
$png = [System.IO.File]::ReadAllBytes($pngPath)
$stream = New-Object System.IO.MemoryStream
$writer = New-Object System.IO.BinaryWriter($stream)
$writer.Write([UInt16]0)
$writer.Write([UInt16]1)
$writer.Write([UInt16]1)
$writer.Write([Byte]0)
$writer.Write([Byte]0)
$writer.Write([Byte]0)
$writer.Write([Byte]0)
$writer.Write([UInt16]1)
$writer.Write([UInt16]32)
$writer.Write([UInt32]$png.Length)
$writer.Write([UInt32]22)
$writer.Write($png)
$writer.Flush()
[System.IO.File]::WriteAllBytes($icoPath, $stream.ToArray())
$writer.Dispose()
$stream.Dispose()
Write-Output ("wrote {0}" -f $icoPath)
