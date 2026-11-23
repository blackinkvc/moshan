# Windows build: source -> dist\Moshan -> Inno Setup installer
#
# Usage (from repo root):
#   powershell -ExecutionPolicy Bypass -File packages\windows\build.ps1
# Requires: Python 3.10+, Inno Setup 6 (iscc on PATH, or set ISCC env var)
#
# TWO HARD RULES (both were learned the painful way, not defensive coding):
#
#   1) KEEP THIS FILE PURE ASCII. PowerShell 5.1 reads a BOM-less script as GBK;
#      a trailing CJK char can swallow the newline and pull the NEXT LINE into
#      the comment. Symptom: statements like Set-Location silently do nothing.
#      scripts/check.sh enforces this for every .ps1/.cmd in the repo.
#
#   2) ALWAYS Set-Location to the repo root. Processes started by proc_start or
#      a scheduled task have CWD = C:\Windows\System32, where PyInstaller
#      refuses to run ("Do not run pyinstaller from C:\Windows\System32").
#      These builds usually run in a hidden remote session, so a missing log
#      line is the only symptom you get.
$ErrorActionPreference = "Stop"

$Root = (Resolve-Path (Join-Path $PSScriptRoot "..\..")).Path
$Src = Join-Path $Root "moshan"
$Dist = Join-Path $Root "dist"
$BuildVenv = Join-Path $Root ".venv-build"

Set-Location $Root
Write-Host "==> cwd $Root" -ForegroundColor DarkGray

# ---- version ----
$versionLine = Select-String -Path (Join-Path $Src "version.py") -Pattern '^VERSION\s*=\s*"([^"]+)"' |
    Select-Object -First 1
if (-not $versionLine) { throw "cannot read VERSION from version.py" }
$Version = $versionLine.Matches[0].Groups[1].Value
Write-Host "==> version $Version" -ForegroundColor Cyan

# ---- build environment ----
if (-not (Test-Path (Join-Path $BuildVenv "Scripts\python.exe"))) {
    Write-Host "==> creating build venv"
    python -m venv $BuildVenv
}
$Py = Join-Path $BuildVenv "Scripts\python.exe"
& $Py -m pip install -q --upgrade pip
Write-Host "==> installing requirements"
& $Py -m pip install -q -r (Join-Path $Root "requirements.txt")
& $Py -m pip install -q "pyinstaller>=6.6"

# ---- package ----
Write-Host "==> pyinstaller"
if (Test-Path (Join-Path $Dist "Moshan")) { Remove-Item -Recurse -Force (Join-Path $Dist "Moshan") }
& (Join-Path $BuildVenv "Scripts\pyinstaller.exe") --noconfirm --clean `
    --distpath $Dist --workpath (Join-Path $Root "build") (Join-Path $Root "packages\moshan.spec")
if (-not (Test-Path (Join-Path $Dist "Moshan\Moshan.exe"))) { throw "build failed: no Moshan.exe" }

# ---- installer ----
$Iscc = $env:ISCC
if (-not $Iscc) {
    $candidates = @(
        "${env:ProgramFiles(x86)}\Inno Setup 6\ISCC.exe",
        "$env:ProgramFiles\Inno Setup 6\ISCC.exe"
    )
    foreach ($c in $candidates) { if (Test-Path $c) { $Iscc = $c; break } }
}
if (-not $Iscc) {
    # No silent download: Inno Setup only ships via GitHub Releases now, and the
    # machines we build on often cannot reach github.com. Host pushes the
    # installer in instead (scripts/host_push_update.py has the chunked transport).
    Write-Warning "Inno Setup (iscc) not found. dist\Moshan is ready; install Inno Setup 6 and re-run."
} else {
    Write-Host "==> compiling installer"
    & $Iscc "/DAppVersion=$Version" "/DSourceDir=$Dist\Moshan" (Join-Path $Root "packages\windows\moshan.iss")
    $setup = Join-Path $Dist "Moshan-$Version-windows-x64-Setup.exe"
    if (Test-Path $setup) {
        $fi = Get-Item $setup
        $hash = (Get-FileHash $setup -Algorithm SHA256).Hash.ToLower()
        Write-Host "==> installer: $setup" -ForegroundColor Green
        Write-Host "    size: $($fi.Length) bytes"
        Write-Host "    sha256: $hash"
    }
}
Write-Host "==> done" -ForegroundColor Green
