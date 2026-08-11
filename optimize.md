# ITL / TPOT 下沉到 agentgateway：实现方案与修改细节

> 基线版本：**v1.4.1**（`163ea214`，2026-07-29）。
> 当前工作区 HEAD（`6a2d02db`，2026-05-15）落后两个半月且代码已重构，**不要基于 HEAD 实现**。
> 前置结论见 `a.md`（死结论证）、`b.md`（方案对比）、`c.md`（v1.4.1 版本核查）。
> 本文所有路径/行号均已在 v1.4.1 上核实。

---

## 0. 目标与总体思路

**目标**：EPP 只做选端点，ITL / TPOT / TTFT 全部由 agentgateway 在数据面产出。

**两个改动 + 一个卸载**：

| 阶段 | 内容 | 产出 |
|---|---|---|
| 改动一 | 打通 `Backend::Service` + `inferenceRouting` 的响应体解析 | TTFT / TPOT / token usage 立即可用 |
| 改动二 | 新增 ITL 指标 | ITL 直方图 |
| 卸载 | EPP 侧关闭 response body 订阅、删除 ITL 计算 | **性能收益主体** |

改动一是前提：不打通解析，改动二的 hook 根本不会被执行。

---

## 1. v1.4.1 关键架构变化（影响实现方式）

相比 `b.md` 所依据的旧版，v1.4.1 有三处结构性变化，直接决定了改法：

### 1.1 LLM 代码**部分**抽为独立 crate（注意是部分）

拆分是按职责切的，两边都有代码，实现时容易找错文件：

| 内容 | 位置 |
|---|---|
| 数据类型：`LLMResponse` | `crates/llm/src/lib.rs:235`（`first_token` 在 `:271`） |
| 流式转换：`passthrough_stream` | `crates/llm/src/conversion/completions.rs:1147` |
| SSE 解析：`json_passthrough` / `PassthroughBody` | `crates/llm/src/parse/{sse,passthrough}.rs` |
| **provider 行为**：`AIProvider` 枚举、`setup_request`、`set_default_authority` | **`crates/agentgateway/src/llm/mod.rs`**（`:998`、`:1207`） |
| **各 provider 实现**（含 `custom`） | **`crates/agentgateway/src/llm/`** |

简言之：**数据与解析在 `crates/llm/`，provider 行为仍在 `crates/agentgateway/src/llm/`。**
`c.md` §1.1 的"已抽为独立 crate"表述过于笼统，以本表为准。

### 1.2 时序计算已集中到一个方法（重要）

旧版在 `log.rs` 里内联计算 TTFT/TPOT；v1.4.1 抽出为
`crates/agentgateway/src/cel/types.rs:1553`：

```rust
pub fn set_token_timing(&mut self, request_start: Instant, response_end: Instant) {
    let Some(first_token) = self.first_token else { return; };
    self.time_to_first_token =
        chrono::Duration::from_std(first_token.duration_since(request_start))
            .ok().map(Into::into);
    if let Some(output_tokens) = self.output_tokens.filter(|t| *t > 0) {
        let first_to_last = response_end.duration_since(first_token);
        let time_per_output_token =
            Duration::from_secs_f64(first_to_last.as_secs_f64() / output_tokens as f64);
        self.time_per_output_token = chrono::Duration::from_std(time_per_output_token)
            .ok().map(Into::into);
    }
}
```

调用点两处：`proxy/httpproxy.rs:3059`、`telemetry/log.rs:1169`。

**这是 ITL 的最佳落点**——只要把 ITL 也做成 `LLMContext` 上的一个 `CelDuration` 字段，
指标发射和 CEL 暴露两件事一次做完，无需改动 `log.rs` 的发射结构。

### 1.3 指标发射已改为读 CelDuration

`telemetry/log.rs:822-841` 不再自己算时间差，而是读 `LLMContext` 上算好的字段：

```rust
if let Some(ttft) = llm_response.time_to_first_token
    .and_then(|duration| duration.0.to_std().ok())
{
    log.metrics.gen_ai_time_to_first_token.get_or_create(&gen_ai_labels)
        .observe(ttft.as_secs_f64());
}
if let Some(time_per_output_token) = llm_response.time_per_output_token
    .and_then(|duration| duration.0.to_std().ok())
{
    log.metrics.gen_ai_time_per_output_token.get_or_create(&gen_ai_labels)
        .observe(time_per_output_token.as_secs_f64());
}
```

