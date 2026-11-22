# Windows 构建脚本：源码 -> dist\Moshan -> Inno Setup 安装程序
#
# 用法（在仓库根目录）：
#   powershell -ExecutionPolicy Bypass -File packages\windows\build.ps1
# 前置：Python 3.10+、Inno Setup 6（iscc 在 PATH，或设 ISCC 环境变量）
$ErrorActionPreference = "Stop"

$Root = (Resolve-Path (Join-Path $PSScriptRoot "..\..")).Path
$Src = Join-Path $Root "moshan"
$Dist = Join-Path $Root "dist"
$BuildVenv = Join-Path $Root ".venv-build"

# ---- 版本号 ----
$versionLine = Select-String -Path (Join-Path $Src "version.py") -Pattern '^VERSION\s*=\s*"([^"]+)"' |
    Select-Object -First 1
if (-not $versionLine) { throw "读不到版本号" }
$Version = $versionLine.Matches[0].Groups[1].Value
Write-Host "==> 版本 $Version" -ForegroundColor Cyan

# ---- 构建环境 ----
if (-not (Test-Path (Join-Path $BuildVenv "Scripts\python.exe"))) {
    Write-Host "==> 创建独立构建环境"
    python -m venv $BuildVenv
}
$Py = Join-Path $BuildVenv "Scripts\python.exe"
& $Py -m pip install -q --upgrade pip
Write-Host "==> 安装依赖"
& $Py -m pip install -q -r (Join-Path $Root "requirements.txt")
& $Py -m pip install -q "pyinstaller>=6.6"

# ---- 打包 ----
Write-Host "==> PyInstaller 打包"
if (Test-Path (Join-Path $Dist "Moshan")) { Remove-Item -Recurse -Force (Join-Path $Dist "Moshan") }
& (Join-Path $BuildVenv "Scripts\pyinstaller.exe") --noconfirm --clean `
    --distpath $Dist --workpath (Join-Path $Root "build") (Join-Path $Root "packages\moshan.spec")
if (-not (Test-Path (Join-Path $Dist "Moshan\Moshan.exe"))) { throw "打包失败：没有生成 Moshan.exe" }

# ---- 安装程序 ----
$Iscc = $env:ISCC
if (-not $Iscc) {
    $candidates = @(
        "${env:ProgramFiles(x86)}\Inno Setup 6\ISCC.exe",
        "$env:ProgramFiles\Inno Setup 6\ISCC.exe"
    )
    foreach ($c in $candidates) { if (Test-Path $c) { $Iscc = $c; break } }
}
if (-not $Iscc) {
    Write-Warning "未找到 Inno Setup（iscc）。已产出 dist\Moshan 目录，可手动打包。"
} else {
    Write-Host "==> 生成安装程序"
    & $Iscc "/DAppVersion=$Version" "/DSourceDir=$Dist\Moshan" (Join-Path $Root "packages\windows\moshan.iss")
    $setup = Join-Path $Dist "Moshan-$Version-windows-x64-Setup.exe"
    if (Test-Path $setup) {
        $hash = (Get-FileHash $setup -Algorithm SHA256).Hash.ToLower()
        Write-Host "==> 安装程序: $setup" -ForegroundColor Green
        Write-Host "    sha256: $hash"
    }
}
Write-Host "==> 完成" -ForegroundColor Green
