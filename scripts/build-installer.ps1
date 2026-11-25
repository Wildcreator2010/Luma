#requires -Version 5.1
<#
.SYNOPSIS
    构建 Luma 的 Windows 发布产物：MSI 安装包与便携 ZIP。

.EXAMPLE
    powershell -ExecutionPolicy Bypass -File .\scripts\build-installer.ps1

.EXAMPLE
    .\scripts\build-installer.ps1 -SkipBuild
#>
[CmdletBinding()]
param(
    [switch]$SkipBuild,
    [switch]$SkipPortable,
    [switch]$Full,
    [string]$Version,
    [string]$WixVersion = "6.0.2",
    [string]$BambuStudioDir,
    [string]$BambuPluginDir
)

$ErrorActionPreference = "Stop"
Set-StrictMode -Version Latest

function Assert-Under([string]$Path, [string]$Parent) {
    $full = [IO.Path]::GetFullPath($Path)
    $root = [IO.Path]::GetFullPath($Parent).TrimEnd('\') + '\'
    if (-not $full.StartsWith($root, [StringComparison]::OrdinalIgnoreCase)) {
        throw "拒绝操作工作区之外的路径：$full"
    }
    return $full
}

$root = (Resolve-Path (Join-Path $PSScriptRoot "..")).Path
$versionFile = Join-Path $root "VERSION"
if (-not $Version) {
    $Version = (Get-Content -LiteralPath $versionFile -Raw).Trim()
}
if ($Version -notmatch '^\d+\.\d+\.\d+$') {
    throw "VERSION 必须是 x.y.z：$Version"
}

$cargoToml = Get-Content -LiteralPath (Join-Path $root "Cargo.toml") -Raw
$cargoVersion = [regex]::Match(
    $cargoToml,
    '(?ms)^\[workspace\.package\].*?^version\s*=\s*"([^"]+)"'
).Groups[1].Value
if ($cargoVersion -and $cargoVersion -ne $Version) {
    throw "版本不一致：VERSION=$Version，Cargo.toml workspace.version=$cargoVersion"
}

$dist = Assert-Under (Join-Path $root "dist") $root
$editionSuffix = if ($Full) { "-full" } else { "" }
$payloadName = if ($Full) { "payload-full" } else { "payload" }
$payload = Assert-Under (Join-Path $dist $payloadName) $root
$exeSource = Join-Path $root "target\release\luma-desktop.exe"
$exe = Join-Path $payload "Luma.exe"
$msi = Join-Path $dist "Luma-$Version-win64$editionSuffix.msi"
$zip = Join-Path $dist "Luma-$Version-win64$editionSuffix-portable.zip"

$editionLabel = if ($Full) { "full" } else { "standard" }
Write-Host "Luma $Version ($editionLabel)" -ForegroundColor Cyan
Write-Host "仓库：$root"
if ($Full) {
    Write-Warning "全量版会复制 Bambu Studio / VC++ 运行库；请确认你对这些二进制有再分发权。"
}

if (-not $SkipBuild) {
    Write-Host "`n[1/4] cargo build --release" -ForegroundColor Cyan
    Push-Location $root
    $previousRustFlags = $env:RUSTFLAGS
    try {
        # 公开二进制不应泄露本机用户目录或第三方 registry 路径。
        $remap = @(
            "--remap-path-prefix=$root=.",
            "--remap-path-prefix=$env:USERPROFILE=~",
            "-C",
            "target-feature=+crt-static"
        ) -join " "
        $env:RUSTFLAGS = if ($previousRustFlags) { "$previousRustFlags $remap" } else { $remap }
        cargo build -p luma-desktop --release
        if ($LASTEXITCODE -ne 0) { throw "cargo build 失败（exit $LASTEXITCODE）" }
    }
    finally {
        $env:RUSTFLAGS = $previousRustFlags
        Pop-Location
    }
}
else {
    Write-Host "`n[1/4] 跳过构建" -ForegroundColor DarkGray
}

if (-not (Test-Path -LiteralPath $exeSource)) {
    throw "找不到 $exeSource，请先运行 cargo build --release"
}

Write-Host "`n[2/4] 组装 payload" -ForegroundColor Cyan
New-Item -ItemType Directory -Path $dist -Force | Out-Null
if (Test-Path -LiteralPath $payload) {
    Remove-Item -LiteralPath $payload -Recurse -Force
}
foreach ($artifact in @($msi, $zip)) {
    if (Test-Path -LiteralPath $artifact) {
        Remove-Item -LiteralPath $artifact -Force
    }
}
New-Item -ItemType Directory -Path $payload -Force | Out-Null
Copy-Item -LiteralPath $exeSource -Destination $exe
Copy-Item -LiteralPath (Join-Path $root "LICENSE") -Destination (Join-Path $payload "LICENSE.txt")
$readmeTemplate = Get-Content -LiteralPath (Join-Path $root "installer\README.payload.txt") -Raw -Encoding UTF8

$cameraNote = "标准安装包会在安装时尝试从本机 Bambu Studio 导入摄像头运行库；便携版可运行 Luma.exe --import-camera-runtime。未安装 Bambu Studio 时其余功能正常。"
if ($Full) {
    Write-Host "      收集 Bambu Studio / FFmpeg / VC++ 运行库" -ForegroundColor Cyan
    $studioCandidates = @()
    if ($BambuStudioDir) { $studioCandidates += $BambuStudioDir }
    if ($env:ProgramFiles) { $studioCandidates += (Join-Path $env:ProgramFiles "Bambu Studio") }
    if (${env:ProgramFiles(x86)}) {
        $studioCandidates += (Join-Path ${env:ProgramFiles(x86)} "Bambu Studio")
    }
    $studio = $studioCandidates |
        Where-Object {
            @(Get-ChildItem -LiteralPath $_ -Filter "avcodec-*.dll" -File -ErrorAction SilentlyContinue).Count -gt 0
        } |
        Select-Object -First 1
    if (-not $studio) {
        throw "找不到 Bambu Studio（需要 avcodec-*.dll）；请用 -BambuStudioDir 指定安装目录"
    }

    $pluginCandidates = @()
    if ($BambuPluginDir) { $pluginCandidates += $BambuPluginDir }
    if ($env:APPDATA) { $pluginCandidates += (Join-Path $env:APPDATA "BambuStudio\plugins") }
    $plugin = $pluginCandidates |
        Where-Object { Test-Path -LiteralPath (Join-Path $_ "BambuSource.dll") } |
        Select-Object -First 1
    if (-not $plugin) {
        throw "找不到 BambuSource.dll；请用 -BambuPluginDir 指定插件目录"
    }

    $cameraDir = Join-Path $payload "camera"
    New-Item -ItemType Directory -Path $cameraDir -Force | Out-Null

    $pluginFiles = @(
        "BambuSource.dll",
        "live555.dll",
        "libaosl.dll",
        "bambu_networking.dll",
        "agora_rtc_sdk.dll",
        "libagora-ffmpeg.dll",
        "libagora-soundtouch.dll"
    )
    foreach ($name in $pluginFiles) {
        $source = Join-Path $plugin $name
        if (-not (Test-Path -LiteralPath $source)) {
            throw "摄像头依赖缺失：$source"
        }
        Copy-Item -LiteralPath $source -Destination (Join-Path $cameraDir $name)
    }

    $ffmpeg = @(
        Get-ChildItem -LiteralPath $studio -File |
            Where-Object { $_.Name -match '^(avcodec|avutil|swscale|swresample)-\d+\.dll$' }
    )
    if ($ffmpeg.Count -lt 4) {
        throw "Bambu Studio 目录缺少完整 FFmpeg DLL：$studio"
    }
    foreach ($file in $ffmpeg) {
        Copy-Item -LiteralPath $file.FullName -Destination (Join-Path $cameraDir $file.Name)
    }

    $system32 = Join-Path $env:WINDIR "System32"
    $vcFiles = @(
        "msvcp140.dll",
        "msvcp140_1.dll",
        "msvcp140_2.dll",
        "vcruntime140.dll",
        "vcruntime140_1.dll",
        "concrt140.dll"
    )
    foreach ($name in $vcFiles) {
        $source = Join-Path $system32 $name
        if (Test-Path -LiteralPath $source) {
            Copy-Item -LiteralPath $source -Destination (Join-Path $cameraDir $name)
        }
    }
    foreach ($required in @("msvcp140.dll", "vcruntime140.dll", "vcruntime140_1.dll")) {
        if (-not (Test-Path -LiteralPath (Join-Path $cameraDir $required))) {
            throw "缺少 VC++ 运行库：$required"
        }
    }

    Copy-Item -LiteralPath (Join-Path $root "installer\THIRD_PARTY_NOTICES.txt") `
        -Destination (Join-Path $cameraDir "THIRD_PARTY_NOTICES.txt")
    Copy-Item -LiteralPath (Join-Path $root "installer\AGPL-3.0.txt") `
        -Destination (Join-Path $cameraDir "AGPL-3.0.txt")
    $cameraNote = "本全量版已内置摄像头运行库（BambuSource、live555、FFmpeg、VC++ Runtime），无需另装 Bambu Studio。第三方组件许可见 camera\THIRD_PARTY_NOTICES.txt。"
    Write-Host ("      camera bundle {0:N1} MiB" -f ((Get-ChildItem $cameraDir -File | Measure-Object Length -Sum).Sum / 1MB))
}

$readme = $readmeTemplate.Replace("{{VERSION}}", $Version).Replace("{{CAMERA_NOTE}}", $cameraNote)
$readme | Set-Content -LiteralPath (Join-Path $payload "README.txt") -Encoding UTF8

$info = (Get-Item -LiteralPath $exe).VersionInfo
if ($info.ProductVersion -and $info.ProductVersion -ne $Version) {
    Write-Warning "exe ProductVersion=$($info.ProductVersion)，VERSION=$Version"
}
if (-not $info.ProductVersion) {
    throw "exe 缺少版本资源；检查 build.rs / rc.exe"
}

# 静态 CRT 检查：安装包不附带 VC++ 运行库。
$ascii = [Text.Encoding]::ASCII.GetString([IO.File]::ReadAllBytes($exe))
if ($ascii.Contains("VCRUNTIME140.dll")) {
    throw "exe 仍依赖 VCRUNTIME140.dll；检查 .cargo/config.toml 的 +crt-static"
}
Write-Host ("      exe {0:N1} MiB，版本资源 {1}" -f ((Get-Item $exe).Length / 1MB), $info.ProductVersion)

if (-not $SkipPortable) {
    Write-Host "`n[3/4] 便携 ZIP" -ForegroundColor Cyan
    Compress-Archive -Path (Join-Path $payload "*") -DestinationPath $zip -CompressionLevel Optimal -Force
}
else {
    Write-Host "`n[3/4] 跳过便携 ZIP" -ForegroundColor DarkGray
}

Write-Host "`n[4/4] WiX MSI" -ForegroundColor Cyan
if (-not (Get-Command wix -ErrorAction SilentlyContinue)) {
    if (-not (Get-Command dotnet -ErrorAction SilentlyContinue)) {
        throw "缺少 WiX。请安装 .NET SDK 后执行：dotnet tool install --global wix --version $WixVersion"
    }
    Write-Host "      未找到 wix，尝试安装 dotnet tool…"
    dotnet tool install --global wix --version $WixVersion
    if ($LASTEXITCODE -ne 0) { throw "WiX 安装失败" }
    $env:Path = "$env:USERPROFILE\.dotnet\tools;$env:Path"
}
wix extension add -g "WixToolset.UI.wixext/$WixVersion" | Out-Null

$wxs = Join-Path $root "installer\Product.wxs"
$icon = Join-Path $root "crates\luma-desktop\assets\app.ico"
$licenseRtf = Join-Path $root "installer\License.rtf"

$wixArgs = @(
    "build",
    "-arch", "x64",
    "-ext", "WixToolset.UI.wixext",
    "-culture", "zh-CN",
    "-pdbtype", "none",
    "-d", "ProductVersion=$Version",
    "-d", "PayloadDir=$payload",
    "-d", "BundleCamera=$(if ($Full) { 1 } else { 0 })",
    "-d", "IconPath=$icon",
    "-d", "LicenseRtf=$licenseRtf",
    "-o", $msi,
    $wxs
)
& wix @wixArgs
if ($LASTEXITCODE -ne 0) { throw "wix build 失败（exit $LASTEXITCODE）" }

Write-Host "`n产物" -ForegroundColor Green
$artifacts = @(Get-ChildItem -LiteralPath $dist -File | Where-Object { $_.Name -ne "SHA256SUMS.txt" })
$checksums = foreach ($file in $artifacts) {
    $hash = (Get-FileHash -LiteralPath $file.FullName -Algorithm SHA256).Hash.ToLowerInvariant()
    Write-Host ("  {0}  ({1:N1} MiB)" -f $file.Name, ($file.Length / 1MB))
    "{0}  {1}" -f $hash, $file.Name
}
$checksums | Set-Content -LiteralPath (Join-Path $dist "SHA256SUMS.txt") -Encoding ASCII
Get-Content -LiteralPath (Join-Path $dist "SHA256SUMS.txt")