ITL 照抄这个模式即可，改动高度对称。

### 1.4 流式日志句柄换成了 StreamingUsageGuard

旧版 `AmendOnDrop` + `non_atomic_mutate` → v1.4.1 `StreamingUsageGuard`
（`crates/llm/src/lib.rs:288`）+ `log.update(|r| ...)`。写法变化，语义相同。

---

## 2. 改动一：打通 service 后端的响应体解析

### 2.1 问题定位（v1.4.1 行号）

**总闸**（`crates/agentgateway/src/proxy/httpproxy.rs:2245`）：

```rust
if let Some(llm) = &backend_call.backend_policies.llm_provider {
```

`llm_provider` 只在 `Backend::AI` 分支赋值（`httpproxy.rs:2040`、`:2079`）。

**Service 分支**（`httpproxy.rs:2124`）走 `build_simple_backend_call`，不设置：

```rust
Backend::Service(svc, port) => {
    let simple = SimpleBackend::Service(svc.clone(), *port);
    build_simple_backend_call(
        &inputs, policy_client.clone(), &simple, policies,
        &mut req, &mut log, response_policies, hbone_source,
    ).await?
},
```

**约束**（`types/local.rs:2447`，`validate_inference_routing_scope`）：`inferenceRouting`
只允许挂 service 后端，挂 AI provider 直接 bail。三条 bail 在 `:2457`、`:2460`、`:2464`。

### 2.2 改法

在 `httpproxy.rs:2124` 的 `Backend::Service` 分支，`build_simple_backend_call` 返回后，
按条件补设 `llm_provider`：

```rust
Backend::Service(svc, port) => {
    let simple = SimpleBackend::Service(svc.clone(), *port);
    let mut call = build_simple_backend_call(
        &inputs, policy_client.clone(), &simple, policies,
        &mut req, &mut log, response_policies, hbone_source,
    ).await?;

    // 当 service 后端配了 AI policy 且开启指标时，挂一个 OpenAI 兼容 provider，
    // 仅用于响应体解析产出 LLM 指标；不改变已由 EPP 选定的连接目标。
    if call.backend_policies.llm_provider.is_none()
        && let Some(ai) = &call.backend_policies.llm
        && ai.metrics_enabled()
    {
        call.backend_policies.llm_provider = Some(observability_provider(ai));
    }
    call
},
```

**三条纪律（务必遵守）**：

1. **绝不调用 `default_connector()`**。它会把 target 改成 `api.openai.com:443` 并挂系统信任
   TLS，会覆盖 EPP 选好的端点。正确做法参照 `httpproxy.rs:2039-2042` 的
   `provider_defaults` 写法——只构造 `BackendPolicies { llm_provider: Some(...), .. }`，
   **不碰 target、不碰 backend_tls**。
2. **不动 `ServiceCallOverride` / EPP 选址结果**。`inference_result.destination` 与
   `destination_passthrough` 的行为必须原样保留。
3. **必须是显式开关**，不能因为配了 `ai` policy 就无条件解析——对非 LLM 流量是纯开销。

### 2.3 provider 构造：必须用 `Custom`，不能用 `OpenAI`

**这是本方案最容易出错、且后果最严重的一处。** 虽然 vLLM 是 OpenAI 兼容接口，但
**不能**用 `AIProvider::OpenAI`——它会覆盖 EPP 已经选定的端点。

#### 为什么 `OpenAI` 会破坏 EPP

设上 `llm_provider` 后，请求侧会走 `setup_request`
（`crates/agentgateway/src/llm/mod.rs:998`），它做三件事：

```rust
if let Some(path_override) = path_override { ... } else {
    self.set_default_path(req, route_type, llm_request, path_prefix, has_host_override)?;
}
if !has_host_override {
    self.set_default_authority(req, route_type)?;     // ← 危险
}
self.set_required_fields(req, route_type, llm_request)?;
```

