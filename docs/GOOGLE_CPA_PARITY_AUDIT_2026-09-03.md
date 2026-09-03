# Google / Antigravity 与 CPA 链路对照

核对日期：2026-09-03。CPA 固定参考提交：`17a65ee5470fbaf0e22fc219381e6a4ae9e07624`。

范围：Codex → Switcher → Google Antigravity → 工具调用 → Codex 本机执行 → 工具结果 → 后续回答。不是对 CPA 所有供应商、管理接口和产品功能的等价性声明。未运行 CPA 服务，也未引入 CPA 源码、依赖或系统提示词。

## 本次故障与修复

真实 Codex CLI（使用现有模型缓存/用户目录、忽略 config，临时工作目录、ephemeral）发出的请求包含 `input[].type=additional_tools`，没有顶层 `tools`。旧实现只读取顶层 tools。实测旧适配在此请求上返回 `MALFORMED_FUNCTION_CALL`；工具声明补齐后，又暴露 Google Schema 不接受 `encrypted` 注解的 HTTP 400。两项修复后，相同测试完成工具执行及回答。

另外补齐自定义工具 output 的 `custom_tool_call/input`、namespace 和 done 事件，避免把它误报为普通 `function_call/arguments`。这些是确定的缺口；原先两个用户任务没有保存原始 Google 响应，因此不声称已经证明其每一次失败都由同一个 finishReason 引起。

## 全链路矩阵

| 阶段 | CPA 参考实现 | Switcher 当前情况 | 差距 / 后续事项 |
| --- | --- | --- | --- |
| Google OAuth / Token | Antigravity auth、executor token refresh | 已有 Google 独立账号；Client 从 MiniMac 租用 ST，本机请求 Google | 架构有意不同，不把 RT 移回本机；本次未重测全套 OAuth 登录 |
| 模型与额度选路 | 动态模型、账号选择、短限流/耗尽分支 | 已有动态目录、模型额度、独立当前号、失败后账号遍历 | 429 目前统一按模型耗尽处理，没有 CPA 的短冷却与真实耗尽细分 |
| 工具声明来源 | `responsesToolSources` 同时读根 tools 和 additional_tools | **本次补齐**，additional 定义覆盖同名根定义，避免重复声明 | 声明顺序变化造成的长名/冲突短名稳定性仍可加强 |
| 工具身份 | 正反向 name / namespace / custom 映射 | **本次补齐**，声明、调用、回传统一映射 | 大规模同名/超长工具集需更多测试 |
| Schema | 多层级 Schema 清洗 | 已有清洗；**本次补齐 encrypted 注解过滤**，保留同名参数 | `$ref/$defs/allOf` 当前删除，anyOf/oneOf 选择分支，复杂约束非无损 |
| 工具选择 | auto / none / required / 命名选择 | **本次补齐** Google functionCallingConfig 与允许列表 | Claude thinking 与强制工具选择互斥：明确报错，不能同时启用；不静默改思考深度 |
| 自定义工具 | 解包 input，输出 custom_tool_call 与完成事件 | **本次补齐**，支持 Unicode、换行、JSON 转义和分片输入 | 不在 Google 侧执行 Lark/regex grammar 约束；客户端仍验证，模型可能需纠正一次参数 |
| 普通函数 | JSON args、call_id、流事件 | 保持 function_call；**本次补齐**命名空间恢复与 arguments.done | 工具本身仍由 Codex 执行，不在代理里执行 |
| 工具结果续接 | 普通/custom 输出、调用配对及重排 | 字符串输出已验证；**本次补齐** custom 历史输入 JSON 包装 | 多个并行结果乱序、缺少调用项、重复结果的系统性重排/去重尚不等价 |
| 推理签名 | carrier、按项/方向/目标关联、服务端 replay cache | **本次补齐**通过 encrypted_content 携带签名，Codex 丢弃扩展字段后仍可恢复 | 目前单响应共享签名；多签名、多段 thought、跨模型切换/压缩历史重放未全面覆盖 |
| HTTP SSE | 完整事件转换，clean EOF 收尾 | 已验证；**本次补齐**单调 sequence、多个 index=0 工具帧不合并、心跳跳过 | message/reasoning 的细粒度 part/done 事件仍少于 CPA，Codex 已接受但不宣称对所有客户端完全等价 |
| 非流式 | 独立非流式转换 | **本次统一**复用相同工具身份、签名、终止验证；单测覆盖 | 未针对所有模型做非流式实测 |
| 错误终止 | executor 读错误与正常 EOF 区分 | **本次补齐** error、blockReason、异常 finish、MAX_TOKENS 与传输错误，不再一律 completed | clean EOF + 完整工具 + usage 允许无 finishReason；其余无结束标志保守失败 |
| WS 会话 | previous request/output/id、增量合并、pending tool ids、compaction | 已有 WS→本机 HTTP 桥；工具两轮同连接已验证 | **没有 CPA 的服务端增量会话缓存**，依靠空 response.id 让 Codex 重发完整 input |
| WS 预连接 | 存储/复用预连接上下文 | **本次修正** generate:false 不再发非空虚构 id | 不支持依赖预连接 id 的第三方增量客户端；不能把空预连接响应当真实推理成功 |
| 请求错误与换号 | 调度层控制错误分类与重试 | 401 ST 强刷已有；**本次修正** HTTP 400 不再遍历全部 Google 号 | 403、429 短限流、容量不足、Retry-After、流开始前重试策略还需细分 |
| 多模态工具结果 | 图片输出块解析、inlineData、functionResponse parts | 普通文本工具结果支持 | 工具返回图片、base64/data URL、文件 MIME、图像输出事件链路未完整覆盖 |
| 托管工具 | Google grounding / 搜索等专门适配 | 本机已经声明的函数工具可用 | OpenAI 托管 web_search / tool_search 不能视为已支持；MCP 动态发现另需适配 |
| 参数与用量 | temperature/top_p、生成 Schema、用量细分 | reasoning/maxOutputTokens 已有；基本 input/output/total | 无 reasoning 时的 max_output_tokens、temperature/top_p、结构化输出映射需补齐；cached/thought token 细分不足 |
| 连接治理 | 流 context 取消、keepalive 等 | 按账号连接池、metadata 预热已有 | idle/read timeout、WS 中途取消与心跳策略还需专项验证 |

