<#
.SYNOPSIS
    停止 WSL2 中的 CSSwitch 代理容器。
.PARAMETER WslDistro
    WSL2 发行版名称（默认：自动选择运行中的发行版）。
.PARAMETER ProjectPath
    项目在 WSL2 中的绝对路径（默认：自动映射当前目录）。
#>

param(
    [string]$WslDistro = "",
    [string]$ProjectPath = ""
)

$ErrorActionPreference = "Stop"

# ── 检测 WSL ──
$wslAvailable = Get-Command "wsl.exe" -ErrorAction SilentlyContinue
if (-not $wslAvailable) {
    Write-Host "✗ 未找到 wsl.exe。" -ForegroundColor Red
    exit 1
}

$distros = wsl -l -v --quiet 2>$null | Where-Object { $_ -match 'Running' }
if (-not $distros) {
    Write-Host "✗ 未找到运行中的 WSL2 发行版。" -ForegroundColor Red
    exit 1
}

if (-not $WslDistro) {
    $WslDistro = ($distros[0] -split '\s+')[0]
}

# ── 映射路径 ──
if (-not $ProjectPath) {
    $hostPath = Get-Location
    if ($hostPath.Path -match '^([A-Z]):\\(.*)') {
        $driveLetter = $matches[1].ToLower()
        $restPath = $matches[2] -replace '\\', '/'
        $ProjectPath = "/mnt/$driveLetter/$restPath"
    } else {
        Write-Host "✗ 无法自动映射路径：$hostPath" -ForegroundColor Red
        exit 1
    }
}

Write-Host "停止 WSL2（$WslDistro）中的 CSSwitch 代理容器…" -ForegroundColor Yellow
$result = wsl -d $WslDistro --cd $ProjectPath bash -c 'docker compose -f docker/docker-compose.yml down 2>&1'
if ($LASTEXITCODE -ne 0) {
    Write-Host "✗ 停止失败：" -ForegroundColor Red
    Write-Host $result -ForegroundColor Red
    exit 1
}
Write-Host $result
Write-Host "✓ 容器已停止" -ForegroundColor Green