`set_default_authority`（`llm/mod.rs:1207`）对各内建 provider 会**强行改写 URI authority**：

```rust
let authority = match self {
    AIProvider::OpenAI(_) => Authority::from_static(openai::DEFAULT_HOST_STR),
    ...
    AIProvider::Custom(_) => return Ok(()),        // ← 唯一不改的
};
http::modify_req(req, |req| {
    http::modify_uri(req, |uri| { uri.authority = Some(authority); ... })
});
```

用 `OpenAI` 且未设 `host_override` ⇒ authority 被改成 `api.openai.com`，EPP 通过
`x-gateway-destination-endpoint` 选出的端点被覆盖，请求发错地方。

#### 正确选择：`AIProvider::Custom`

`Custom` 是专为自建/兼容服务设计的 variant，在三个关键位置都**主动不改请求**：

| 位置 | 行为 |
|---|---|
| `llm/mod.rs:1219`（`set_default_authority`） | `return Ok(())`，不改 authority |
| `llm/mod.rs:977`（`default_connector`） | `return None`，不给默认连接策略 |
| `llm/mod.rs:994`（`default_connector_target`） | `return None`，不给默认 target |

`default_connector_target` 上方的注释是直接依据：

> Returns `None` for custom providers, which require an explicit host override or
> provider backend.

即 Custom provider 在设计上就把连接目标的决定权交给外部——**正是我们要的语义：连接由
EPP 决定，provider 只负责解析**。

另外 `llm/mod.rs:1084` 还有一处保护：

```rust
if has_host_override && path_prefix.is_none() && !matches!(self, AIProvider::Custom(_)) {
```

Custom 被显式排除在默认 path 改写之外，进一步降低干扰请求的风险。

#### 构造

```rust
fn observability_provider() -> Arc<llm::NamedAIProvider> {
    Arc::new(llm::NamedAIProvider {
        name: strng::literal!("observability"),
        // Custom：不改 authority、不改 path、不给默认连接目标。
        // formats 声明 OpenAI 兼容，使响应体按 completions 格式解析。
        provider: AIProvider::Custom(custom::Provider { /* formats: [Completions], model: None */ }),
        provider_backend: None,   // 不解析 provider 后端，连接目标完全由 EPP 决定
        host_override: None,
        path_override: None,
        path_prefix: None,
        tokenize: false,          // 不做请求侧本地分词（见 §5.5）
        inline_policies: Default::default(),
    })
}
```

`custom::Provider` 的字段以 v1.4.1 实际定义为准（`formats` 用于声明支持的协议格式，
见 `llm/mod.rs:825`、`:859` 的 `p.formats` 用法）。**实现时第一步就该确认 `Custom` +
`formats: [Completions]` 能正确路由到 `passthrough_stream`**（`llm/mod.rs:240` 的
`AIProvider::Custom(_) => match provider_format` 是入口）。

#### 必须验证的断言

改动一完成后，**在验证指标之前**先确认请求没被改坏（§6 阶段 A 第 5 步已覆盖，但这里是
具体断言）：

- 请求实际到达的后端 IP:Port == EPP 返回的 `x-gateway-destination-endpoint`
  （对比访问日志的 `inference_pool` 字段，`httpproxy.rs:1840` 写入）。
- 请求的 URI path 与 Host 头相对改动前**逐字节一致**。
- `destinationMode: passthrough` 行为未变。

任一条不成立，说明 provider 选择或 `setup_request` 有干扰，**必须先修好再继续**——
指标错了可以重算，请求发错端点是线上事故。

### 2.3.1 补充：v1.4.1 的 llm 模块并未完全迁走

`c.md` §1.1 说"LLM 代码已抽为独立 crate `crates/llm/`"，这个说法**不完整**，需修正：

- `crates/llm/`：类型定义、流式转换（`conversion/completions.rs` 等）、SSE 解析
- `crates/agentgateway/src/llm/`：**仍然存在**，含 `AIProvider` 枚举、`setup_request`
  （`mod.rs:998`）、`set_default_authority`（`:1207`）、各 provider 实现、`custom.rs`

即"provider 行为"留在 agentgateway crate，"数据类型与流式解析"移到了 llm crate。
§7 改动清单已按此修正。

