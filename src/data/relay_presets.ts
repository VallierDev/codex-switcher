/**
 * 中转站 / OpenAI 兼容服务的预设列表。
 *
 * 选预设后会自动填 base_url + usage_preset，用户只需贴 API Key。
 *
 * `usage_preset` 字段命中后端 Rust 内置 fetcher 名（见 `usage.rs`）：
 *   - "openai_compat": GET {base}/v1/usage with Bearer
 *   - "mimo_token_plan": MiMo 控制台 Cookie → /api/v1/tokenPlan/usage
 *   - null: 不拉余额（中转站没标准 usage 接口时用）
 *
 * 加新条目时：除非中转站确实暴露 OpenAI 兼容的 /v1/usage，否则 usage_preset 用 null。
 */
export interface RelayPreset {
    /** 内部唯一标识（不展示） */
    id: string;
    /** 默认账号名（用户可改） */
    name: string;
    /** 必填，OpenAI 兼容 base_url，不带尾斜杠 */
    base_url: string;
    /** 中转站主页（用户参考，可选） */
    homepage?: string;
    /** 后端内置 usage fetcher preset 名；null = 不拉余额 */
    usage_preset?: string | null;
    /** UI 显示用的一行说明 */
    description?: string;
    /** 模型映射兜底（codex 端发的所有未命中 map 的 model 都替换成它） */
    model_fallback?: string | null;
    /** 模型映射表（key=客户端 model，value=中转站 model） */
    model_map?: Record<string, string> | null;
    /**
     * 上游协议 wire format：
     * - "responses"（默认 / 不填 = 等价）—— 上游原生支持 codex /v1/responses（Unity2、ChatGPT 子集、OpenAI key）
     * - "chat_completions" —— 上游只懂 /chat/completions（GLM/MiMo Coding Plan / 通用 OpenAI Chat），proxy 翻译
     */
    relay_protocol?: 'responses' | 'chat_completions';
    /** 展示用：1–3 字符 monogram（卡片左侧图标的文字） */
    mark?: string;
    /** 展示用：monogram 背景色（hex） */
    color?: string;
    /** 展示用：在 AddRelay 卡片选择器中的分组 */
    group?: '通用中转' | 'CODING PLAN' | '三方模型' | '自定义';
    /** 默认 API Key 前缀（占位提示用） */
    auth_prefix?: string;
    /**
     * 业务分类（用于 UI 过滤胶囊 + 行内标签）：
     * - `aggregator` —— 聚合中转（new-api/sub2api/CLIProxyAPI 一类的 reseller，PinCC/Unity2/FreeModel/PackyCode 等）
     * - `coding_plan` —— 厂商自家的 Coding Plan / Token Plan 订阅（GLM Coding Plan / MiMo Token Plan / 火山 Coding Plan 等）
     * - `third_party` —— 厂商按量付费 API（DeepSeek / Kimi / OpenRouter / Fireworks 等）
     */
    category?: 'aggregator' | 'coding_plan' | 'third_party';
}

