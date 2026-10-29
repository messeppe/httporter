param([string]$Exe, [string]$Label, [string]$Backend = "", [string]$OutDir)

Add-Type -AssemblyName System.Drawing
Add-Type @"
using System;
using System.Runtime.InteropServices;
public static class W {
  [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr h, out RECT r);
  [DllImport("user32.dll")] public static extern bool PrintWindow(IntPtr h, IntPtr hdc, uint flags);
  [DllImport("user32.dll")] public static extern bool SetForegroundWindow(IntPtr h);
  [StructLayout(LayoutKind.Sequential)] public struct RECT { public int Left, Top, Right, Bottom; }
}
"@

if ($Backend) { $env:SLINT_BACKEND = $Backend } else { Remove-Item Env:SLINT_BACKEND -ErrorAction SilentlyContinue }
$p = Start-Process -FilePath $Exe -PassThru
$deadline = (Get-Date).AddSeconds(20)
do { Start-Sleep -Milliseconds 250; $p.Refresh() } while ($p.MainWindowHandle -eq 0 -and (Get-Date) -lt $deadline)
if ($p.MainWindowHandle -eq 0) { "$Label : window never appeared"; $p | Stop-Process -Force; exit 1 }
[W]::SetForegroundWindow($p.MainWindowHandle) | Out-Null
Start-Sleep -Seconds 5   # let it load and settle the 13,793-row table
$p.Refresh()
$priv = [math]::Round($p.PrivateMemorySize64 / 1MB, 1)
$ws = [math]::Round($p.WorkingSet64 / 1MB, 1)

$r = New-Object W+RECT
[W]::GetWindowRect($p.MainWindowHandle, [ref]$r) | Out-Null
$w = $r.Right - $r.Left; $h = $r.Bottom - $r.Top
$bmp = New-Object System.Drawing.Bitmap $w, $h
$g = [System.Drawing.Graphics]::FromImage($bmp)
$hdc = $g.GetHdc()
[W]::PrintWindow($p.MainWindowHandle, $hdc, 2) | Out-Null
$g.ReleaseHdc($hdc); $g.Dispose()
$png = Join-Path $OutDir "$Label.png"
$bmp.Save($png, [System.Drawing.Imaging.ImageFormat]::Png); $bmp.Dispose()

"{0,-28} private {1,6} MB   working set {2,6} MB   window {3}x{4}   -> {5}" -f $Label, $priv, $ws, $w, $h, $png
$p | Stop-Process -Force