### 2.4 配置面

新增开关，建议挂在 `ai` policy 下（`types/local.rs` 的 `llm::Policy`）：

```yaml
backends:
- service:
    name: default/my-model
    port: 8000
  policies:
    inferenceRouting:
      endpointPicker:
        host: 127.0.0.1:9002
      destinationMode: passthrough
    ai:
      metrics: true          # 新增：开启响应体解析以产出 LLM 指标
```

K8s 形态需同步：`controller/pkg/agentgateway/plugins/inference_plugin.go` 在生成
`InferenceRouting` policy 时，可顺带生成指向同一 service target 的 AI policy
（或由用户通过 `AgentgatewayPolicy` 显式配置）。

### 2.5 需要同步调整的测试

v1.4.1 为那三条 bail **新增了回归测试**，说明该约束是社区有意维持的：

- `crates/agentgateway/src/types/local_tests.rs:1334`
- `crates/agentgateway/src/types/local_tests.rs:1702`
- `crates/agentgateway/src/types/local_tests.rs:1728`

本改动不放宽 `validate_inference_routing_scope`（仍禁止把 `inferenceRouting` 挂到 AI
后端），走的是"给 service 后端补 provider"的旁路，因此**这三个测试应保持通过、不需修改**。
若实现中发现必须放宽校验，则属于设计走偏，需回退重新考虑。

---

## 3. 改动二：新增 ITL 指标

### 3.1 数据结构

**`crates/llm/src/lib.rs`**，`LLMResponse`（`:235`）中 `first_token`（`:271`）旁新增：

```rust
#[serde(skip)]
pub first_token: Option<Instant>,
/// 最近一个 SSE chunk 的到达时刻，用于计算相邻 chunk 间隔。
#[serde(skip)]
pub last_token: Option<Instant>,
/// 相邻 chunk 间隔累计与样本数（用于算平均 ITL）。
#[serde(skip)]
pub itl_sum: Option<Duration>,
#[serde(skip)]
pub itl_count: u64,
```

**精度取舍**：

- **方案 i（推荐，改动小）**：累计 sum/count，在 `set_token_timing` 里算出平均 ITL，
  作为一个 `CelDuration` 字段发射 → 得到"平均 ITL 的分布"直方图。语义清晰、与 TPOT 对称。
- **方案 ii（分布更精确）**：在流中每次算出间隔就直接 `observe` 进 histogram → 得到
  "所有 chunk 间隔的真实分布"。但需要把 metrics 引用传进 `crates/llm`，跨 crate 依赖，
  且 `StreamingUsageGuard` 当前只暴露 `update(|&mut LLMInfo|)`，拿不到 metrics 句柄。

**建议先做方案 i**，与现有 TPOT 架构完全对称、不引入跨 crate 依赖；若后续确实需要真实
分布，再单独评估方案 ii。

### 3.2 流式 hook

**`crates/llm/src/conversion/completions.rs:1213`**（`passthrough_stream` 内，vLLM 直通
必经路径），现状：

```rust
if !saw_token {
    saw_token = true;
    log.update(|r| {
        r.response.first_token = Some(Instant::now());
    });
}
```

**采用闭包本地累加，避免每 chunk 都调 `log.update`**（理由见 §3.2.1）。在闭包捕获变量处
（`saw_token` 声明旁）增加本地状态：

```rust
let mut last_token: Option<Instant> = None;
let mut itl_sum = Duration::ZERO;
let mut itl_count: u64 = 0u64;
```

chunk 处理改为纯本地运算，不触碰共享状态：

```rust
let now = Instant::now();
if !saw_token {
    saw_token = true;
    log.update(|r| {
        r.response.first_token = Some(now);
    });
} else if let Some(prev) = last_token {
    itl_sum += now.duration_since(prev);
    itl_count += 1;
}
last_token = Some(now);
```

在**已有的** usage 分支（`completions.rs:1226`，`if let Some(u) = f.usage`）内，以及
流结束分支（`None =>`，对应 `[DONE]`，见 `parse/sse.rs:22`）里一次性写回：

```rust
if itl_count > 0 {
    r.response.itl_sum = Some(itl_sum);
    r.response.itl_count = itl_count;
}
```

