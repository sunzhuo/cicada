param(
    [switch]$Debug
)

$ErrorActionPreference = "Stop"

if ($Debug) {
    Write-Host "==> Building Debug binary..." -ForegroundColor Cyan
    cargo build
    $targetExe = "target\debug\zero-daemon.exe"
} else {
    Write-Host "==> Building Release binary..." -ForegroundColor Cyan
    cargo build --release
    $targetExe = "target\release\zero-daemon.exe"
}

Write-Host "==> Embedding app.ico into $targetExe via Win32 UpdateResource..." -ForegroundColor Cyan
& powershell -NoProfile -ExecutionPolicy Bypass -File "scripts\inject_icon.ps1" -ExePath $targetExe -IcoPath "app.ico"

$size = (Get-Item $targetExe).Length / 1KB
Write-Host "==> Done! Binary: $targetExe ($([math]::Round($size, 1)) KB)" -ForegroundColor Green

