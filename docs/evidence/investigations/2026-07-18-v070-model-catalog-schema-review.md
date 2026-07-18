# v0.7.0 多模型目录：schema / preset 主审与修改报告

日期：2026-07-18

范围：`config`、provider contract、template registry、model preset、显式 v2 downgrade

隔离边界：只读/修改目标 worktree；测试均使用临时目录或纯单元测试；未读取正式配置、真实凭据或 Science 数据。

## 主审结论

修改前存在四个 P0：当前 `Config` 与 v3 迁移类型耦合、未知字段会丢失、旧 fixed policy 与新目录 policy 混用、v2 downgrade 只处理 Codex 而会静默丢弃静态目录。四项均已修改，schema/catalog 可以作为 gateway 严格 resolver 的输入合同。

## 已修改

- canonical schema 升为 v4；新增独立只读 `ConfigV3` / `ProfileV3` / `ModelPolicyV3`。迁移链固定为 v1→v2→v3→v4、v2→v3→v4、v3→v4，v4 才直接加载。
- v4 profile policy 只允许 `saved_catalog` 或 `dynamic_catalog`；provider contract 也只声明这两项。旧 `optional_fixed` / `required_fixed` 仅在 v3 类型中存在。
- v3 迁移在完整内存校验后保存 byte-identical、不可覆盖的 `config.json.v3.bak`；v4 原子发布后重新读取并做字节及语义校验，失败会原子恢复原始配置。
- 顶层、profile、Codex 网络设置、model route、role bindings 均透传未知 extension；显式降级遇到未知 extension 时 fail closed。
- 新增版本化 `catalog/model-presets.v1.json`。DeepSeek、GLM、MiMo、SiliconFlow、Kimi、MiniMax、OpenRouter、Qwen 各自独立 preset；三类 custom 为 manual/discovered；Codex 为 dynamic。
- v3 DeepSeek/Qwen 迁移复制完整当前 preset，并保留旧 shell、upstream、已持久化 selector 或未知精确模型的默认选择。未知模型作为 manual route 保留，不静默替换。
- relay/custom 的旧非空 model 迁成一条 route，四个 role 绑定默认项；旧空 model 保留为未完成 profile，若原本激活则取消激活并追加 notice，不发明 upstream。
- selector v1 冻结为 `claude-csswitch-<namespace>-<slug>-<12 hex>`；48-bit digest 是 CSSwitch 碰撞策略，Science 兼容合同只要求已验证的 `claude-csswitch-` ASCII 语法类。现有 Codex alias 不重命名。
- profile 最多 64 route、目录 JSON 最大 64 KiB；selector/display/upstream 长度与字符受限，控制字符和已知 `supports_tools=false` route 被拒；selector 唯一但 upstream 可以重复。
- canonical v4 不再序列化旧 `model`；当前 Rust 内存中暂保留由 default route 投影的兼容影子，供 gateway/UI 分阶段迁移，落盘真源只有目录/default/roles。
- v2 downgrade 会先导出 Codex metadata 和所有 saved catalog metadata，再投影默认 upstream；导出不含 API key、credential ref、base URL、path secret 或 extensions。预览带安全 fingerprint，执行时配置或目录变化即拒绝。

## 已验证

- `cargo test --manifest-path desktop/src-tauri/Cargo.toml config::tests --lib`：58 passed。
- `cargo test --manifest-path desktop/src-tauri/Cargo.toml model_catalog::tests --lib`：4 passed。
- `cargo test --manifest-path desktop/src-tauri/Cargo.toml templates::tests --lib`：13 passed。
- `cargo test --manifest-path desktop/src-tauri/Cargo.toml provider_contracts::tests --lib`：7 passed。
- `cargo test --manifest-path desktop/src-tauri/Cargo.toml commands::codex::tests::downgrade_preview_and_confirmation_are_complete_and_secret_free --lib`：1 passed。

覆盖包括版本探测、原始 v3 backup、unknown-field roundtrip、旧 native model 变体、未完成静态 profile、selector golden/碰撞边界、template/preset registry coverage、降级安全投影及 stale preview。

## 留给后续模块的合同

- `/v1/models` 必须由 resolver 把 default 放在响应第一项；保存数组本身保持用户顺序。Science 会自行排序模型菜单，不能把响应顺序当作 Science 默认选择合同。
- discovery 的逐项 `origin` / `availability` / `supports_tools` 与“只有权威非空刷新可标 `not_reported`”由 gateway/UI 阶段实现。
- 旧内存 `Profile.model` 影子必须在 gateway/UI 完成后尽量缩小使用面；不得重新成为持久化真源。
