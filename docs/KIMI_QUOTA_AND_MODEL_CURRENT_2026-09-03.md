# Kimi 编程套餐额度与中转模型当前号

## 额度

- Kimi Coding 使用配置的 Base URL + `/usages`，官方 `/coding` 自动规范为 `/coding/v1/usages`；静态 Key 只发往配置的服务，不伪造 KimiCLI 的 User-Agent。
- `RelayUsageCache.windows` 为可选、向后兼容的新字段，分别存储 5H 与 7D 剩余百分比和 Unix 重置时间。
- 自动识别官方 Coding 地址；自定义中转需显式选择 `kimi_coding` 额度策略。显式选择“不拉取”不会自动探测。
- 启动后自动查询，随后每分钟更新；保留手动刷新。每台 Switcher 使用本机保存的该账号 Key 进行只读查询，不改变 RT/ST 所有权。
- 缺失值显示 `--`；超过三分钟未更新提示数据过期。重置时间到达不擅自填满 100%，等待上游新数据。
- 查询不发模型生成请求，不开启加油包，不自动购买/消费额外额度，不更改当前账号。
- 已实际验证本机和 MiniMac 自动刷新，两个时间戳连续增长；额度曾读到 5H 100%、7D 42%（2026-09-03 15:38 台北），数值会随使用变化。

## 独立当前号

- `AppSettings.current_relay_accounts` 按实际 upstream model ID 存储当前 Relay account ID。不同模型互不覆盖，且不修改 `AccountStore.current`、Google 当前号或 Codex auth.json。
- 同一模型多个来源：只显示一个 `relay-current:<model>` 可选模型；请求按当前映射选择账号、地址与 Key。
- 旧 `relay-model:<account-id>:<model>` 保留为隐藏目录项，避免旧任务的模型被判定停用。旧标识同样跟随该模型的新当前号；不再永久钉死旧账号。
- 账号行显示 Kimi 当前 / DeepSeek 当前；不同模型分别选择时可显示“部分模型当前”，tooltip 列出模型 ID。
- 行内切号按钮将该账号配置的模型设为当前；底层独立命令也支持指定一个模型。其它模型的当前选择不变。
- 首个账号自动建立初始选择；删除当前账号后修复选择。手动指定的账号若失效，不擅自换到另一个可能收费的来源。
- 保存其它设置时保留最新模型选择，避免旧设置表单覆盖刚完成的切号。
- 本阶段适用于原生 Responses 中转；旧 Chat Completions 中转流程保留。上游 alias 不同（如 `k3` 与 `kimi-k3`）视为不同配置模型。
- 每台设备的模型当前映射为本地路由状态，与既有 Google 当前号的本地语义一致。

## 验证

- 全套 Rust：207 passed，2 ignored；显式运行 Kimi 真实 Key 的只读额度测试另 1 passed。
- 解析测试覆盖数字/字符串、已用零、剩余零、未知窗口、零上限、URL 隔离。
- 当前号测试覆盖不同模型独立切换、跨重启序列化、保留 Codex/Google 当前号、旧别名跟随新选择、删除后修复、失效不偷偷换源。
- 隔离代理模拟测试：canonical / 旧 alias 均命中第二个 Kimi 来源的地址和 Key，DeepSeek 路由保持独立；HTTP 工具结果往返与 WS 均通过。
- Chrome 验收真实组件：Kimi B → Kimi A 后，DeepSeek A 仍为当前；额度条、倒计时、未知/过期状态、编程套餐预设均通过。全部 UI fixture 使用模拟账号。

## Kimi 首次响应调查与 WS 修复

- 任务 `01a06642-76df-7203-a143-3de8a3a1bd52`（确认当前模型）使用 K3 high；395 秒后中断，期间无助手输出或工具调用。未改写该任务历史，也未发送继续消息。
- 日志确认它的握手携带 `x-codex-routing-hint: model=relay-model:...:k3`，旧路径却先连接 ChatGPT WebSocket，再通过首帧判断目标模型。
- 新路径在有明确 provider 模型提示时直接完成本机 WS 握手，不查询/预检 ChatGPT 当前号、不先建 ChatGPT 上游连接；无提示的老客户端保留首帧兜底。
- 把握手/前帧的 model 保留为后续帧的默认值；显式 model 始终优先。不支持的 response.append 明确失败，不静默等待。
- 桥接错误发送可识别的 response.failed 并关闭连接，避免只发一个不完整 error 后无限等待。
- 隔离验收：有 hint 且实际帧省略 model，完整响应成功；本地握手约 4ms，mock 上游 WS 连接计数保持为 0。该数字不是 Kimi 推理耗时。
- 单次真实探测：短请求 low 直连 Kimi约 5.2s，Switcher HTTP约 7.4s；复用原任务文本上下文但不含工具声明，high 档约 12.7s。非严格性能基准，不将顺序请求差值全部归因于代理。
- 已确认额外握手问题；原任务长时间卡住的全部因素仍需其重试后的端到端验收，不能声称所有 K3 high 请求均会在上述时间内返回。
- 发布后真实 Kimi WS（沿用用户旧 model alias 提示、后续帧省略 model）：握手 5ms、首事件约 17.264s、最终 response.completed / OK。说明路由可工作，但 Kimi 上游等待仍有明显波动，不能承诺固定首包速度。

## 最终发布

- 本机、MiniMac 最终构建二进制 SHA-256：`747653ece5d0af85f9d23d8d86369d45d0aa7ef52a341f91aaf1ae760baaec45`。
- 两台均有旧应用备份：`/Applications/Codex Switcher.app.backup-20260903-162209`。
- 临时 mock、debug proxy、浏览器 fixture 已关闭；只保留正式服务。
- 发布期间观察到原有 FastAuthSync 对齐了 Codex current，随后复核本机和服务端均与发布前选择一致；没有为此改写原有 GPT 切号/同步逻辑。

## 账号列排版

- 中转账号名与 PLAN/当前号标签改为上下两行；常规窗口账号列 220px，窄窗口 180px，把余下空间分配给额度列。
- Chrome 实测 1200px 下表头/行均 220px，900px 下均 180px，名称与标签垂直间距 6px；720px 下响应式换行无横向溢出。