注意两点：`Instant::now()` 每 chunk 只取一次并复用，避免同一 chunk 内两次读时钟造成偏差；
写回必须同时放在 usage 分支和 `[DONE]` 分支，因为**没有 `include_usage` 时不会有 usage
chunk**（§5.1），只靠 usage 分支会导致 ITL 丢失——而 ITL 恰恰是不依赖 usage 的那个指标。

### 3.2.1 为什么不在每 chunk 调 `log.update`

`StreamingUsageGuard::update`（`crates/llm/src/lib.rs:296`）经
`Box<dyn StreamingUsageReporter>` 动态派发到共享的 `LLMInfo`：

```rust
pub fn update(&self, mut f: impl FnMut(&mut LLMInfo)) {
    self.reporter.update(&mut f);
}
```

每次调用含一次虚调用加一次共享状态写入（具体同步原语取决于 reporter 实现）。当前代码在
流中只有首 chunk（`saw_token`）、首次见到 model（`seen_provider`）、usage chunk 三个时机
调用它，**稳态下每 chunk 零调用**。若 ITL 改成每 chunk 都调，会把它从"每请求 3 次"变成
"每 token 1 次"，在高并发下是可观测的新增开销。

本地累加把这一点消除：稳态每 chunk 只做一次 `Instant::now()` 加一次整数/Duration 加法，
写回仍是每请求 1 次。这也是 §5.8 压测要验证的点。

**其余 provider 的对称点**（vLLM 场景不触发，建议一并处理以保持语义一致）：

- `crates/llm/src/conversion/messages.rs:833`（`passthrough_stream`）
- `crates/llm/src/conversion/responses.rs:22`（`passthrough_stream`）
- `crates/llm/src/types/detect.rs:625`（`passthrough_stream`）
- 各协议转换路径中设置 `first_token` 的位置（用 `git grep -n "first_token" v1.4.1 -- 'crates/llm/*'` 定位）

**`LLMResponse` 构造点**需同步补字段（新增字段若都是 `Option`/`u64` 且有 `Default`，
使用 `..Default::default()` 的构造点无需改）。用
`git grep -n "first_token:" v1.4.1 -- '*.rs'` 核对，已知包括
`crates/agentgateway/src/parse/websocket.rs:77`、`:551`、`cel/types.rs:1596`、`:2303`。

### 3.3 CEL 字段与计算

**`crates/agentgateway/src/cel/types.rs`**，在 `time_per_output_token`（`:1478`）旁新增：

```rust
/// Average interval between consecutive streamed response chunks.
#[dynamic(rename = "interTokenLatency")]
#[serde(skip_serializing_if = "Option::is_none")]
#[cfg_attr(feature = "schema", schemars(with = "Option<String>"))]
pub inter_token_latency: Option<CelDuration>,
```

同时在 `From<llm::LLMInfo>`（`:1519` 附近）里透传 `itl_sum` / `itl_count`
（作为 `#[dynamic(skip)] #[serde(skip)]` 的内部字段，与 `first_token` 同处理方式）。

在 `set_token_timing`（`:1553`）末尾追加：

```rust
if self.itl_count > 0 && let Some(sum) = self.itl_sum {
    let avg = sum / self.itl_count as u32;
    self.inter_token_latency = chrono::Duration::from_std(avg).ok().map(Into::into);
}
```

放在这里的好处：两个调用点（`httpproxy.rs:3059`、`log.rs:1169`）自动覆盖，无需改调用方。

### 3.4 指标注册与发射

**注册**，`crates/agentgateway/src/telemetry/metrics.rs`：

- 结构体字段：在 `gen_ai_time_to_first_token`（`:209`）旁加
  `pub gen_ai_inter_token_latency: Histogram<GenAILabels>,`
- 注册（照 `:343-349` 的 TTFT 写法）：

```rust
let gen_ai_inter_token_latency = Family::<GenAILabels, _>::new_with_constructor(move || {
    PromHistogram::new(OUTPUT_TOKEN_BUCKET)     // :561，1ms–2.5s，正合 ITL 量级
});
registry.register(
    "gen_ai_server_inter_token_latency",
    "Average interval between consecutive streamed response chunks",
    gen_ai_inter_token_latency.clone(),
);
```

