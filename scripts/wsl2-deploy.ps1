<#
.SYNOPSIS
    在 WSL2 中部署 CSSwitch 代理容器。
.DESCRIPTION
    检测 WSL2 环境，在 WSL2 中调用 docker compose up -d，
    自动探测 WSL2 IP 并输出供 Windows 客户端使用的 ANTHROPIC_BASE_URL。
.PARAMETER WslDistro
    WSL2 发行版名称（默认：从 wsl -l -v 自动选择运行中的发行版）。
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
    Write-Host "✗ 未找到 wsl.exe。请先安装 WSL2。" -ForegroundColor Red
    exit 1
}

# 列出运行中的 WSL2 发行版
$distros = wsl -l -v --quiet 2>$null | Where-Object { $_ -match 'Running' }
if (-not $distros) {
    Write-Host "✗ 未找到运行中的 WSL2 发行版。" -ForegroundColor Red
    Write-Host "  请先启动一个 WSL2 发行版（例如：wsl -d Ubuntu）" -ForegroundColor Yellow
    exit 1
}

if (-not $WslDistro) {
    # 取第一个运行中的发行版
    $WslDistro = ($distros[0] -split '\s+')[0]
    Write-Host "  自动选择 WSL2 发行版：$WslDistro" -ForegroundColor Cyan
}

# ── 映射项目路径 ──
if (-not $ProjectPath) {
    $hostPath = Get-Location
    # 尝试将 Windows 路径（如 G:\WorkSpace\CSswitch）映射到 WSL2 挂载点
    if ($hostPath.Path -match '^([A-Z]):\\(.*)') {
        $driveLetter = $matches[1].ToLower()
        $restPath = $matches[2] -replace '\\', '/'
        $ProjectPath = "/mnt/$driveLetter/$restPath"
    } else {
        Write-Host "✗ 无法自动映射路径：$hostPath" -ForegroundColor Red
        Write-Host "  请用 -ProjectPath 参数指定 WSL2 内的项目路径" -ForegroundColor Yellow
        exit 1
    }
}

# ── 检查 .env ──
$checkResult = wsl -d $WslDistro --cd $ProjectPath bash -c 'test -f docker/.env && echo "EXISTS" || echo "MISSING"'
if ($checkResult -eq "MISSING") {
    Write-Host "✗ 未找到 docker/.env" -ForegroundColor Red
    Write-Host "  在 WSL2 中执行：cp docker/.env.example docker/.env" -ForegroundColor Yellow
    Write-Host "  然后编辑 docker/.env，填入你的 API key。" -ForegroundColor Yellow
    Write-Host ""
    Write-Host "  快速编辑（在 WSL2 中）：wsl -d $WslDistro --cd $ProjectPath vi docker/.env" -ForegroundColor Cyan
    exit 1
}

# ── 部署 ──
Write-Host "部署 CSSwitch 代理容器到 WSL2（$WslDistro）…" -ForegroundColor Green
Write-Host "  项目路径：$ProjectPath" -ForegroundColor Cyan

$deployResult = wsl -d $WslDistro --cd $ProjectPath bash -c 'docker compose -f docker/docker-compose.yml up -d 2>&1'
if ($LASTEXITCODE -ne 0) {
    Write-Host "✗ 部署失败：" -ForegroundColor Red
    Write-Host $deployResult -ForegroundColor Red
    exit 1
}
Write-Host $deployResult
Write-Host "✓ 容器已启动" -ForegroundColor Green

# ── 获取 WSL2 IP ──
$wsl2Ip = wsl -d $WslDistro --cd $ProjectPath bash -c "hostname -I 2>/dev/null | awk '{print \$1}'" 2>$null
if (-not $wsl2Ip) {
    $wsl2Ip = wsl -d $WslDistro --cd $ProjectPath bash -c "ip route get 1 2>/dev/null | head -1 | awk '{print \$7}'" 2>$null
}

# ── 读取端口与 secret ──
$proxyPort = wsl -d $WslDistro --cd $ProjectPath bash -c 'grep "^CSSWITCH_PROXY_PORT=" docker/.env 2>/dev/null | cut -d= -f2 || echo "18991"'
$authToken = wsl -d $WslDistro --cd $ProjectPath bash -c 'grep "^CSSWITCH_AUTH_TOKEN=" docker/.env 2>/dev/null | cut -d= -f2 || echo ""'

$baseUrl = "http://${wsl2Ip}:${proxyPort}/"
if ($authToken) {
    $baseUrl += "${authToken}/"
}

Write-Host ""
Write-Host "=== 部署完成 ===" -ForegroundColor Green
Write-Host "  WSL2 IP：$wsl2Ip" -ForegroundColor Cyan
Write-Host "  代理端口：$proxyPort" -ForegroundColor Cyan
Write-Host ""
Write-Host "  Windows 客户端配置 ANTHROPIC_BASE_URL：" -ForegroundColor White
Write-Host "  $baseUrl" -ForegroundColor Yellow
Write-Host ""
Write-Host "  健康检查命令（在 Windows PowerShell 中）：" -ForegroundColor White
Write-Host "  curl.exe -s -m 5 $($baseUrl)health" -ForegroundColor Gray
Write-Host ""
Write-Host "  查看日志：.\scripts\wsl2-logs.ps1" -ForegroundColor Gray
Write-Host "  停止服务：.\scripts\wsl2-stop.ps1" -ForegroundColor Gray
