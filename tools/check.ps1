# Local quality gate. Run before every commit:
#   powershell -NoProfile -File tools/check.ps1
# Runs the same steps as .github/workflows/ci.yml and stops at the first failure.
$ErrorActionPreference = 'Stop'

# Run from the repository root no matter where the script was started.
Set-Location (Split-Path -Parent $PSScriptRoot)

# rustup puts cargo in %USERPROFILE%\.cargo\bin; a bare shell may not have it on PATH.
$cargoBin = Join-Path $env:USERPROFILE '.cargo\bin'
if ((Test-Path $cargoBin) -and ($env:PATH -notlike "*$cargoBin*")) {
    $env:PATH = "$cargoBin;$env:PATH"
}

$steps = @(
    @('fmt', '--check'),
    @('clippy', '--workspace', '--all-targets', '--', '-D', 'warnings'),
    @('test', '--workspace'),
    @('deny', 'check', 'licenses', 'bans')
)

foreach ($step in $steps) {
    Write-Host ''
    Write-Host ('==> cargo ' + ($step -join ' '))
    & cargo @step
    if ($LASTEXITCODE -ne 0) {
        Write-Host ''
        Write-Host ('FAILED: cargo ' + ($step -join ' ') + " (exit $LASTEXITCODE)")
        exit $LASTEXITCODE
    }
}

Write-Host ''
Write-Host 'check: all steps passed'