标签沿用 `GenAILabels`（`:92`），与 TTFT/TPOT 同维度，便于同面板下钻。

**发射**，`crates/agentgateway/src/telemetry/log.rs`，紧跟 `time_per_output_token`
（`:832-841`）之后照抄：

```rust
if let Some(itl) = llm_response.inter_token_latency
    .and_then(|duration| duration.0.to_std().ok())
{
    log.metrics.gen_ai_inter_token_latency.get_or_create(&gen_ai_labels)
        .observe(itl.as_secs_f64());
}
```

---

## 4. 卸载 EPP（性能收益主体）

**这一步不做，前两步的性能收益就是零。**

`InferenceRouting::build`（`crates/agentgateway/src/http/ext_proc.rs:188-197`）把 EPP 的
body 模式**硬编码**为 `FullDuplexStreamed`，不读用户配置：

```rust
ProcessingOptions {
    request_body_mode: BodySendMode::FullDuplexStreamed,
    response_body_mode: BodySendMode::FullDuplexStreamed,
    ...
}
```

因此 gateway 会一直把每个响应 chunk 泵给 EPP。要真正卸载，两条路：

1. **EPP 侧改**（推荐）：在 EPP 的 processing mode 声明里放弃 response body，
   并删除其中的 ITL 计算逻辑。EPP 主动不要 body，gateway 的泵送即失去意义。
2. **gateway 侧改**：让 `InferenceRouting::build` 的 `response_body_mode` 可配置
   （v1.4.1 已有 `ProcessingOptions` 基础设施，`ExtProc` 也已暴露该字段，
   见 `ext_proc.rs:270`），EPP 路径改为默认 `None`。这是更彻底的修法，但涉及 GIE
   兼容性判断（EPP 是否依赖 response 阶段做别的事，例如 KV-cache 状态回写）。

**上线前必须确认**：EPP 是否在 response 阶段做了选端点之外的事（如指标上报、
负载状态更新）。若有，只能删 ITL、保留其余，不能整体关闭 body 订阅。

---

## 5. 落地陷阱（会导致"改完没数据"）

### 5.1 `include_usage` —— 最高优先级

`passthrough_stream` 中 usage 只在上游发送时才记录
（`crates/llm/src/conversion/completions.rs:1226`，`if let Some(u) = f.usage`）。
vLLM 默认**不发** usage，需客户端请求带 `stream_options: {"include_usage": true}`。

链条：无 usage → `output_tokens = None` → `set_token_timing` 里
`self.output_tokens.filter(|t| *t > 0)` 落空 → **TPOT 无数据**（`cel/types.rs:1561`）。

**ITL 不受影响**（只依赖 chunk 到达时刻，不依赖 token 数）——这是 ITL 相比 TPOT 的
一个实际优势。

两个处理选项：

- 在改动一的请求方向对 OpenAI 直通强制注入 `stream_options.include_usage: true`。
  客户端会多收到一个 usage 末尾 chunk，主流 SDK 兼容，但属行为变更，需在文档注明。
- 不改请求，要求客户端自带。

### 5.2 chunk 间隔 ≠ 严格 token 间隔

gateway 只能看到 SSE chunk 边界，一个 chunk 可能含多个 token（vLLM 批处理 / TCP 聚合）。
指标名与文档必须标明是 **chunk 间隔**，否则与 EPP 旧数据、vLLM 原生直方图三方对比时
会出现无法解释的偏差。

### 5.3 TTFT 起算点

`set_token_timing(log.start.as_instant(), ...)`（`httpproxy.rs:3059`）——从 gateway
**收到客户端请求**起算，含排队与策略执行耗时；vLLM 自报从引擎收到请求起算。两者差一个
固定偏移，跨系统对比时需校准。

### 5.4 非流式请求无 ITL / TPOT

`first_token` 仅在流式路径设置；非流式请求只有 request_duration 与 token usage。
做 SLO 时注意分母口径。

### 5.5 本地 tokenize 不能兜底 output

