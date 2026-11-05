# Build and package a HTTPorter release: the portable zip, the installer and their checksums.
#   powershell -NoProfile -File tools/package.ps1 [-TargetDir <dir>] [-OutDir <dir>]
#
# Steps:
#   1. cargo build --release --locked -p httporter-app
#   2. note the exe size (no budget: the user prefers function over a smaller exe)
#   3. stage dist\HTTPorter-<version>\ (exe, LICENSE-MIT.txt, LICENSE-APACHE.txt,
#      THIRD-PARTY-NOTICES.txt from tools/third_party.py, README.txt)
#   4. tools/sign.ps1 on the staged exe
#   5. dist\HTTPorter-<version>-windows-x64-portable.zip
#   6. Inno Setup (installer/httporter.iss) -> dist\HTTPorter-<version>-setup.exe, then sign.ps1 on it
#   7. dist\SHA256SUMS.txt
#   8. print the sizes
#
# The version is httporter-app's, read with cargo metadata. The update check is compiled in
# only when HTTPORTER_RELEASE_REPO (owner/repo) is set for the build; release.yml sets it.
# Signing: see tools/sign.ps1 (skips, and says so, when no certificate is configured).
#
# Options:
#   -TargetDir   cargo target folder; default $env:CARGO_TARGET_DIR, else <repo>\target.
#   -OutDir      where the release files go; default <repo>\dist.
#   -SkipBuild   package the httporter.exe already in <TargetDir>\release.
#   -Exe         package this httporter.exe instead (implies -SkipBuild); for trying the script.
#   -IconFile    setup icon for Inno Setup; default the .iss one (assets\icon\httporter.ico).
#   -Iscc        path of ISCC.exe; default HTTPORTER_ISCC, PATH, then the usual Inno Setup 6 folders.
param(
    [string]$TargetDir,
    [string]$OutDir,
    [switch]$SkipBuild,
    [string]$Exe,
    [string]$IconFile,
    [string]$Iscc
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version 3

$repo = Split-Path -Parent $PSScriptRoot
Set-Location $repo

# rustup puts cargo in %USERPROFILE%\.cargo\bin; a bare shell may not have it on PATH.
$cargoBin = Join-Path $env:USERPROFILE '.cargo\bin'
if ((Test-Path $cargoBin) -and ($env:PATH -notlike "*$cargoBin*")) {
    $env:PATH = "$cargoBin;$env:PATH"
}

# Runs a native program, echoing its output as plain text, and stops on a non-zero exit.
# Windows PowerShell 5.1 would otherwise turn each stderr line (cargo's progress) into an error
# record and, under 'Stop', abort on the first one.
function Invoke-Native([string]$Program, [string[]]$Arguments) {
    Write-Host "> $Program $($Arguments -join ' ')"
    $saved = $ErrorActionPreference
    $ErrorActionPreference = 'Continue'
    try {
        & $Program @Arguments 2>&1 | ForEach-Object {
            if ($_ -is [System.Management.Automation.ErrorRecord]) { "$($_.TargetObject)" } else { "$_" }
        } | Write-Host
        $code = $LASTEXITCODE
    } finally {
        $ErrorActionPreference = $saved
    }
    if ($code -ne 0) { throw "$([IO.Path]::GetFileName($Program)) failed (exit $code)" }
}

# Like Invoke-Native, but returns stdout (stderr is still shown).
function Get-NativeOutput([string]$Program, [string[]]$Arguments) {
    $saved = $ErrorActionPreference
    $ErrorActionPreference = 'Continue'
    try {
        $lines = & $Program @Arguments 2>&1 | ForEach-Object {
            if ($_ -is [System.Management.Automation.ErrorRecord]) { Write-Host "$($_.TargetObject)" } else { "$_" }
        }
        $code = $LASTEXITCODE
    } finally {
        $ErrorActionPreference = $saved
    }
    if ($code -ne 0) { throw "$([IO.Path]::GetFileName($Program)) failed (exit $code)" }
    return ($lines -join "`n")
}

function Find-Python {
    $python = Get-Command python -ErrorAction SilentlyContinue
    if ($python) { return @{ Exe = $python.Source; Args = @() } }
    $launcher = Get-Command py -ErrorAction SilentlyContinue
    if ($launcher) { return @{ Exe = $launcher.Source; Args = @('-3') } }
    throw 'Python 3 not found (needed for tools/third_party.py)'
}

function Find-Iscc {
    $candidates = @()
    if ($Iscc) { $candidates += $Iscc }
    if ($env:HTTPORTER_ISCC) { $candidates += $env:HTTPORTER_ISCC }
    $onPath = Get-Command ISCC.exe -ErrorAction SilentlyContinue
    if ($onPath) { $candidates += $onPath.Source }
    $candidates += Join-Path $env:LOCALAPPDATA 'Programs\Inno Setup 6\ISCC.exe'
    if (${env:ProgramFiles(x86)}) { $candidates += Join-Path ${env:ProgramFiles(x86)} 'Inno Setup 6\ISCC.exe' }
    $candidates += Join-Path $env:ProgramFiles 'Inno Setup 6\ISCC.exe'
    foreach ($path in $candidates) {
        if ($path -and (Test-Path -LiteralPath $path -PathType Leaf)) { return $path }
    }
    throw 'ISCC.exe (Inno Setup 6.7+) not found: install it, or pass -Iscc / set HTTPORTER_ISCC'
}

# A text file for Windows readers: CRLF line endings, UTF-8 without a byte-order mark.
function Write-WindowsText([string]$Path, [string]$Text) {
    $crlf = ($Text -replace "`r`n", "`n") -replace "`n", "`r`n"
    [IO.File]::WriteAllText($Path, $crlf, (New-Object System.Text.UTF8Encoding $false))
}

function Format-Size([long]$Bytes) {
    '{0:N2} MB ({1:N0} bytes)' -f ($Bytes / 1000000), $Bytes
}

# --- Version --------------------------------------------------------------------------------
$metadata = Get-NativeOutput 'cargo' @('metadata', '--format-version', '1', '--no-deps', '--locked') | ConvertFrom-Json
$version = ($metadata.packages | Where-Object { $_.name -eq 'httporter-app' }).version
if (-not $version) { throw 'httporter-app not found in cargo metadata' }
# Windows file versions are numeric: 1.2.0-rc.1 -> 1.2.0.
$numericVersion = ($version -split '[-+]')[0]
Write-Host "HTTPorter $version"

if ($env:HTTPORTER_RELEASE_REPO) {
    Write-Host "update check: on, release feed github.com/$($env:HTTPORTER_RELEASE_REPO)"
} else {
    Write-Host 'update check: off (HTTPORTER_RELEASE_REPO is not set)'
}

# --- 1. Build -------------------------------------------------------------------------------
if (-not $TargetDir) {
    $TargetDir = if ($env:CARGO_TARGET_DIR) { $env:CARGO_TARGET_DIR } else { Join-Path $repo 'target' }
}
if ($Exe) {
    $SkipBuild = $true
    $builtExe = (Resolve-Path -LiteralPath $Exe).Path
} else {
    $builtExe = Join-Path $TargetDir 'release\httporter.exe'
}
if ($SkipBuild) {
    Write-Host "build: skipped, packaging $builtExe"
} else {
    Invoke-Native 'cargo' @('build', '--release', '--locked', '-p', 'httporter-app', '--target-dir', $TargetDir)
}
if (-not (Test-Path -LiteralPath $builtExe -PathType Leaf)) { throw "no exe at $builtExe" }

# --- 2. Size (reported only) ----------------------------------------------------------------
$exeSize = (Get-Item -LiteralPath $builtExe).Length

# --- 3. Stage -------------------------------------------------------------------------------
if (-not $OutDir) { $OutDir = Join-Path $repo 'dist' }
New-Item -ItemType Directory -Force -Path $OutDir | Out-Null
$OutDir = (Resolve-Path -LiteralPath $OutDir).Path
$name = "HTTPorter-$version"
$stage = Join-Path $OutDir $name
if (Test-Path -LiteralPath $stage) { Remove-Item -LiteralPath $stage -Recurse -Force }
New-Item -ItemType Directory -Path $stage | Out-Null

Copy-Item -LiteralPath $builtExe -Destination (Join-Path $stage 'httporter.exe')
Write-WindowsText (Join-Path $stage 'LICENSE-MIT.txt') ([IO.File]::ReadAllText((Join-Path $repo 'LICENSE-MIT')))
Write-WindowsText (Join-Path $stage 'LICENSE-APACHE.txt') ([IO.File]::ReadAllText((Join-Path $repo 'LICENSE-APACHE')))

$python = Find-Python
Invoke-Native $python.Exe ($python.Args + @('-I', (Join-Path $repo 'tools\third_party.py'), '--output', (Join-Path $stage 'THIRD-PARTY-NOTICES.txt')))

$projectPage = ''
if ($env:HTTPORTER_RELEASE_REPO) { $projectPage = "https://github.com/$($env:HTTPORTER_RELEASE_REPO)" }
$readme = @"
HTTPorter $version
$('=' * ("HTTPorter $version".Length))

HTTPorter is a free, native Windows app for cloud storage that speaks HTTP APIs:
Bunny Storage and S3-compatible services (AWS S3, Cloudflare R2, Backblaze B2, Wasabi,
DigitalOcean Spaces, MinIO and others). It publishes a local folder to a bucket or
storage zone (only what changed, then clears the CDN cache), and manages remote files in
two panes.

Running it
----------
- Installer (HTTPorter-$version-setup.exe): installs for your Windows account, without
  admin rights, in %LOCALAPPDATA%\Programs\HTTPorter, with a Start menu shortcut.
- Portable zip: unzip anywhere and run httporter.exe.

Requires Windows 10 or 11, 64-bit.

Your data
---------
Sites, settings, history and logs: %APPDATA%\HTTPorter
Previews, thumbnails and edit copies: %LOCALAPPDATA%\HTTPorter\cache
Keys and passwords: Windows Credential Manager only (targets starting with "HTTPorter/").

Uninstalling keeps all of it. Settings > Security > Reset HTTPorter removes it.
HTTPorter sends no telemetry. It connects only to the storage services and CDNs you add
and, if you turn the update check on, to its release feed.

Licence
-------
HTTPorter is free software under the MIT License or the Apache License 2.0, at your
option: LICENSE-MIT.txt, LICENSE-APACHE.txt. The software it includes and their licences
are listed in THIRD-PARTY-NOTICES.txt.

Made with Slint (https://slint.dev).
"@
if ($projectPage) { $readme += "`n`nProject page: $projectPage`n" } else { $readme += "`n" }
Write-WindowsText (Join-Path $stage 'README.txt') $readme

# --- 4. Sign the exe ------------------------------------------------------------------------
$signScript = Join-Path $repo 'tools\sign.ps1'
$powershell = Join-Path $PSHOME 'powershell.exe'
if (-not (Test-Path -LiteralPath $powershell)) { $powershell = (Get-Process -Id $PID).Path }
Invoke-Native $powershell @('-NoProfile', '-ExecutionPolicy', 'Bypass', '-File', $signScript, (Join-Path $stage 'httporter.exe'))

# --- 5. Portable zip ------------------------------------------------------------------------
# Built entry by entry so every name uses forward slashes (Compress-Archive in Windows
# PowerShell 5.1 writes backslashes, which other unzip tools mishandle).
Add-Type -AssemblyName System.IO.Compression
Add-Type -AssemblyName System.IO.Compression.FileSystem
$zipPath = Join-Path $OutDir "$name-windows-x64-portable.zip"
if (Test-Path -LiteralPath $zipPath) { Remove-Item -LiteralPath $zipPath -Force }
$zip = [System.IO.Compression.ZipFile]::Open($zipPath, [System.IO.Compression.ZipArchiveMode]::Create)
try {
    foreach ($file in Get-ChildItem -LiteralPath $stage -File | Sort-Object Name) {
        [System.IO.Compression.ZipFileExtensions]::CreateEntryFromFile(
            $zip, $file.FullName, "$name/$($file.Name)", [System.IO.Compression.CompressionLevel]::Optimal) | Out-Null
    }
} finally {
    $zip.Dispose()
}

# --- 6. Installer ---------------------------------------------------------------------------
$isccPath = Find-Iscc
$signing = [bool]($env:HTTPORTER_SIGN_THUMBPRINT -or ($env:HTTPORTER_SIGN_DLIB -and $env:HTTPORTER_SIGN_METADATA))
$isccArgs = @('/Q', "/DAppVersion=$version", "/DVersionInfo=$numericVersion", "/DSourceDir=$stage", "/O$OutDir")
if ($IconFile) { $isccArgs += "/DIconFile=$((Resolve-Path -LiteralPath $IconFile).Path)" }
if ($projectPage) { $isccArgs += "/DRepoUrl=$projectPage" }
if ($signing) {
    # Inno Setup signs the uninstaller it embeds, and the setup, through sign.ps1.
    $isccArgs += '/DSign'
    # $q is Inno Setup's quote and $f the file to sign; no literal quotes, which Windows
    # PowerShell 5.1 would pass to ISCC unescaped.
    $isccArgs += '/Shttporter=$q' + $powershell + '$q -NoProfile -ExecutionPolicy Bypass -File $q' + $signScript + '$q $f'
}
$isccArgs += (Join-Path $repo 'installer\httporter.iss')
Invoke-Native $isccPath $isccArgs
$setupPath = Join-Path $OutDir "$name-setup.exe"
if (-not (Test-Path -LiteralPath $setupPath)) { throw "Inno Setup wrote no $setupPath" }
if (-not $signing) {
    # Prints "unsigned: ..." and changes nothing; with a certificate, Inno Setup signed it above.
    Invoke-Native $powershell @('-NoProfile', '-ExecutionPolicy', 'Bypass', '-File', $signScript, $setupPath)
}

# --- 7. Checksums ---------------------------------------------------------------------------
$sums = foreach ($path in @($zipPath, $setupPath)) {
    '{0}  {1}' -f (Get-FileHash -LiteralPath $path -Algorithm SHA256).Hash.ToLowerInvariant(), [IO.Path]::GetFileName($path)
}
$sumsPath = Join-Path $OutDir 'SHA256SUMS.txt'
[IO.File]::WriteAllText($sumsPath, (($sums -join "`n") + "`n"), (New-Object System.Text.UTF8Encoding $false))

# --- 8. Sizes -------------------------------------------------------------------------------
Write-Host ''
Write-Host "HTTPorter $version packaged in $OutDir"
Write-Host ("  httporter.exe   {0}" -f (Format-Size $exeSize))
Write-Host ("  portable zip    {0}" -f (Format-Size (Get-Item -LiteralPath $zipPath).Length))
Write-Host ("  setup           {0}" -f (Format-Size (Get-Item -LiteralPath $setupPath).Length))
Write-Host "  SHA256SUMS.txt"
foreach ($line in $sums) { Write-Host "    $line" }
exit 0
