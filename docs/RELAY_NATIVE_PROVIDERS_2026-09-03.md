# Kimi / DeepSeek 原生 Responses 中转接入

## 用户可见行为

- 添加中转卡片统一展示公司/服务名称，不再展示模型版本。版本仅出现在默认模型配置和 Codex 模型选择器。
- Kimi 默认 `https://api.moonshot.cn/v1`、`kimi-k3`，原生 Responses。
- DeepSeek 默认 `https://api.deepseek.com`，默认 Pro，同时配置 Flash / Flash Vision 可选项，原生 Responses。保留用户提供的路径前缀，仅追加 `/responses`，不强制添加 `/v1`。
- 两家 API 地址、默认模型 ID、协议均可编辑。免费/自有中转使用它自己的地址、凭据及实际模型 ID；不强制回官网。
- 原生 Responses 中转的兜底模型及映射表目标模型进入 Codex 目录，名称包含账号名，内部 slug 为 `relay-model:<account-id>:<upstream-model>`。同名模型在不同中转下不会串号。
- 按模型独立选路，不需要修改当前 ChatGPT/Google 账号。下游只收到该中转 Key；不发送客户端 ChatGPT 凭据、账号 ID、Cookie。
- 使用 HTTP 原生 Responses 透传，不走 Chat Completions 转译，不运行厂商的一键改配置脚本，不复制其系统提示词。

## 边界

- 本阶段只给原生 Responses 中转增加独立模型目录。旧 Chat Completions 中转按原有方式使用，未自动升级、未改写已保存账号。
- 中转必须提供所选协议及模型；“免费”不代表无鉴权或支持全部官方模型。不会替用户声称服务免费/可用。
- 目录来自配置的模型 ID，并非已通过 Key 从上游发现并验证的模型列表。新账号添加后 Codex 需要刷新模型目录才能显示。
- 已知 Kimi / DeepSeek V4 使用官方 1M 上下文和 low/high/max 档位；自定义模型名不猜测其能力，保守提供基础元数据。
- 原生中转元数据优先 HTTP。后续修复让携带明确 routing hint 的 WS 直接分流到本机 HTTP 桥；无 hint 老客户端仍保留首帧兜底。未引入跨供应商服务端会话缓存。
- 本轮没有提供真实 Kimi / DeepSeek Key，因此不宣称真实上游对话或工具调用已经通过。

## 验证

- Rust 全套测试：201 passed，1 ignored；TypeScript/Vite/Tauri 正式构建通过。包含同品牌多个 Kimi / DeepSeek 账号不会覆盖的回归测试。
- `relay_catalog` 单测验证公司模型元数据、URL 自定义前缀、原生工具历史保持、失效模型拒绝、当前账号不变、凭据隔离。
- `scripts/relay-native-mock.mjs` + `scripts/fixtures/relay-accounts.json` + `scripts/relay-native-smoke.mjs`，使用 debug-only `CODEX_SWITCHER_TEST_HOME` 在临时目录运行隔离账号库。
- 完整模拟链路通过：/models → 分账号 /responses → custom_tool_call → custom_tool_call_output → 回答；自定义 DeepSeek API 路径正确；WS 路径通过；不存在的模型返回 400，不回落 GPT。
- 内置浏览器验收真实 AddRelayModal 组件（Chrome 扩展连接失败后切换）：公司名称、默认地址、协议、模型 ID、Kimi 和 DeepSeek 地址编辑均通过。组件 fixture 无真实账号，不提交凭据。

## 部署

- 本机与 MiniMac 使用相同构建，二进制 SHA-256：`20853ce2792f243d9362f25c6663bb2e44046fe7de8ed8c6555e9a237da911a1`。
- 两台均保留旧应用 `/Applications/Codex Switcher.app.backup-20260903-145308`。
- 发布后本机 Google WebSocket 工具往返回归通过；新本机与旧 MiniMac 同时读取的模型目录逐项一致（尚未配置真实中转账号）。
- 本地组件页面 1280px 验收无横向溢出；临时浏览器标签、1422/18082/18090 测试服务已关闭。

## 官方依据

- [Kimi Codex 接入](https://platform.kimi.com/docs/guide/codex-kimi)
- [Kimi Responses API](https://platform.kimi.com/docs/api/responses)
- [DeepSeek 首次调用](https://api-docs.deepseek.com/zh-cn/)
- [DeepSeek Codex 接入与模型元数据](https://api-docs.deepseek.com/zh-cn/quick_start/agent_integrations/codex)