`crates/llm` 的 tokenize 只算 **input_tokens**（用于精确限流），且非 OpenAI tokenizer
一律回落 `Cl100kBase`，对 Qwen/Llama 等开源模型有系统性偏差。§2.3 中设 `tokenize: false`
正是因此——不要试图用它替代上游 usage。

### 5.6 错误响应不进流式路径

非 2xx 走 buffered 路径，不产生流式指标。监控上 ITL/TPOT 曲线断点可能与错误率尖峰同时
出现，属预期，不是指标丢失。

### 5.7 指标基数

打通解析后 `gen_ai_client_token_usage` 会展开 input / output / input_cache_read /
input_cache_write 四组标签。若只用延迟指标，可用 `FilteredRegistry`
（`metrics.rs`，配置 `removes`）裁掉以控制基数。

### 5.8 解析开销：增加 CPU，不增加延迟

「打通响应体解析」本身的开销必须说清楚，它**不是零**，但性质与 ext-proc 完全不同。

**字节是原样直通的。** `crates/llm/src/parse/passthrough.rs:88-93`：

```rust
Some(Ok(frame)) => {
    if let Some(data) = frame.data_ref() {
        this.decode_buffer.extend_from_slice(data);
    }
    Some(Ok(frame))     // ← 原始 frame 原样返回给下游
},
```

原始 frame 直接下发，解析走旁路（复制一份到 `decode_buffer` 再解码）。frame 转发与解析在
**同一次 `poll_frame` 内**完成，不引入额外 await 点、channel 跳转或等待。

对比同目录下两条路，差别很清楚：

| 路径 | 机制 | 是否在关键路径 |
|---|---|---|
| `json_passthrough`（`sse.rs:11`）——**vLLM 直通走这条** | 旁路窥探，原 frame 直发 | 否 |
| `json_transform`（`sse.rs:51`）——协议转换（Anthropic↔OpenAI）走这条 | `serde_json::to_vec` + `encode_sse_event` 重新编码 | 是 |
| ext-proc（`ext_proc/mutation.rs:276`） | frame 交出去、等回包才能下发 | 是，且跨进程 |

**实际成本（每 chunk）**：

1. `extend_from_slice` 一次内存拷贝——SSE chunk 通常几十到几百字节，可忽略。
2. `serde_json::from_slice::<StreamResponse>`（`sse.rs:26`）——主要成本。
   `StreamResponse`（`types/completions.rs:643`）8 个字段，嵌套
   `Vec<ChatChoiceStream>` → `StreamResponseDelta`（含 `Option<Vec<ToolCallChunk>>`）。
   典型 delta chunk 反序列化在**微秒级**。
3. `log.update` 共享状态写入——按 §3.2 的本地累加实现后，稳态每 chunk **零次**。

以 50 tokens/s 的单流估算：每秒约 50 次微秒级解析，单请求 CPU 占用约 0.005%。与 ext-proc
的每 chunk 两次 gRPC 往返（百微秒至毫秒级，含序列化、内核态切换、网络栈）**相差约三个
数量级**。

**两点补充**：

- 这不是新增一条解析，而是让**已存在**的 `passthrough_stream` 代码路径生效。走 AI backend
  而非 EPP 的流量现在就在付这个成本。
- 结论是"增加 CPU、不增加延迟"，但**须经压测证实**，不要凭代码推断收工。压测项见 §6 阶段 A
  第 6 步。

---

## 6. 实施顺序与验证

### 阶段 A：改动一（打通解析）

1. 切基线：`git checkout v1.4.1 -b feat/gateway-llm-metrics`
   （`a.md`/`b.md`/`c.md`/`optimize.md` 均为未跟踪文件，不受影响）
2. 实施 §2，`cargo build` 通过。
3. 起 vLLM 或 OpenAI 兼容 mock，按 §2.4 配 service 后端 + `inferenceRouting`
   （可直接改 `examples/standalone-epp/config.yaml`）。
4. 发**带 `include_usage`** 的流式请求，抓 `/metrics` 确认
   `gen_ai_server_time_to_first_token`、`gen_ai_server_time_per_output_token` 有值。
