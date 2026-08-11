# ITL / TPOT 统计从 EPP 下沉到 agentgateway：全面分析（第二轮）

> 本文是 `a.md` 之后的第二轮分析。覆盖范围更广：新增 controller（K8s Gateway API / llm-d
> 官方部署形态）路径的实证、流式解析的实际执行路径（`passthrough_stream`）、usage 数据的
> 隐性前置条件、以及包含"不改 agentgateway"选项在内的方案全家福对比。
> 结论与 a.md 一致且有强化，落地陷阱清单是新增重点。

---

## 1. 场景与问题

- 自建模型服务：llm-d + vLLM，agentgateway 作为 Gateway 数据面。
- EPP（Endpoint Picker）通过 ext-proc 参与请求侧选端点。
- ITL 目前在 EPP 内计算，EPP CPU 占用过高、转发性能受损。
- 目标：把 ITL/TPOT 统计移到 agentgateway，EPP 回归"只做选端点"。

**TL;DR**：

1. EPP 算 ITL 的贵是 ext-proc 协议形态决定的，无法通过优化 EPP 内部实现解决——必须挪走。
2. agentgateway 的 TTFT、TPOT 指标**代码早已就绪**（`gen_ai_server_time_to_first_token` /
   `gen_ai_server_time_per_output_token`），ITL 是三者中唯一需要新写的。
3. 但当前代码存在结构性死结：**只要流量经 EPP 选端点（`inferenceRouting` + InferencePool/
   service 后端），agentgateway 就不解析响应体，三个指标全部落空**。local 配置和 K8s CRD
   两种部署形态走的是同一条死路（见第 4 节）。
4. 因此落地 = 一步小改动打通解析（TTFT/TPOT 白拿）+ 一步对称改动补 ITL。
5. 若只要"先看到数据"：vLLM 原生 Prometheus 指标零改动即可用（方案 A），建议与 B 组合。

---

## 2. 为什么 EPP 算 ITL 注定贵：ext-proc 协议代价

ITL 需要每个 SSE chunk 的到达时刻。在 ext-proc 下，这意味着 EPP 订阅 `ResponseBody`。
agentgateway 侧的实现证据：

**发送方向**（`crates/agentgateway/src/http/ext_proc.rs:546`，`handle_body_stream`）：

```rust
while let Some(Ok(frame)) = stream.next().await {
    let request = Some(if frame.is_data() {
        body_fn(HttpBody { body: frame.into(), end_of_stream: false })
    } ...);
    let Ok(()) = tx.send(ProcessingRequest { ... }).await else { return };
}
```

每个 data frame = 一条独立 `ProcessingRequest` gRPC 消息。

**接收方向**（`ext_proc.rs:666-713`，`mutate_response`）：

- body 先由独立 task 泵给 EPP（`ext_proc.rs:670`，`tokio::task::spawn(Self::handle_body_stream(...))`）；
- 返回体另建 `mpsc::channel(1)`（`ext_proc.rs:680`），再 spawn 一个 task 把 EPP 逐 chunk
  回吐的 body 转发给下游（`ext_proc.rs:705`）。

**累计成本（按 N 个 token 计）**：

| 项目 | 数量级 |
|---|---|
| gRPC 消息（gateway→EPP） | N |
| gRPC 消息（EPP→gateway） | N |
| protobuf 编解码 | 2N 次 |
| body 字节逐 chunk 拷贝 | 2N 次 |
| 额外 tokio task | 每请求 2 个 |

一次 1000 token 的响应 ≈ 2000 次跨进程消息。**且 EPP 在数据通路正中间，其处理延迟直接注入
用户感知的 token 间隔**——观测行为污染被观测指标。在 EPP 内部做任何优化（批处理、异步、降
precision）都只能缓解、不能消除，因为协议本身要求逐 chunk 过 EPP。

对比 agentgateway 侧：它为了 token 计数和 prompt guard **本就在解析这条 SSE 流**，补 ITL
的边际成本是每 chunk 一次 `Instant::now()` + 一次进程内 histogram 观测，无序列化、无 IPC。
成本差距约三个数量级。

---

## 3. agentgateway 现有能力：精确盘点

### 3.1 指标注册现状（`crates/agentgateway/src/telemetry/metrics.rs`）

