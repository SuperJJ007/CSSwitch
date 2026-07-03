<#
.SYNOPSIS
    查看 WSL2 中 CSSwitch 代理容器的实时日志。
.PARAMETER WslDistro
    WSL2 发行版名称（默认：自动选择运行中的发行版）。
.PARAMETER ProjectPath
    项目在 WSL2 中的绝对路径（默认：自动映射当前目录）。
.PARAMETER Lines
    显示最近 N 行日志（默认：显示所有历史并 follow）。
#>

param(
    [string]$WslDistro = "",
    [string]$ProjectPath = "",
    [int]$Lines = 0
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

$tailFlag = if ($Lines -gt 0) { "--tail=$Lines" } else { "" }

Write-Host "查看 WSL2（$WslDistro）CSSwitch 代理日志（按 Ctrl+C 退出）…" -ForegroundColor Cyan
wsl -d $WslDistro --cd $ProjectPath bash -c "docker compose -f docker/docker-compose.yml logs -f $tailFlag csswitch-proxy"
