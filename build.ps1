param(
    [switch]$Debug,
    [switch]$Deploy
)

$ErrorActionPreference = "Stop"

if ($Debug) {
    Write-Host "==> Building Debug binary..." -ForegroundColor Cyan
    cargo build
    $targetExe = "target\debug\cicada.exe"
} else {
    Write-Host "==> Building Release binary..." -ForegroundColor Cyan
    cargo build --release
    $targetExe = "target\release\cicada.exe"
}

Write-Host "==> Embedding app.ico into $targetExe via Win32 UpdateResource..." -ForegroundColor Cyan
& powershell -NoProfile -ExecutionPolicy Bypass -File "scripts\inject_icon.ps1" -ExePath $targetExe -IcoPath "app.ico"

$size = (Get-Item $targetExe).Length / 1KB
Write-Host "==> Done! Binary: $targetExe ($([math]::Round($size, 1)) KB)" -ForegroundColor Green

# 复制到项目根目录以及 dist 目录，方便 Cloudflare Pages 部署
Copy-Item -Force $targetExe "cicada.exe"
New-Item -ItemType Directory -Force -Path "dist" | Out-Null
Copy-Item -Force $targetExe "dist\cicada.exe"
Copy-Item -Force "index.html" "dist\index.html"
if (Test-Path "app.ico") {
    Copy-Item -Force "app.ico" "dist\favicon.ico"
    Copy-Item -Force "app.ico" "dist\app.ico"
}
if (Test-Path "icon.png") {
    Copy-Item -Force "icon.png" "dist\icon.png"
}
if (Test-Path "logo.png") {
    Copy-Item -Force "logo.png" "dist\logo.png"
}

Write-Host "==> Ready for Cloudflare Pages! Both root and dist/ contain index.html and cicada.exe." -ForegroundColor Green

if ($Deploy) {
    Write-Host "==> Deploying to Cloudflare Pages (cicada)..." -ForegroundColor Cyan
    npx wrangler pages deploy dist --project-name cicada --commit-dirty=true
}