| 指标 | 位置 | 语义 | 现状 |
|---|---|---|---|
| `gen_ai_server_time_to_first_token` | `metrics.rs:266` | TTFT，`log.rs:556`：首 chunk 时刻 − 请求到达时刻 | ✅ 已有 |
| `gen_ai_server_time_per_output_token` | `metrics.rs:257` | TPOT，`log.rs:566`：(末 chunk − 首 chunk) / output_tokens | ✅ 已有 |
| `gen_ai_server_request_duration` | `metrics.rs:247` | 全请求时长 | ✅ 已有 |
| `gen_ai_client_token_usage` | `metrics.rs:237` | input/output/cache token 计数直方图 | ✅ 已有 |
| ITL | — | 相邻 chunk 间隔分布 | ❌ **没有** |

ITL 缺失的根因：`LLMResponse`（`crates/agentgateway/src/llm/mod.rs:278`）只保存
`first_token: Option<Instant>` 一个流式时间戳，后续 chunk 到达时刻全部丢弃。

### 3.2 流式响应的实际执行路径：`passthrough_stream`

vLLM（OpenAI 兼容）在 agentgateway 里走 openai provider，响应处理在
`crates/agentgateway/src/llm/conversion/completions.rs:936` 的 `passthrough_stream`：

```rust
parse::sse::json_passthrough::<types::completions::typed::StreamResponse>(
    b, buffer_limit, move |f| { match f {
        Some(Ok(f)) => {
            if !saw_token {
                saw_token = true;
                log.non_atomic_mutate(|r| { r.response.first_token = Some(Instant::now()); });
            }
            ...
            if let Some(u) = f.usage {
                log.non_atomic_mutate(|r| {
                    r.response.output_tokens = Some(u.completion_tokens as u64);
                    ...
                });
                log.report_rate_limit();
            }
        },
```

三个要点：

1. **字节原样转发**：`json_passthrough` 逐 chunk 解析但不改写内容，只在闭包里偷记时间戳和
   usage。性能开销很低——这正是"在 gateway 算 ITL 几乎免费"的直接证据。
2. **`first_token` 记的是"第一个 SSE chunk 到达 gateway 的时刻"**，即 gateway 视角的 TTFT
   终点。ITL 的 hook 点就在这里（第 6.2 节）。
3. **usage 完全依赖上游主动发送**。`if let Some(u) = f.usage`——如果最后一个 chunk 没有
   usage，`output_tokens` 保持 `None`。这引出 3.3 的隐性前置条件。

### 3.3 隐性前置条件：`stream_options.include_usage`

vLLM（及 OpenAI API 语义）**默认不在流式响应里发 usage**，只有客户端请求里带
`stream_options: {"include_usage": true}` 时才会在末尾追加 usage chunk。

代码里唯一强制注入 include_usage 的地方（`completions.rs:902-908`）：

```rust
stream_options: if stream {
    Some(completions::StreamOptions {
        include_usage: Some(true),
        ...
```

属于 **messages → completions 协议转换**路径（客户端用 Anthropic 格式、上游是 OpenAI 格式
时才走）。openai→openai 直通时**用户请求原样转发，不会注入**。

结论贯穿链：`output_tokens = None` ⇒ `log.rs:521` 和 `log.rs:566` 的 `if let Some(ot)`
落空 ⇒ **token usage 指标和 TPOT 都没有数据**（ITL 不受影响，只看 chunk 到达时刻）。
落地时必须处理，详见第 7 节陷阱清单。

### 3.4 request 端的 tokenize 与 output 侧无关

`llm/mod.rs:96` 的 `tokenize` 选项用 tiktoken 在请求方向本地计算 `input_tokens`
（注释说明用途是精确限流），与 output token 统计无关。且 `llm/mod.rs:1497`：

```rust
let tokenizer = get_tokenizer(model).unwrap_or(Tokenizer::Cl100kBase);
if tokenizer != Tokenizer::Cl100kBase && tokenizer != Tokenizer::O200kBase { ... }
```

只支持 OpenAI 系 tokenizer，其余模型一律回落 `Cl100kBase`。对 vLLM 上的开源模型（Qwen、
Llama 等），本地估算与真实分词存在系统性偏差——**不要指望 tokenize 兜底 output 统计**。

---

## 4. 死结的完整证据链：两种部署形态，同一条死路

### 4.1 总闸：`llm_provider` 决定是否解析响应体

`crates/agentgateway/src/proxy/httpproxy.rs:1903`：

```rust
let mut resp = if let (Some(llm), Some(llm_request)) =
    (backend_call.backend_policies.llm_provider, llm_request)
{
    llm.provider.process_response(...).await?
} else {
    resp   // 原样透传，第 3 节的解析全部不发生
};
```