export const RELAY_PRESETS: RelayPreset[] = [
    {
        id: 'kimi_coding', name: '月之暗面（Kimi）',
        base_url: 'https://api.kimi.com/coding/v1',
        homepage: 'https://www.kimi.com/code/console',
        usage_preset: 'kimi_coding', relay_protocol: 'responses',
        model_fallback: 'k3-256k', model_map: null,
        description: '编程套餐：5H / 7D 额度；使用 Kimi Code Key，模型按会员权限选择',
        mark: 'K', color: '#0F0F10', group: 'CODING PLAN', auth_prefix: 'sk-kimi-', category: 'coding_plan',
    },
    {
        id: 'glm',
        name: '智谱',
        base_url: 'https://open.bigmodel.cn/api/paas/v4',
        homepage: 'https://docs.bigmodel.cn/cn/guide/develop/openai/introduction',
        // GLM 余额走自家 monitor 接口（不是 OpenAI /v1/usage）
        usage_preset: 'glm_zhipu',
        // codex 端发的 gpt-5.5 / gpt-4o 等 GLM 不认识，统一映射成 glm-5.1
        // 用户可在表单里覆盖（如 gpt-4o-mini → glm-5.1-x）
        model_fallback: 'glm-5.1',
        model_map: {
            'gpt-5.5': 'glm-5.1',
            'gpt-5': 'glm-5.1',
            'gpt-5-codex': 'glm-5.1',
            'gpt-4o': 'glm-5',
            'gpt-4o-mini': 'glm-5.1-x',
            'o1': 'glm-5.1',
            'o1-mini': 'glm-5.1-x',
        },
        description: '开放平台 API；OpenAI 兼容，支持模型映射',
        mark: 'GLM', color: '#4F46E5', group: '三方模型', auth_prefix: 'sk-',
        category: 'third_party',
    },
    {
        id: 'glm_coding',
        name: '智谱',
        // GLM Coding 套餐专属端点（与普通 paas/v4 不同）；只暴露 /chat/completions
        base_url: 'https://open.bigmodel.cn/api/coding/paas/v4',
        homepage: 'https://docs.bigmodel.cn/cn/guide/start/coding-plan',
        usage_preset: 'glm_zhipu',
        relay_protocol: 'chat_completions',
        model_fallback: 'glm-5.1',
        model_map: {
            'gpt-5.5': 'glm-5.1',
            'gpt-5': 'glm-5.1',
            'gpt-5-codex': 'glm-5.1',
            'gpt-4o': 'glm-5',
            'gpt-4o-mini': 'glm-5.1-x',
            'o1': 'glm-5.1',
            'o1-mini': 'glm-5.1-x',
        },
        description: 'GLM Coding Plan（codex /v1/responses ↔ /chat/completions 翻译，内置）',
        mark: 'GLM', color: '#4F46E5', group: 'CODING PLAN', auth_prefix: 'sk-',
        category: 'coding_plan',
    },
    {
        id: 'mimo_token_plan_sgp',
        name: '小米',
        // MiMo Token Plan 专属端点；官方文档说明 MiMo 暂不适配 Responses API，只适用于 Chat Completions。
        base_url: 'https://token-plan-sgp.xiaomimimo.com/v1',
        homepage: 'https://platform.xiaomimimo.com/console/plan-manage',
        usage_preset: 'mimo_token_plan',
        relay_protocol: 'chat_completions',
        model_fallback: 'mimo-v2.5-pro',
        model_map: {
            'gpt-5.5': 'mimo-v2.5-pro',
            'gpt-5': 'mimo-v2.5-pro',
            'gpt-5-codex': 'mimo-v2.5-pro',
            'gpt-4o': 'mimo-v2.5-pro',
            'gpt-4o-mini': 'mimo-v2.5-pro',
            'o1': 'mimo-v2.5-pro',
            'o1-mini': 'mimo-v2.5-pro',
        },
        description: 'Token Plan 订阅（tp-key；配额用控制台 Cookie）',
        mark: 'Mi', color: '#FF6900', group: 'CODING PLAN', auth_prefix: 'tp-',
        category: 'coding_plan',
    },
    {
        id: 'mimo_api_pay',
        name: '小米',
        // MiMo 按量付费独立端点；跟 Token Plan 的 base + key 形式不同。
        base_url: 'https://api.xiaomimimo.com/v1',
        homepage: 'https://platform.xiaomimimo.com/console/api-keys',
        usage_preset: null,
        relay_protocol: 'chat_completions',
        model_fallback: 'mimo-v2.5-pro',
        model_map: {
            'gpt-5.5': 'mimo-v2.5-pro',
            'gpt-5': 'mimo-v2.5-pro',
            'gpt-5-codex': 'mimo-v2.5-pro',
            'gpt-4o': 'mimo-v2.5-pro',
            'gpt-4o-mini': 'mimo-v2.5-pro',
            'o1': 'mimo-v2.5-pro',
            'o1-mini': 'mimo-v2.5-pro',
        },
        description: 'MiMo 按量付费（sk-key；非 Token Plan，无配额限制按用量计费）',
        mark: 'Mi', color: '#FF6900', group: '三方模型', auth_prefix: 'sk-',
        category: 'third_party',
    },
    // ────────────────────────────────────────────────────────────────
    // A 类：通用 Responses 中转 —— 基于 new-api / sub2api / CLIProxyAPI
    // 的第三方 codex 中转都用这条。用户自己填 base_url + Bearer key。
    // ────────────────────────────────────────────────────────────────
    {
        id: 'generic_responses_relay',
        name: '通用中转',
        base_url: '',
        // "auto" → 后端 probe_relay_usage_preset 自动探测：
        //   new-api → /v1/dashboard/billing/* ；sub2api → /v1/usage ；CLIProxyAPI → 不拉取
        usage_preset: 'auto',
        relay_protocol: 'responses',
        model_fallback: 'gpt-5.5',
        description: 'PinCC / PackyCode / AICodeMirror / 自建 CLIProxyAPI 等；余额自动探测',
        mark: '⇄', color: '#64748B', group: '通用中转', auth_prefix: 'sk-',
        category: 'aggregator',
    },
    // ────────────────────────────────────────────────────────────────
    // B 类：厂商 Coding Plan / Token Plan 直连
    // 全部走 chat_completions 翻译。⚠️ 翻译器目前只在 GLM 上完全验证过，
    // 其他厂商可能撞到 reasoning_content / tool_calls 的 quirk，
    // 需要在 relay_translate.rs 里逐个 case-by-case 处理。
    // ────────────────────────────────────────────────────────────────
    {
        id: 'deepseek_api',
        name: 'DeepSeek',
        base_url: 'https://api.deepseek.com',
        homepage: 'https://api-docs.deepseek.com/zh-cn/quick_start/agent_integrations/codex',
        usage_preset: null,
        relay_protocol: 'responses',
        // Models are selectable independently; never advertise them as GPT aliases.
        model_fallback: 'deepseek-v4-pro',
        model_map: {
            'deepseek-v4-pro': 'deepseek-v4-pro',
            'deepseek-v4-flash': 'deepseek-v4-flash',
            'deepseek-v4-flash-vision-exp': 'deepseek-v4-flash-vision-exp',
        },
        description: '原生 Responses API；支持官网或自有中转，API 地址和模型 ID 均可修改',
        mark: 'DS', color: '#1E40AF', group: '三方模型', auth_prefix: 'sk-',
        category: 'third_party',
    },
    {
        id: 'moonshot_kimi',
        name: '月之暗面（Kimi）',
        base_url: 'https://api.moonshot.cn/v1',
        homepage: 'https://platform.kimi.com/docs/guide/codex-kimi',
        usage_preset: null,
        relay_protocol: 'responses',
        model_fallback: 'kimi-k3',
        model_map: null,
        description: '开放平台 API，原生 Responses；使用平台 API Key，非 Kimi Code 订阅密钥',
        mark: 'K', color: '#0F0F10', group: '三方模型', auth_prefix: 'sk-',
        category: 'third_party',
    },
    {
        id: 'minimax_api',
        name: 'MiniMax',
        base_url: 'https://api.minimax.chat/v1',
        homepage: 'https://platform.minimaxi.com/document/',
        usage_preset: null,
        relay_protocol: 'chat_completions',
        model_fallback: 'MiniMax-M2',
        model_map: {
            'gpt-5.5': 'MiniMax-M2',
            'gpt-5': 'MiniMax-M2',
            'gpt-5-codex': 'MiniMax-M2',
            'gpt-4o': 'MiniMax-M2',
            'gpt-4o-mini': 'MiniMax-M2',
            'o1': 'MiniMax-M2',
            'o1-mini': 'MiniMax-M2',
        },
        description: '按量付费 / 订阅服务；OpenAI Chat 兼容',
        mark: 'MM', color: '#7C3AED', group: '三方模型', auth_prefix: 'sk-',
        category: 'third_party',
    },
    {
        id: 'alibaba_dashscope',
        name: '阿里云',
        base_url: 'https://dashscope.aliyuncs.com/compatible-mode/v1',
        homepage: 'https://help.aliyun.com/zh/dashscope/',
        usage_preset: null,
        relay_protocol: 'chat_completions',
        model_fallback: 'qwen3-max',
        model_map: {
            'gpt-5.5': 'qwen3-max',
            'gpt-5': 'qwen3-max',
            'gpt-5-codex': 'qwen3-coder-plus',
            'gpt-4o': 'qwen-plus',
            'gpt-4o-mini': 'qwen-turbo',
            'o1': 'qwen3-max',
            'o1-mini': 'qwen-plus',
        },
        description: '百炼开放平台；OpenAI 兼容模式',
        mark: '通义', color: '#FF6A00', group: '三方模型', auth_prefix: 'sk-',
        category: 'third_party',
    },
    {
        id: 'volcengine_ark',
        name: '字节跳动',
        base_url: 'https://ark.cn-beijing.volces.com/api/v3',
        homepage: 'https://www.volcengine.com/docs/82379',
        usage_preset: null,
        relay_protocol: 'chat_completions',
        // ⚠️ 火山方舟的 model id 是用户在控制台创建 endpoint 时分配的 ep-xxx，
        // 这里只给一个占位；用户必须改成自己 endpoint 的实际 id。
        model_fallback: 'doubao-seed-1-6-thinking',
        description: '火山方舟 (Coding Plan / Agent Plan / 按量付费)；model 字段需用户填实际 endpoint id',
        mark: '火', color: '#DC2626', group: 'CODING PLAN', auth_prefix: 'sk-',
        category: 'coding_plan',
    },
    {
        id: 'tencent_hunyuan',
        name: '腾讯',
        base_url: 'https://api.hunyuan.cloud.tencent.com/v1',
        homepage: 'https://cloud.tencent.com/document/product/1729',
        usage_preset: null,
        relay_protocol: 'chat_completions',
        model_fallback: 'hunyuan-turbos-latest',
        model_map: {
            'gpt-5.5': 'hunyuan-turbos-latest',
            'gpt-5': 'hunyuan-turbos-latest',
            'gpt-5-codex': 'hunyuan-code',
            'gpt-4o': 'hunyuan-turbos-latest',
            'gpt-4o-mini': 'hunyuan-lite',
            'o1': 'hunyuan-t1-latest',
            'o1-mini': 'hunyuan-t1-latest',
        },
        description: '腾讯混元 (Token Plan / TokenHub 按量)；OpenAI Chat 兼容',
        mark: '混', color: '#0EA5E9', group: '三方模型', auth_prefix: 'sk-',
        category: 'third_party',
    },
    {
        id: 'baidu_qianfan',
        name: '百度',
        base_url: 'https://qianfan.baidubce.com/v2',
        homepage: 'https://cloud.baidu.com/doc/WENXINWORKSHOP/index.html',
        usage_preset: null,
        relay_protocol: 'chat_completions',
        model_fallback: 'ernie-4.5-turbo-128k',
        model_map: {
            'gpt-5.5': 'ernie-4.5-turbo-128k',
            'gpt-5': 'ernie-4.5-turbo-128k',
            'gpt-5-codex': 'ernie-4.5-turbo-128k',
            'gpt-4o': 'ernie-4.5-turbo-128k',
            'gpt-4o-mini': 'ernie-speed-128k',
            'o1': 'ernie-x1-turbo-32k',
            'o1-mini': 'ernie-x1-turbo-32k',
        },
        description: '百度千帆 / 文心一言 ERNIE（按量付费 / 中国订阅）',
        mark: '千', color: '#3B82F6', group: '三方模型', auth_prefix: 'bce-',
        category: 'third_party',
    },
    {
        id: 'ucloud_modelverse',
        name: '优刻得（UCloud）',
        base_url: 'https://deepseek.uk-tokyo.ucloud-global.com/v1',
        homepage: 'https://www.ucloud.cn/site/active/modelverse.html',
        usage_preset: null,
        relay_protocol: 'chat_completions',
        model_fallback: 'glm-4.7',
        description: 'UCloud Modelverse (Coding Plan + 按量付费 国内/海外)；多模型聚合',
        mark: 'U', color: '#0066FF', group: 'CODING PLAN', auth_prefix: 'sk-',
        category: 'coding_plan',
    },
    {
        id: 'fireworks_ai',
        name: 'Fireworks AI',
        base_url: 'https://api.fireworks.ai/inference/v1',
        homepage: 'https://docs.fireworks.ai/',
        usage_preset: null,
        relay_protocol: 'chat_completions',
        // Fireworks 的模型 id 形如 accounts/fireworks/models/glm-4p6
        model_fallback: 'accounts/fireworks/models/glm-4p6',
        model_map: {
            'gpt-5.5': 'accounts/fireworks/models/glm-4p6',
            'gpt-5': 'accounts/fireworks/models/glm-4p6',
            'gpt-5-codex': 'accounts/fireworks/models/qwen3-coder-480b-a35b-instruct',
            'gpt-4o': 'accounts/fireworks/models/llama-v3p3-70b-instruct',
            'gpt-4o-mini': 'accounts/fireworks/models/llama-v3p1-8b-instruct',
        },
        description: 'Fireworks AI 海外高速推理（按量 / Fire Pass 国际订阅）；OpenAI Chat 兼容',
        mark: 'FW', color: '#7C3AED', group: '三方模型', auth_prefix: 'fw-',
        category: 'third_party',
    },
    {
        id: 'stepfun_step',
        name: '阶跃星辰',
        base_url: 'https://api.stepfun.com/v1',
        homepage: 'https://platform.stepfun.com/docs/',
        usage_preset: null,
        relay_protocol: 'chat_completions',
        model_fallback: 'step-3',
        model_map: {
            'gpt-5.5': 'step-3',
            'gpt-5': 'step-3',
            'gpt-5-codex': 'step-3',
            'gpt-4o': 'step-2-16k',
            'gpt-4o-mini': 'step-1-flash',
            'o1': 'step-r-mini',
            'o1-mini': 'step-r-mini',
        },
        description: '阶跃星辰 step 系列 (按量付费 / 中国订阅 / 国际订阅)',
        mark: 'St', color: '#0F766E', group: '三方模型', auth_prefix: 'sk-',
        category: 'third_party',
    },
    {
        id: 'openrouter',
        name: 'OpenRouter',
        base_url: 'https://openrouter.ai/api/v1',
        homepage: 'https://openrouter.ai/docs',
        usage_preset: null,
        relay_protocol: 'chat_completions',
        // OpenRouter 用 vendor/model 形式；不设兜底，让用户自己选
        model_fallback: 'openai/gpt-5.5',
        description: '多模型聚合 API；可配置厂商模型 ID',
        mark: 'OR', color: '#10B981', group: '三方模型', auth_prefix: 'sk-or-',
        category: 'third_party',
    },
    {
        id: 'aiberm',
        name: 'Aiberm',
        // Aiberm 同时支持 OpenAI /v1/chat/completions 和 Anthropic /v1/messages，
        // cc-router 走 Anthropic 路径，我们走 OpenAI 路径。
        // 实际可用模型清单由 key 所属 token group 决定，需用 /v1/models 探测。
        base_url: 'https://aiberm.com/v1',
        homepage: 'https://aiberm.com',
        usage_preset: null,
        relay_protocol: 'chat_completions',
        // 文档示例只给了 gpt-4 / gpt-3.5-turbo / claude-3-sonnet，
        // 实际模型按 key 等级动态返回 —— 这里用 gpt-4o 作 fallback
        model_fallback: 'gpt-4o',
        description: 'Aiberm 全球聚合 API（OpenAI/Anthropic 双协议，模型按 token group 动态）',
        mark: 'Ai', color: '#0EA5E9', group: '三方模型', auth_prefix: 'sk-',
        category: 'third_party',
    },
    {
        id: 'whatai',
        name: '神马中转 API (Whatai)',
        // Whatai 明确支持 OpenAI + Anthropic 双协议，cc-router 用 Anthropic，
        // 我们用 OpenAI Chat Completions 路径。
        base_url: 'https://api.whatai.cc/v1',
        homepage: 'https://api.whatai.cc',
        usage_preset: null,
        relay_protocol: 'chat_completions',
        model_fallback: 'chatgpt-4o-latest',
        model_map: {
            'gpt-5.5': 'chatgpt-4o-latest',
            'gpt-5': 'chatgpt-4o-latest',
            'gpt-5-codex': 'chatgpt-4o-latest',
            'gpt-4o': 'gpt-4o',
            'gpt-4o-mini': 'gpt-4o-mini',
        },
        description: '神马中转 API 全球按量付费聚合（OpenAI/Anthropic 双协议）',
        mark: '神', color: '#F59E0B', group: '三方模型', auth_prefix: 'sk-',
        category: 'third_party',
    },
    {
        id: 'modelscope',
        name: '魔搭（阿里云）',
        base_url: 'https://api-inference.modelscope.cn/v1',
        homepage: 'https://modelscope.cn/docs/model-service/api-inference/intro',
        usage_preset: null,
        relay_protocol: 'chat_completions',
        // ModelScope 用 vendor/model 形式，比如 Qwen/Qwen3-Max
        model_fallback: 'Qwen/Qwen3-Max',
        model_map: {
            'gpt-5.5': 'Qwen/Qwen3-Max',
            'gpt-5': 'Qwen/Qwen3-Max',
            'gpt-5-codex': 'Qwen/Qwen3-Coder-480B-A35B-Instruct',
            'gpt-4o': 'Qwen/Qwen3-Max',
            'gpt-4o-mini': 'Qwen/Qwen3-Next-80B-A3B-Instruct',
            'o1': 'Qwen/Qwen3-Max',
            'o1-mini': 'Qwen/Qwen3-Next-80B-A3B-Thinking',
        },
        description: '阿里魔搭 ModelScope 按量付费；OpenAI Chat 兼容（Qwen 系列为主）',
        mark: '魔', color: '#624AFF', group: '三方模型', auth_prefix: 'ms-',
        category: 'third_party',
    },
    {
        id: 'ollama_local',
        name: 'Ollama 本地推理',
        base_url: 'http://localhost:11434/v1',
        homepage: 'https://github.com/ollama/ollama/blob/main/docs/openai.md',
        usage_preset: null,
        relay_protocol: 'chat_completions',
        // Ollama 0.1.30+ 暴露 OpenAI 兼容 /v1/chat/completions；模型名按本地 pulled 的 tag
        model_fallback: 'qwen3:latest',
        description: 'Ollama 本地推理（localhost:11434，需先 ollama pull 模型）；API Key 任意填一个非空字符串即可',
        mark: 'O', color: '#000000', group: '三方模型', auth_prefix: 'ollama',
        category: 'third_party',
    },
    {
        id: 'custom',
        name: '自定义中转站',
        base_url: '',
        usage_preset: 'auto',
        description: '手动填 base_url；余额自动探测',
        mark: '+', color: '#64748B', group: '自定义', auth_prefix: 'sk-',
        category: 'aggregator',
    },
];
