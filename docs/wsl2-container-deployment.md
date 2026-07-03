# CSSwitch WSL2 容器化部署指南

> **实验性支持**：本方案仅容器化 CSSwitch 翻译代理（`csswitch_proxy.py`），
> 在 WSL2 中以 Docker 容器形式运行代理服务。
> Claude Science 本身仍是 macOS 应用，不在此部署范围内。
> 容器化代理适合 Windows 上的 **OpenAI/Anthropic 兼容客户端** 使用，
> 或作为局域网内的代理服务。

## 目录

- [前置条件](#前置条件)
- [快速开始（PowerShell）](#快速开始powershell)
- [快速开始（WSL2 内）](#快速开始wsl2-内)
- [配置说明](#配置说明)
- [网络说明](#网络说明)
- [可选：Caddy TLS 反代](#可选caddy-tls-反代)
- [手动构建镜像](#手动构建镜像)
- [故障排查](#故障排查)
- [安全提示](#安全提示)
- [限制](#限制)

## 前置条件

- **Windows 10/11** 已安装 **WSL2** 和 **Docker**（以下任选其一）：
  - [Docker Desktop for Windows](https://docs.docker.com/desktop/wsl/)（推荐，集成 WSL2 后端）
  - 或 WSL2 发行版内安装 [Docker Engine](https://docs.docker.com/engine/install/ubuntu/)
- WSL2 发行版（推荐 Ubuntu 22.04+）
- 一个有效的 **第三方 API key**（[DeepSeek](https://platform.deepseek.com/api_keys) 或 [阿里通义千问（DashScope）](https://bailian.console.aliyun.com/)）
- PowerShell 5.1+（Windows 自带）或 PowerShell 7+

## 快速开始（PowerShell）

从 Windows 一侧一键部署，适合不常进 WSL2 终端的用户。

### 步骤 1：克隆项目到 Windows

```powershell
git clone https://github.com/SuperJJ007/CSswitch.git
cd CSswitch
```

### 步骤 2：配置环境变量

从模板复制 `.env` 文件：

```powershell
# 在 PowerShell 中：
wsl -d <你的发行版> --cd /mnt/g/WorkSpace/CSswitch cp docker/.env.example docker/.env
```

或用 WSL2 终端编辑：

```bash
cd /mnt/g/WorkSpace/CSswitch   # 替换为你实际的挂载路径
cp docker/.env.example docker/.env
vi docker/.env
```

填入你的 key：

```ini
# 选 deepseek 时填写
DEEPSEEK_API_KEY=sk-your-deepseek-key-here

# 强烈建议设置随机 path secret
CSSWITCH_AUTH_TOKEN=your-random-secret-here
```

### 步骤 3：一键部署

```powershell
.\scripts\wsl2-deploy.ps1
```

脚本会自动：
1. 检测 WSL2 运行中的发行版
2. 在 WSL2 内执行 `docker compose up -d`
3. 探测 WSL2 IP 地址
4. 输出供 Windows 客户端使用的 `ANTHROPIC_BASE_URL`

### 步骤 4：验证代理

```powershell
# 用 .\scripts\wsl2-deploy.ps1 输出的地址测试健康检查
curl.exe -s -m 5 http://<wsl2-ip>:18991/<secret>/health
# 预期：{"status":"ok","provider":"deepseek",...}
```

### 步骤 5：停止服务

```powershell
.\scripts\wsl2-stop.ps1
```

## 快速开始（WSL2 内）

### 步骤 1：进入项目目录

```bash
cd /path/to/CSswitch
```

### 步骤 2：配置环境变量

```bash
cp docker/.env.example docker/.env
vi docker/.env
```

### 步骤 3：构建并启动

```bash
# 构建镜像
bash scripts/docker-build.sh

# 启动容器
bash scripts/wsl2-deploy.sh
```

### 步骤 4：验证

```bash
# 健康检查
curl http://localhost:18991/<secret>/health

# 查看日志
bash scripts/wsl2-logs.sh
```

### 步骤 5：停止

```bash
bash scripts/wsl2-stop.sh
```

## 配置说明

全部配置通过 `docker/.env` 文件设定（参见 `docker/.env.example` 模板）：

| 变量 | 说明 | 默认值 |
|------|------|--------|
| `DEEPSEEK_API_KEY` | DeepSeek API Key（`deepseek` provider 时必填） | — |
| `DASHSCOPE_API_KEY` | 阿里 DashScope API Key（`qwen` provider 时必填） | — |
| `CSSWITCH_PROVIDER` | 模型提供商：`deepseek` 或 `qwen` | `deepseek` |
| `CSSWITCH_PROXY_PORT` | 代理监听端口 | `18991` |
| `CSSWITCH_AUTH_TOKEN` | 路径 secret 鉴权（**强烈建议设置**） | — |
| `CSSWITCH_UPSTREAM_URL` | 上游 API 地址覆盖（高级用法） | — |

> **安全**：`DEEPSEEK_API_KEY` 与 `DASHSCOPE_API_KEY` 只通过 `.env` → 环境变量注入容器，
> 不会出现在镜像层、git 历史或命令行参数中。

## 网络说明

### WSL2 IP 地址

WSL2 使用虚拟化网络，其 IP 地址可能随重启变化。`wsl2-deploy.ps1` 会自动探测当前 IP 并输出。

```powershell
# 手动获取 WSL2 IP：
wsl hostname -I
```

### Docker Desktop 模式

使用 Docker Desktop with WSL2 backend 时，容器可通过 `localhost` 直接访问（Docker Desktop 自动端口转发）：

```powershell
curl.exe http://localhost:18991/<secret>/health
```

### 传统 WSL2 模式

在 WSL2 发行版内直接安装 Docker Engine 时，需要 **通过 WSL2 IP 访问**：

```powershell
curl.exe http://<wsl2-ip>:18991/<secret>/health
```

### Windows 防火墙

如果从局域网其他机器访问，需要在 Windows 防火墙中放行 WSL2 代理端口：

```powershell
# 以管理员身份运行
New-NetFirewallRule -DisplayName "CSSwitch Proxy" -Direction Inbound -LocalPort 18991 -Protocol TCP -Action Allow
```

### 客户端配置

Windows 上的 OpenAI/Anthropic 兼容客户端需设置：

```ini
ANTHROPIC_BASE_URL=http://<wsl2-ip>:18991/<secret>/
# 或（使用 Docker Desktop 时）
ANTHROPIC_BASE_URL=http://localhost:18991/<secret>/
```

## 可选：Caddy TLS 反代

容器编排支持可选 Caddy 服务，在 WSL2 内部提供 **自签名 HTTPS**，保护 path secret 不在明文 HTTP 上传输。

启用方式：

```bash
# 构建并启动（包含 Caddy）
docker compose -f docker/docker-compose.yml --profile tls up -d
```

访问地址变为：

```
https://<wsl2-ip>:8443/<secret>/health
```

> **注意**：自签名证书首次访问时浏览器会提示安全风险，确认即可。
> `curl` 需加 `-k` 或 `--insecure` 参数。

## 构建镜像

```bash
bash scripts/docker-build.sh
```

或手动：

```bash
docker build -t csswitch-proxy:latest -f docker/Dockerfile .
```

也可通过 Compose 自动构建（首次 `up -d` 时自动完成）：

```bash
docker compose -f docker/docker-compose.yml up -d
```

## 故障排查

### 代理启动后立即退出

最常见原因——**缺少 API key**：

```bash
# 检查 .env 是否正确
cat docker/.env | grep -E '^(DEEPSEEK|DASHSCOPE)_API_KEY='

# 查看容器日志
bash scripts/wsl2-logs.sh
```

### 端口已被占用

```bash
# 修改 docker/.env 中的端口
CSSWITCH_PROXY_PORT=18992

# 重写 docker-compose.yml 映射（或在 docker-compose.yml 中改 ports）
```

### WSL2 IP 变化

重新运行 `bash scripts/wsl2-deploy.sh` 或 `.\scripts\wsl2-deploy.ps1` 获取新的 IP 地址。

### curl 连接被拒绝

1. 确认容器在运行：`docker ps | grep csswitch-proxy`
2. 确认端口映射正确：`docker port csswitch-proxy`
3. 确认 Windows 防火墙未拦截：临时关闭防火墙测试
4. 在 WSL2 内测试回环地址：`curl http://127.0.0.1:18991/<secret>/health`

### Docker Desktop 无法启动 WSL2 后端

- 确认 WSL2 已安装：`wsl --set-default-version 2`
- 确认发行版版本为 2：`wsl -l -v`
- 升级：`wsl --set-version <发行版名> 2`
- 在 Docker Desktop Settings → Resources → WSL Integration 中启用集成

## 安全提示

- **path secret 保护**：容器到客户端的 HTTP 连接在本地 WSL2 桥上为明文。
  在不可信网络（如公司内网、跨子网）上使用时，强烈建议启用 [Caddy TLS](#可选caddy-tls-反代)。
- **API key 保护**：`docker/.env` 包含你的第三方 API key，请勿提交到 git。
  `.env` 已在 `.gitignore` 中（被 `.dockerignore` 排除），但仍需注意不要手动 `git add` 它。
- **容器权限**：容器内以非 root 用户（`csswitch`）运行，最小权限原则。
- **监听地址**：容器内默认监听 `0.0.0.0`（由 Dockerfile 的 `ENTRYPOINT` 指定 `--host 0.0.0.0`）；
  在本机使用 `127.0.0.1:18991`（通过 Docker Desktop 的端口转发），局域网可用 WSL2 IP 访问。
- **端口 8765 禁止**：脚本与 compose 配置均拒绝使用 8765 端口（真实 Claude Science 保留端口）。

## 限制

- **Claude Science 不在此范围内**：容器化的是翻译代理，Claude Science 仍需运行在 macOS 上。
- **Windows 上无 Claude Science**：目前 Anthropic 未提供 Windows 版 Claude Science，
  因此本容器方案面向的是其他 OpenAI/Anthropic 兼容客户端的用户。
- **实验性**：WSL2 容器化部署为实验性功能，未经过生产环境的充分验证。
- **网络依赖**：WSL2 网络架构（NAT）可能在某些 VPN/代理环境下需要额外配置。