`llm_provider` 只在 `Backend::AI` 分支赋值（`httpproxy.rs:1540`、`1546`）。
`Backend::Service` 分支（`httpproxy.rs:1563` → `build_service_call`）不设置。

### 4.2 约束：`inferenceRouting` 只能挂 service 后端

`crates/agentgateway/src/types/local.rs:1484`，`validate_inference_routing_scope`：

```rust
InferenceRoutingScope::ServiceRouteBackend => Ok(()),
InferenceRoutingScope::NonServiceRouteBackend => {
    bail!("inferenceRouting is only supported on service route backends")
},
InferenceRoutingScope::AIProviderPolicies => {
    bail!("inferenceRouting is only supported on service route backends, not AI provider policies")
},
```

### 4.3 K8s / llm-d 形态实证：InferencePool → service policy

llm-d 官方部署用 Gateway API Inference Extension 的 `InferencePool` CRD。controller（Go）
把它翻译成 backend policy，`controller/pkg/agentgateway/plugins/inference_plugin.go:71-93`：

```go
inferencePolicy := &api.Policy{
    Target: &api.PolicyTarget{Kind: utils.ServiceTargetWithHostname(pool.Namespace, hostname, nil)},
    Kind: &api.Policy_Backend{
        Backend: &api.BackendPolicySpec{
            Kind: &api.BackendPolicySpec_InferenceRouting_{
                InferenceRouting: &api.BackendPolicySpec_InferenceRouting{
                    EndpointPicker: &api.BackendReference{ ... eppSvc, Port: eppPort ... },
                    FailureMode: failureMode,   // InferencePool spec.endpointPickerRef.failureMode
                },
            },
        },
    },
}
```

Rust 侧 `crates/agentgateway/src/types/agent_xds.rs:1350` 把这条 XDS policy 转成
`BackendTrafficPolicy::InferenceRouting`，注入 service 后端（`binds.rs:957`）。

**注意 target 是 Service**：和 local config 形态（`examples/standalone-epp/config.yaml`）
完全同构，最终都汇合到 4.1 的总闸——`llm_provider = None`，响应体不解析。

另外两个顺带发现：

- 同一插件还给 EPP 连接强制生成 TLS 策略（`inference_plugin.go:102-117`），
  `Verification: INSECURE_ALL`，注释原文 "The spec mandates this :vomit:"——GIE 规范要求
  gateway→EPP 用 TLS 但不验证。与你的问题无关，但审计时值得知道。
- `failureMode` 来自 InferencePool 的 `endpointPickerRef.failureMode`，默认 FAIL_CLOSED
  （EPP 挂了请求直接失败），可显式配 FAIL_OPEN。LLM 流量对延迟敏感时建议评估 FAIL_OPEN。

### 4.4 结论：纯配置无解

路由级 / 后端级 `ai` policy（`local.rs:2882` / `local.rs:1569`）都只填 `policies.llm`
（prompt 改写、限流、模型别名），**不填 `llm_provider`**。所以无论怎么写 YAML / CRD，
都不能在 EPP 模式下打开响应解析。必须改代码。

---

## 5. 方案全家福对比

| | 方案 A：vLLM 原生指标 | 方案 B：agentgateway 改造 | 方案 C：纯配置 agentgateway | 方案 D：独立 ext-proc observer |
|---|---|---|---|---|
| 改动量 | 零 | 两处代码（见第 6 节） | 零 | 新组件 + 部署 |
| TTFT | `vllm:time_to_first_token_seconds` ✅ | `gen_ai_server_time_to_first_token` | ❌ 死结 | ✅ |
| TPOT | `vllm:time_per_output_token_seconds` ✅ | `gen_ai_server_time_per_output_token` | ❌ | ✅ |
| ITL 分布 | ✅ 同上（vLLM 这个直方图本质就是 ITL 分布） | 需第 6.2 节新增 | ❌ | ✅ |
| 视角 | vLLM 进程内（不含网络） | gateway（含 vLLM→gateway 网络，更接近用户） | — | gateway |
| 维度标签 | pod 级，无路由/用户维度 | model/route/tenant 等 `GenAILabels` | — | 自选 |
| 对 EPP 的影响 | 无（但要自己去 EPP 删逻辑） | 无，可随即下掉 EPP 的 body 订阅 | 无 | EPP 可卸载，但 observer 继承同样的协议成本 |
| 观测开销 | 零（vLLM 已内置） | 每 chunk 一次 Instant + histogram | — | 与 EPP 同量级（见第 2 节） |

