# CSNative 架构参考

Reviewed snapshot：`eust-w/CSNative@64a68b1`。

这是责任边界比较，不是 CSSwitch 当前事实，也不是代码来源。

该快照提供的架构启发是：wrapper 选择配置、准备 runtime 资源、使用稳定 data-dir 启动 Science 并管理进程生命周期；升级可复用 data-dir，而 executable / runtime 随版本变化。配置和组织选择影响启动上下文，不要求 wrapper 接管 Science 存储的每项能力。

CSNative 不需要另一套 Skill 平台来保持 Science Skill 数据；组织和 Skills 自然位于被复用的 Science data-dir。CSSwitch 采用这一所有权边界，同时保留自己的 provider 转换与 Rust Gateway 实现。

不得复制 CSNative 实现代码。更新本参考时必须重新固定 reviewed commit，只记录该快照能支持的行为；不能把外部项目文档当成 CSSwitch runtime 或 release 证据。
