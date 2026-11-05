# Authenticode-sign a release file, or skip when no certificate is configured.
#   powershell -NoProfile -File tools/sign.ps1 <file>
#
# The certificate comes from the environment, never from the repository:
#   HTTPORTER_SIGN_THUMBPRINT   SHA-1 thumbprint of a code-signing certificate in the user's or
#                               the machine's certificate store (signtool /sha1).
#   HTTPORTER_SIGN_DLIB         Azure Trusted Signing: the path of Azure.CodeSigning.Dlib.dll,
#   HTTPORTER_SIGN_METADATA     and of the metadata.json naming the account and profile
#                               (signtool /dlib /dmdf). Both or neither.
#   HTTPORTER_SIGN_TIMESTAMP    optional RFC 3161 timestamp server; default DigiCert's.
#   HTTPORTER_SIGNTOOL          optional path of signtool.exe; default the newest Windows SDK one.
# With none of the certificate variables set it prints "unsigned: no signing certificate
# configured" and exits 0, so unsigned builds (HTTPorter 1.0) go through package.ps1 unchanged.
# Inno Setup calls this script too, for the uninstaller (installer/httporter.iss, /DSign).
param(
    [Parameter(Mandatory = $true, Position = 0)]
    [string]$File
)

$ErrorActionPreference = 'Stop'

if (-not (Test-Path -LiteralPath $File -PathType Leaf)) {
    Write-Host "sign: no such file: $File"
    exit 1
}
$File = (Resolve-Path -LiteralPath $File).Path

$thumbprint = $env:HTTPORTER_SIGN_THUMBPRINT
$dlib = $env:HTTPORTER_SIGN_DLIB
$metadata = $env:HTTPORTER_SIGN_METADATA

if (-not $thumbprint -and -not $dlib -and -not $metadata) {
    Write-Host "unsigned: no signing certificate configured ($([IO.Path]::GetFileName($File)))"
    exit 0
}
if ($thumbprint -and ($dlib -or $metadata)) {
    Write-Host 'sign: set HTTPORTER_SIGN_THUMBPRINT or HTTPORTER_SIGN_DLIB + HTTPORTER_SIGN_METADATA, not both'
    exit 1
}
if (-not $thumbprint -and -not ($dlib -and $metadata)) {
    Write-Host 'sign: Azure Trusted Signing needs both HTTPORTER_SIGN_DLIB and HTTPORTER_SIGN_METADATA'
    exit 1
}

function Find-SignTool {
    if ($env:HTTPORTER_SIGNTOOL) {
        if (Test-Path -LiteralPath $env:HTTPORTER_SIGNTOOL -PathType Leaf) { return $env:HTTPORTER_SIGNTOOL }
        throw "HTTPORTER_SIGNTOOL points at a missing file: $($env:HTTPORTER_SIGNTOOL)"
    }
    # The Windows SDK puts one signtool per SDK version under bin\<version>\x64; take the newest.
    $kits = Join-Path ${env:ProgramFiles(x86)} 'Windows Kits\10\bin'
    if (Test-Path -LiteralPath $kits) {
        $newest = Get-ChildItem -LiteralPath $kits -Directory |
            Where-Object { $_.Name -match '^\d+(\.\d+){3}$' } |
            Sort-Object { [version]$_.Name } -Descending |
            ForEach-Object { Join-Path $_.FullName 'x64\signtool.exe' } |
            Where-Object { Test-Path -LiteralPath $_ } |
            Select-Object -First 1
        if ($newest) { return $newest }
    }
    $onPath = Get-Command signtool.exe -ErrorAction SilentlyContinue
    if ($onPath) { return $onPath.Source }
    throw 'signtool.exe not found: install the Windows SDK, or set HTTPORTER_SIGNTOOL'
}

# Runs a native program with its stderr shown as plain text: Windows PowerShell 5.1 would
# otherwise turn each stderr line into an error record and, under 'Stop', abort the script.
function Invoke-Native([string]$Program, [string[]]$Arguments) {
    $saved = $ErrorActionPreference
    $ErrorActionPreference = 'Continue'
    try {
        & $Program @Arguments 2>&1 | ForEach-Object {
            if ($_ -is [System.Management.Automation.ErrorRecord]) { "$($_.TargetObject)" } else { "$_" }
        } | Write-Host
        return $LASTEXITCODE
    } finally {
        $ErrorActionPreference = $saved
    }
}

$signtool = Find-SignTool
$timestamp = if ($env:HTTPORTER_SIGN_TIMESTAMP) { $env:HTTPORTER_SIGN_TIMESTAMP } else { 'http://timestamp.digicert.com' }

$arguments = @('sign', '/fd', 'SHA256', '/tr', $timestamp, '/td', 'SHA256')
if ($thumbprint) {
    $arguments += @('/sha1', ($thumbprint -replace '\s', ''))
    $how = 'certificate store'
} else {
    foreach ($path in @($dlib, $metadata)) {
        if (-not (Test-Path -LiteralPath $path -PathType Leaf)) {
            Write-Host "sign: Azure Trusted Signing file not found: $path"
            exit 1
        }
    }
    $arguments += @('/dlib', $dlib, '/dmdf', $metadata)
    $how = 'Azure Trusted Signing'
}
$arguments += $File

Write-Host "sign: $([IO.Path]::GetFileName($File)) ($how)"
$code = Invoke-Native $signtool $arguments
if ($code -ne 0) {
    Write-Host "sign: signtool sign failed (exit $code)"
    exit 1
}
$code = Invoke-Native $signtool @('verify', '/pa', '/q', $File)
if ($code -ne 0) {
    Write-Host "sign: the signature on $File does not verify (exit $code)"
    exit 1
}
Write-Host "sign: signed and verified $([IO.Path]::GetFileName($File))"
exit 0