**方案 D 直接否决**：本质是把 EPP 的成本平移到一个新组件，协议代价分毫不减。

**推荐组合：A + B**。A 零成本立即可用且能提供 ITL 分布；B 提供 gateway 视角与
路由/租户维度，且最终让你能把 EPP 的 ITL 逻辑连同 `ResponseBody` 订阅一起删除——
**EPP 卸载才是本项目的性能收益主体**，B 的指标只是顺带品。

---

## 6. 方案 B 实施细节

### 6.1 第一步：打通 service 后端的响应解析（TTFT/TPOT 白拿）

目标：`Backend::Service` + `inferenceRouting` 时也设上 `llm_provider`，进入
`httpproxy.rs:1903` 的解析分支。

改动点：`httpproxy.rs:1563` 的 `Backend::Service` 分支，`build_service_call` 返回后，
若路由带 `ai` policy（或新增显式开关如 `ai.metrics: true`），在
`backend_call.backend_policies.llm_provider` 上补一个 OpenAI provider
（vLLM 是 OpenAI 兼容；构造参照 `local.rs:2100` 的
`LocalModelAIProvider::OpenAI → AIProvider::OpenAI(openai::Provider { model })`）。

三条纪律：

1. **绝不调用 `default_connector()`**（`llm/mod.rs:311`）：它会把 target 指向
   `api.openai.com:443` 并挂系统信任 TLS，会覆盖 EPP 已选好的端点。正确模式参照
   `httpproxy.rs:1535` 的 `host_override` 分支——"只挂 provider、不改连接"。
2. **不动 `ServiceCallOverride`**（`httpproxy.rs:1511`）：那是 EPP 选址结果的载体，
   `destination_passthrough` 行为必须原样保留。
3. **K8s 形态一并考虑**：建议做成 `AgentgatewayPolicy`（backend AI policy）+ 路由规则
   驱动的行为，而不是只在 local config 生效；controller 侧 `inference_plugin.go` 可
   顺带生成对应的 AI policy，与 InferenceRouting policy 指向同一 service target。

完成后 TTFT、TPOT、token usage、request duration 四个指标即刻可用。

**同时卸载 EPP**：把 EPP 里的 ITL 计算、`ResponseBody` 处理模式一并删除，只保留
请求 headers 处理（选端点所必需）。这是性能收益的主体。

### 6.2 第二步：新增 ITL（唯一真正的新指标）

**数据结构**（`llm/mod.rs:278`）：`first_token` 旁加 `last_token: Option<Instant>`。
在线观测（流中直接进 histogram）或离线累计（记 `itl_sum`/`itl_count`，落盘时 observe）
二选一：前者分布精确，后者改动最小。`LLMResponse` 的构造点要同步：
`llm/types/completions.rs:199`、`llm/types/messages.rs:389`/`:1031`、
`llm/types/responses.rs:455`、`llm/types/detect.rs:257`、`cel/types.rs:1126`/`:1738`、
`parse/websocket.rs:60`。

**hook**：vLLM 直通场景只需改 `passthrough_stream`（`completions.rs:936`）——第 957-961
行那个 `if !saw_token { saw_token = true; first_token = now }` 改造成
`record_token_chunk`：首次设 `first_token`；否则观测 `now − last_token` 并更新
`last_token`。这里每个 SSE chunk 必经，位置现成。

**其他 provider 的对称点**（不触发本场景，建议一并处理保持语义一致）：
`completions.rs:373`（`maybe_set_first_token`，调用点 `:498`/`:552`/`:960`）、
`messages.rs:600`/`:739`、`openai_compat.rs:614`/`:658`、`bedrock.rs:713`/`:1466`/`:2306`、
`responses.rs:42`。

**注册**（`metrics.rs`）：`gen_ai_server_inter_token_latency`，bucket 复用
`OUTPUT_TOKEN_BUCKET`（`metrics.rs:419`，1ms–2.5s，正合 ITL 量级），标签沿用
`GenAILabels`，与现有 LLM 指标同面板下钻。

### 6.3 顺手要做的两件小事

- **usage 注入选择**（见 3.3）：要么在 6.1 的请求方向对 OpenAI 直通请求强制注入
  `stream_options.include_usage: true`（客户端会多收到一个 usage 末尾 chunk，主流 SDK
  兼容，但属于行为变更，需注明）；要么不改请求、在文档里要求客户端自带。
  不处理的话 TPOT 和 token usage 仍是空的。
