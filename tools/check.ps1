# Local quality gate. Run before every commit:
#   powershell -NoProfile -File tools/check.ps1
# Runs the same steps as .github/workflows/ci.yml (keep the two in sync) and stops at the
# first failure, with that step's exit code. Exits 0 only when every step passed.
#
# Works the same in Windows PowerShell 5.1 and PowerShell 7, with or without its output
# redirected: native programs' stderr (cargo prints its progress there) is shown as plain
# text, and only exit codes decide success.
param()

# 'Stop' for PowerShell's own commands. Native programs run through Invoke-Step, which
# relaxes it around the call: under 'Stop', Windows PowerShell 5.1 turns the first stderr
# line of a redirected native program into a terminating NativeCommandError.
$ErrorActionPreference = 'Stop'

# Run from the repository root no matter where the script was started.
Set-Location (Split-Path -Parent $PSScriptRoot)

# rustup puts cargo in %USERPROFILE%\.cargo\bin; a bare shell may not have it on PATH.
$cargoBin = Join-Path $env:USERPROFILE '.cargo\bin'
if ((Test-Path $cargoBin) -and ($env:PATH -notlike "*$cargoBin*")) {
    $env:PATH = "$cargoBin;$env:PATH"
}

$python = Get-Command python -ErrorAction SilentlyContinue
$pythonExe = if ($python) { $python.Source } else { 'py' }
[string[]]$pythonArgs = if ($python) { @('-I') } else { @('-3', '-I') }

function Invoke-Step([string]$Label, [string]$Program, [string[]]$Arguments) {
    Write-Host ''
    Write-Host "==> $Label"
    $saved = $ErrorActionPreference
    $ErrorActionPreference = 'Continue'
    try {
        & $Program @Arguments 2>&1 | ForEach-Object {
            if ($_ -is [System.Management.Automation.ErrorRecord]) { "$($_.TargetObject)" } else { "$_" }
        } | Write-Host
        $code = $LASTEXITCODE
    } catch {
        # The program could not start at all (not installed, not on PATH).
        Write-Host $_.Exception.Message
        $code = 127
    } finally {
        $ErrorActionPreference = $saved
    }
    if ($code -ne 0) {
        Write-Host ''
        Write-Host "FAILED: $Label (exit $code)"
        exit $code
    }
}

Invoke-Step 'cargo fmt --check' 'cargo' @('fmt', '--check')
Invoke-Step 'cargo clippy' 'cargo' @('clippy', '--locked', '--workspace', '--all-targets', '--', '-D', 'warnings')
Invoke-Step 'cargo test' 'cargo' @('test', '--locked', '--workspace')
# Duplicate versions are warnings; drop --hide-inclusion-graph to see who pulls each crate in.
Invoke-Step 'cargo deny (licences, bans)' 'cargo' @('deny', '--locked', 'check', '--hide-inclusion-graph', 'licenses', 'bans')
Invoke-Step 'SPDX headers' $pythonExe ($pythonArgs + @('tools/spdx_headers.py', '--check'))
Invoke-Step 'third-party notices' $pythonExe ($pythonArgs + @('tools/third_party.py', '--check'))

Write-Host ''
Write-Host 'check: all steps passed'
exit 0