5. 回归：确认 `local_tests.rs` 的三个 inferenceRouting 测试仍通过（§2.5）；
   确认 EPP 选址与 `destinationMode: passthrough` 行为未变（对比 `inference_pool`
   访问日志字段）。
6. **压测验证解析开销**（对应 §5.8，不要跳过——这是"增加 CPU 不增加延迟"这一论断的
   实证环节）。同一 mock、同一并发下跑两组：
   - 对照组：`ai.metrics: false`（不解析，等价改动前行为）
   - 实验组：`ai.metrics: true`（解析开启）

   观测三项：
   - **端到端 chunk 间隔 p99**：预期无显著变化（解析在旁路）。若显著上升，说明
     `poll_frame` 里出现了预期外的阻塞，需回查。
   - **gateway 进程 CPU**：预期小幅上升（微秒级解析 × chunk 数），量化实际百分比。
   - **首字节延迟 TTFB**：预期无变化。

   建议并发量取生产峰值的 1～2 倍，且 mock 的 chunk 大小/频率贴近真实模型输出
   （小 chunk 高频最能放大 per-chunk 开销）。

### 阶段 B：改动二（ITL）

6. 实施 §3，`cargo build` + `cargo test` 通过。
7. 用**可控 chunk 间隔**的 mock（如每 chunk 固定 sleep 50ms）验证
   `gen_ai_server_inter_token_latency` 的观测值与注入间隔吻合。
8. 发**不带 `include_usage`** 的请求，确认 TTFT/ITL 仍在、TPOT 为空——验证对 §5.1 的理解。

### 阶段 C：卸载 EPP 与收益量化

9. 按 §4 关闭 EPP 的 response body 订阅、删除其 ITL 逻辑。
10. 压测对比两组数据：
    - 基线：EPP 保留 ITL 计算
    - 目标：EPP 卸载 + gateway 统计
    量化 **EPP CPU 占用**与**端到端 token 间隔抖动（p99）**两项收益。
11. 与 vLLM 原生 `vllm:time_per_output_token_seconds` 交叉核对 p50/p99，记录
    gateway↔engine 视角偏移量，写入监控文档。

### 环境说明

当前集群没有 llm-d/EPP/vLLM 部署（`inferencepool` CRD 不存在、`polaris` 命名空间为空），
上述验证需在真实 llm-d 环境或本地 mock 中进行。建议先用 mock 完成阶段 A/B
（可精确控制 chunk 间隔，便于验证指标正确性），再到真实环境做阶段 C。

---

## 7. 改动清单速查

| 文件 | 位置 | 改动 |
|---|---|---|
| `crates/agentgateway/src/proxy/httpproxy.rs` | `:2124` Service 分支 | 条件补设 `llm_provider`（用 `AIProvider::Custom`，见 §2.3） |
| `crates/agentgateway/src/llm/mod.rs` | `:998` `setup_request`、`:1207` `set_default_authority` | **只读不改**，用于确认 Custom 不改写请求 |
| `crates/agentgateway/src/types/local.rs` | `llm::Policy` | 新增 `metrics` 开关 |
| `crates/llm/src/lib.rs` | `:271` 附近 | 新增 `last_token` / `itl_sum` / `itl_count` |
| `crates/llm/src/conversion/completions.rs` | `:1213` | `saw_token` 块改为记录 chunk 间隔 |
| `crates/llm/src/conversion/{messages,responses}.rs`、`types/detect.rs` | 各 `passthrough_stream` | 对称改动 |
| `crates/agentgateway/src/cel/types.rs` | `:1478` / `:1553` | 新增 `inter_token_latency` 字段与计算 |
| `crates/agentgateway/src/telemetry/metrics.rs` | `:209` / `:343` | 注册 `gen_ai_server_inter_token_latency` |
| `crates/agentgateway/src/telemetry/log.rs` | `:841` 后 | 发射 ITL |
| `crates/agentgateway/src/http/ext_proc.rs` | `:188` | （可选）EPP body 模式可配置 |
| `controller/pkg/agentgateway/plugins/inference_plugin.go` | `:71` | （K8s 形态）同步生成 AI policy |

**预估**：改动一约 30 行；改动二约 60 行（含 4 处对称 hook）；EPP 侧视其实现而定。