- **指标裁剪**：打通解析后 `gen_ai_client_token_usage` 会同时展开 input/output/
  input_cache_read/input_cache_write 四组标签（`log.rs:521-549`）。若只用延迟指标，
  可用 `FilteredRegistry`（`metrics.rs:156`，配置 `removes`）裁掉，控制基数。

---

## 7. 落地陷阱清单

1. **`include_usage` 前置条件**（最高优先级）：客户端不带则 vLLM 不发 usage，
   `output_tokens = None`，TPOT/token usage 静默缺失。见 3.3、6.3。
2. **chunk 间隔 ≠ 严格 token 间隔**：gateway 看到的是 SSE chunk 边界，一个 chunk 可含
   多 token（vLLM 批处理 / TCP 聚合）。报表必须标注口径；与 EPP 旧数据、vLLM 原生
   直方图三组口径互比时会有系统性偏差，属预期。
3. **TTFT 起算点偏差**：gateway 从"收到客户端请求"起算（`log.rs:557`，代码注释自认
   不够精准），含排队与策略耗时；vLLM 自报从引擎收到请求起算。两者差一个固定偏移。
4. **非流式请求无 ITL/TPOT**：`first_token` 只在 `req.streaming` 路径设置
   （`llm/mod.rs:956`）。分母口径注意。
5. **本地 tokenize 不可用作 output 兜底**：只算 input、且非 OpenAI tokenizer 一律回落
   Cl100kBase，开源模型有系统性偏差（3.4）。
6. **response buffer limit**：解析受 `http::response_buffer_limit` 约束，超长流需确认
   上限不截断（`completions.rs:942`）。
7. **EPP 连接默认 TLS 不验证 + fail-closed**：既有行为（4.3），卸载 EPP ITL 逻辑时
   不要顺手改这两个语义，除非有明确决策。
8. **错误响应不进流式路径**：非 2xx 走 buffered 路径（`llm/mod.rs:969`），不产生
   流式指标——监控上 ITL/TPOT 曲线断点可能与错误率尖峰同时出现，不是指标丢失。

---

## 8. 验证步骤

1. 起一个 vLLM（或 OpenAI 兼容 mock + 可控 chunk 间隔的脚本），按 6.1 配好 service
   后端 + `inferenceRouting`（local 形态可直接改 `examples/standalone-epp/config.yaml`）。
2. 发**带 `stream_options.include_usage: true`** 的流式请求，抓 gateway `/metrics`，确认：
   - `gen_ai_server_time_to_first_token` 有值；
   - `gen_ai_server_time_per_output_token` 有值；
   - 新增的 `gen_ai_server_inter_token_latency` 分布与 mock 的注入间隔吻合。
3. 再发**不带** include_usage 的请求，确认 TTFT/ITL 仍在、TPOT/token usage 如期为空——
   验证对前置条件的理解。
4. 与 vLLM 原生 `vllm:time_per_output_token_seconds` 交叉核对 p50/p99，量化
   gateway↔engine 视角偏移量，写入监控文档。
5. 压测对比：EPP 保留 ITL（基线） → EPP 卸载 + gateway 统计（目标态），量化 EPP CPU
   与端到端 token 间隔抖动两项收益。
6. 环境说明：本仓库所在集群当前没有 llm-d/EPP/vLLM 部署（`inferencepool` CRD 不存在、
   `polaris` 命名空间为空），上述验证需在真实 llm-d 环境或本地 mock 中进行。

---

## 9. 结论

| 指标 | 现状 | 前置条件 | 拿到它的路径 |
|---|---|---|---|
| TTFT | 代码已就绪 | 打通解析（6.1） | 方案 B 白拿；方案 A 立等可取 |
| TPOT | 代码已就绪 | 6.1 + `include_usage` | 方案 B 白拿；方案 A 立等可取 |
| ITL | 未实现 | 6.1 + 6.2 | 方案 B 新增；方案 A 用 vLLM 现有直方图兜底 |

EPP 的贵是协议决定的，只能挪、不能优化。agentgateway 是唯一一个"本就在看每一个 chunk"
的组件，把 ITL 放它这里是成本最小的正确位置；工作量的大头不在 ITL 本身（hook 现成），
而在打通 `Backend::Service` + `inferenceRouting` 下的响应体解析——这一步同时是 TTFT/TPOT
白拿的前提，做完即可把 EPP 的 ITL 逻辑和 `ResponseBody` 订阅整体删除，拿到全部性能收益。