## 验证证据

- `src-tauri/src/antigravity/tool_tests.rs`：additional_tools、重复声明、自定义类型、namespace、签名经过 Codex 式序列化后续接、分片/转义、多调用、非法结束、工具选择、Relay 未启用 Google 转换、非流式签名、无 finishReason 的完整工具帧。
- `scripts/google-tool-roundtrip.mjs 18082`：Gemini 3.8 自定义 exec 两轮 SSE 成功，不执行模型生成代码。
- 同脚本 `18082 claude-sonnet-4-6`：Claude thinking + auto 选择，两轮成功。强制选择 + thinking 的上游互斥限制已实测。
- 同脚本 `18082 gemini-3.8-flash-high ws`：同一 WebSocket 两轮工具结果续接成功。
- `scripts/google-codex-probe.mjs 18082`：真实 CLI 普通函数执行 pwd 并回复；Gemini 和 Claude 均成功。
- `GOOGLE_PROBE_USE_USER_HOME=1 ...`：使用用户现有模型缓存的 Responses Lite/additional_tools 请求，Gemini 成功执行 pwd 并续接。
- `GOOGLE_PROBE_USE_USER_HOME=1 GOOGLE_PROBE_CUSTOM=1 ...`：真实 CLI 成功使用自定义 apply_patch 在临时目录建立 probe.txt，再通过命令工具读取并回复标记。此测试有模型纠正工具输入的回合，不能据此宣称 grammar 在上游被强制执行。
- 正式构建：195 项 Rust 测试通过、1 项忽略；TypeScript/Vite/Tauri 构建通过。Gemini 非流式请求实际返回 `SYNC_OK`。
- 部署后本机实际 Codex CLI（Responses Lite）执行 pwd、回传结果、最终回答通过；本机 Gemini WebSocket 和 Claude SSE 自定义工具两轮通过。
- MiniMac 部署后，同一 WebSocket 预连接、自定义工具调用、结果回传、最终回答全部通过。
- 未修改、重放或清理用户提供的两个故障任务，也未运行其路由器/Apple Pay 业务操作。

## 部署记录

- 本机与 MiniMac 均已安装同一正式构建，应用二进制 SHA-256：`7c8df12f1ce6a94ad5eb6deb001d034b1080d6440a21c9d6e5a4f75d61c9ccfd`。
- 两台均保留 `/Applications/Codex Switcher.app.backup-20260903-141622`；仅重启 Switcher，没有重启 Codex Desktop。
- 本机测试端口 18082 已关闭，正式代理恢复使用 18080。MiniMac 18080 / 18081 监听正常。
- 新旧代理 GPT 目录过滤结果校验一致；本次改动未修改 GPT 选路/凭证流程。

## 建议下一阶段顺序

1. P1：429 短限流与真实耗尽分类，Retry-After；避免一次短限流把某模型长期标记耗尽。
2. P1：按调用关联多个 thought signature、并行工具结果配对、复杂历史回放测试。
3. P1：图片/文件工具结果、多模态往返；这是读取截图/浏览器结果时的重要缺口。
4. P2：WS 增量会话、断线恢复与压缩历史；当前 Codex 使用完整 input，不急于改变默认路径。
5. P2：复杂 Schema、生成参数、结构化输出、token 分类、托管工具和取消/超时治理。

## 固定源码参考

- [CPA 工具来源与身份映射](https://github.com/router-for-me/CLIProxyAPI/blob/17a65ee5470fbaf0e22fc219381e6a4ae9e07624/internal/util/responses_tools.go)
- [CPA Responses → Gemini](https://github.com/router-for-me/CLIProxyAPI/blob/17a65ee5470fbaf0e22fc219381e6a4ae9e07624/internal/translator/gemini/openai/responses/gemini_openai-responses_request.go)
- [CPA Gemini → Responses](https://github.com/router-for-me/CLIProxyAPI/blob/17a65ee5470fbaf0e22fc219381e6a4ae9e07624/internal/translator/gemini/openai/responses/gemini_openai-responses_response.go)
- [CPA WS 历史处理](https://github.com/router-for-me/CLIProxyAPI/blob/17a65ee5470fbaf0e22fc219381e6a4ae9e07624/sdk/api/handlers/openai/openai_responses_websocket_requests.go)
- [CPA Antigravity 流执行器](https://github.com/router-for-me/CLIProxyAPI/blob/17a65ee5470fbaf0e22fc219381e6a4ae9e07624/internal/runtime/executor/antigravity_executor_stream.go)
- [CPA 上游无 finishReason 回归测试](https://github.com/router-for-me/CLIProxyAPI/blob/17a65ee5470fbaf0e22fc219381e6a4ae9e07624/internal/runtime/executor/antigravity_executor_finish_reason_test.go)
- [OpenAI 工具协议](https://developers.openai.com/api/docs/guides/function-calling)
