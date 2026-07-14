# 测试与证据规则

- 证据层必须分开：源码 / 单测、构建 artifact、临时安装副本、已安装 runtime、真实 provider、签名 / 公证 / Gatekeeper、公开 release。
- 默认总入口是 `bash test/run_all.sh`；只有五层全部通过且没有环境阻塞，才可写 `release-ready green`。
- 默认模式无失败但存在缺失依赖或需真机项目时，只能写 `current-env clean`，并列出 `ENV-BLOCKED` / `NEEDS-REAL-MACHINE`。
- 文件复制、Science 发现、Agent attach、Skill load / trigger、领域功能执行、重启持久化是不同结论。
- mock / loopback 不能写成 live provider；源码测试不能写成 installed runtime；本地 release 元数据不能写成公开发布。
- 失败、未运行、环境阻塞、需人工判断不得记为通过。
- 运行真机或已安装 runtime 测试前遵守[真机验收](../../docs/operations/real-machine-acceptance.md)的隔离护栏。
- 报告命令、目标 commit / artifact、环境、退出码和脱敏证据；不要只记录历史 pass 数量。
