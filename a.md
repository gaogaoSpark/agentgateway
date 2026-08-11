# 将 ITL / TPOT 统计从 llm-d EPP 下沉到 agentgateway 的可行性分析

## 背景与目标

现状是自建模型服务（llm-d + vLLM），agentgateway 作为数据面代理，EPP（Endpoint Picker，Gateway
API Inference Extension）通过 ext-proc 协议参与选端点。ITL（Inter-Token Latency，相邻 token
到达间隔）目前在 EPP 内计算，代价是 EPP CPU 占用高并影响转发性能，希望改由 agentgateway 统计。

结论先说：**方案可行，且 agentgateway 是比 EPP 更合适的位置**，因为它本来就在流式路径上逐 chunk
解析 SSE。但当前代码有一个结构性阻塞点——EPP 接入模式（`inferenceRouting` + service 后端）下
agentgateway 完全不解析响应体，所以现有的 TTFT / TPOT 指标一个都产不出来，ITL 也无从谈起。需要一处
不大但必须的代码改动。

---

## 一、为什么在 EPP 里算 ITL 一定贵

这不是 EPP 实现问题，是 ext-proc 协议形态决定的。

要算 ITL，必须看到每一个 SSE chunk 的到达时刻。ext-proc 里这意味着 EPP 必须订阅
`ResponseBody` 流。agentgateway 侧对应的发送逻辑在
`crates/agentgateway/src/http/ext_proc.rs:546` 的 `handle_body_stream`：

```rust
let mut stream = BodyStream::new(body);
while let Some(Ok(frame)) = stream.next().await {
    let request = Some(if frame.is_data() {
        let frame = frame.into_data().expect("already checked");
        body_fn(HttpBody { body: frame.into(), end_of_stream: false })
    } ...);
    let Ok(()) = tx.send(ProcessingRequest { ... }).await else { return };
}
```

每一个 data frame 都是一条独立的 `ProcessingRequest`，走 gRPC 双向流发给 EPP。一次 1000
token 的流式输出就是约 1000 次 gRPC 消息往返（外加 protobuf 编解码和 body 字节拷贝）。而且
`mutate_response`（`ext_proc.rs:602`）的返回路径是对称的：响应体要从 EPP 再流回来，
`ext_proc.rs:680` 那里新建 channel、`ext_proc.rs:705` 再 spawn 一个 task 把 chunk 逐个转回下游。

也就是说 **EPP 算 ITL 的成本 = 每 token 两次跨进程消息 + 两次序列化**，并且它插在数据通路正中间，
其延迟直接叠加到用户感知的 token 间隔上——观测行为本身污染了被观测的指标。这正是你观察到的"成本高、
影响性能"。

对比之下 agentgateway 侧的成本：它为了 token 计数和 prompt guard，**已经在解析 SSE 流**了。加 ITL
的边际成本只是每 chunk 一次 `Instant::now()` 加一次 histogram 观测，纯进程内、无锁竞争、无序列化。
量级差异是数千倍。

---

## 二、agentgateway 现有能力盘点

### 已经有的：TPOT

`crates/agentgateway/src/telemetry/metrics.rs:138` 已注册 TPOT 指标：

```rust
pub gen_ai_time_per_output_token: Histogram<GenAILabels>,
```

对外名称 `gen_ai_server_time_per_output_token`（`metrics.rs:257`），计算在
`crates/agentgateway/src/telemetry/log.rs:566`：

```rust
if let Some(ot) = llm_response.output_tokens {
    let first_to_last = end_time.as_instant().duration_since(ft);
    let throughput = first_to_last.as_secs_f64() / (ot as f64);
    log.metrics.gen_ai_time_per_output_token
        .get_or_create(&gen_ai_labels)
        .observe(throughput);
}
```

即 `(最后 chunk 时刻 - 首 token 时刻) / output_tokens`，符合 TPOT 标准定义。**所以 TPOT 你不需要
自己实现，它已经在了。** bucket 定义在 `metrics.rs:419` 的 `OUTPUT_TOKEN_BUCKET`，覆盖 1ms–2.5s。

顺带 TTFT 也有：`gen_ai_server_time_to_first_token`（`metrics.rs:266`），在 `log.rs:556` 计算。
注意其语义是"从 gateway 收到请求"起算，而非"从 gateway 发出上游请求"起算，代码里也留了注释承认这点
（`log.rs:559`）——和 vLLM 自报的 TTFT 会有个固定偏移，对比时要留意。

### 没有的：ITL

全代码库 grep 不到 `inter_token` / `itl` 相关指标。`LLMResponse`
（`crates/agentgateway/src/llm/mod.rs:278`）里只有一个流式时间戳：

```rust
#[serde(skip)]
// Time to get the first token. Only used for streaming.
pub first_token: Option<Instant>,
```

只记首个 token 的时刻，后续 chunk 的到达时间没有留存，因此无法还出间隔分布。ITL 是本次唯一真正
需要新增的指标。

---

## 三、关键阻塞点：EPP 模式下响应体根本没被解析

这是本方案最重要的发现，也是我上一轮把话题绕到 `llm_provider` 的原因——**它在代码里不是"选云厂商"
的开关，而是"是否解析响应体"的总闸**。

### 3.1 闸门位置

`crates/agentgateway/src/proxy/httpproxy.rs:1903`：

```rust
let mut resp = if let (Some(llm), Some(llm_request)) =
    (backend_call.backend_policies.llm_provider, llm_request)
{
    llm.provider.process_response(...).await?
} else {
    resp   // 原样透传，不解析
};
```

`llm_provider` 为 `None` 时，响应体被原样透传。而所有流式指标——`first_token`、`output_tokens`——
都是在 `process_response` 内部的解析循环里填充的。没有它，`log.rs:556` 和 `log.rs:566` 两个
`if let Some(...)` 全部落空，TTFT / TPOT / token usage 一个都不产出。

### 3.2 为什么 EPP 模式下它必然是 None

`llm_provider` 只在 `Backend::AI` 分支被赋值，`httpproxy.rs:1540` 和 `httpproxy.rs:1546`：

```rust
None => {
    let (tgt, mut pol) = provider.provider.default_connector();
    pol.llm_provider = Some(provider.clone());
    (tgt, pol)
},
```

而 `Backend::Service` 分支（`httpproxy.rs:1563`）走 `build_service_call`，**不设置
`llm_provider`**，保持 `None`。

同时 `inferenceRouting` 被硬性限制只能挂在 service 后端上。
`crates/agentgateway/src/types/local.rs:1484` 的 `validate_inference_routing_scope`：

```rust
match scope {
    InferenceRoutingScope::ServiceRouteBackend => Ok(()),
    InferenceRoutingScope::NonServiceRouteBackend => {
        bail!("inferenceRouting is only supported on service route backends")
    },
    InferenceRoutingScope::NamedBackend => { bail!(...) },
    InferenceRoutingScope::AIProviderPolicies => {
        bail!("inferenceRouting is only supported on service route backends, not AI provider policies")
    },
}
```

校验点分别在 `local.rs:2637`（路由后端，按是否 `LocalBackend::Service` 二选一）、`local.rs:1835`
（命名后端）、`local.rs:838`（AI provider 策略）。

于是形成一个死结：

| 想要的 | 需要的后端类型 | 结果 |
|---|---|---|
| EPP 选端点（`inferenceRouting`） | 必须 `Backend::Service` | `llm_provider` = None |
| 解析 SSE 出指标 | 必须 `Backend::AI` | 无法配 `inferenceRouting`，启动即报错 |

`examples/standalone-epp/config.yaml` 就是标准 EPP 接法，它拿不到任何 LLM 指标。

### 3.3 配置层面绕不开

值得说明的是，路由级和后端级的 `ai` policy 都只填 `policies.llm`，**不填 `llm_provider`**：

- 路由级：`local.rs:2882` → `TrafficPolicy::AI`
- 后端级：`local.rs:1569` → `BackendTrafficPolicy::AI` → `binds.rs:963` → `pol.llm`

两者最终合入 `llm_request_policies`（`httpproxy.rs:1674`），管的是 prompt 改写、模型别名、
限流这类策略，**与"是否解析响应体"无关**。所以纯改 YAML 无解，必须动代码。

---

## 四、实施方案

### 第一步：打通 service 后端的响应解析（必须，改动小）

目标是让 `Backend::Service` + `inferenceRouting` 的组合也能设上 `llm_provider`，从而进入
`httpproxy.rs:1903` 的解析分支。

做法：在 `httpproxy.rs:1563` 的 `Backend::Service` 分支，当该路由带有 `ai` policy 时，为
`backend_policies` 补设 `llm_provider`。provider 类型用 OpenAI 兼容
（`LocalModelAIProvider::OpenAI`，见 `local.rs:2100`），因为 vLLM 暴露的就是 OpenAI 兼容接口，
`process_completions_request` / `process_response` 这条路径可直接复用。

需要注意几点：

- **不要走 `default_connector()`**。它会把 target 改成 `api.openai.com:443` 并挂上系统信任的
  TLS（`llm/mod.rs:311` 附近），必须保留 EPP 已选定的端点。参考 `httpproxy.rs:1535` 的
  `host_override` 分支——那里就是"只挂 provider、不改连接目标"的现成写法。
- **`destinationMode: passthrough` 的行为不能变**。`httpproxy.rs:1511` 的
  `ServiceCallOverride` 是 EPP 选址结果的载体，改动不应触碰它。
- 建议加配置开关（如 `ai.metrics.enabled`）而非无条件开启，因为解析响应体对非 LLM 流量是纯开销。

这一步做完，**TTFT、TPOT、token usage 立即全部可用，完全不碰 EPP**——EPP 继续只做请求侧选端点，
连响应体都不用订阅，你就可以把 EPP 里的 ITL 计算逻辑连同 `ResponseBody` 订阅一起删掉，这才是性能
收益的主要来源。

### 第二步：新增 ITL 指标

改动点集中，模式统一。

**数据结构**（`llm/mod.rs:278`）：在 `first_token` 旁增加

```rust
#[serde(skip)]
pub last_token: Option<Instant>,
```

若要在流中在线观测，还需持有 histogram 引用；或者累积 `itl_sum` / `itl_count` 到日志落盘时统一
observe（前者分布更准，后者改动更小）。注意 `LLMResponse` 有多处构造点需同步补字段：
`llm/types/completions.rs:199`、`llm/types/messages.rs:389` 与 `:1031`、
`llm/types/responses.rs:455`、`llm/types/detect.rs:257`、`cel/types.rs:1126` 与 `:1738`、
`parse/websocket.rs:60`。

**钩子函数**：`llm/conversion/completions.rs:373` 现有

```rust
fn maybe_set_first_token(state: &mut StreamState, log: &AmendOnDrop) {
    if state.sent_first_token { return; }
    state.sent_first_token = true;
    log.non_atomic_mutate(|r| { r.response.first_token = Some(Instant::now()); });
}
```

改造成 `record_token_chunk`：首次设 `first_token`，后续每次计算 `now - last_token` 并观测，
然后更新 `last_token`。调用点已经现成（`completions.rs:498` 和 `:552`，每个 content delta
都会走到）。

**其余 provider 的对称改动**：同样的流式时间戳赋值散落在

- `llm/conversion/messages.rs:600`、`:739`
- `llm/conversion/openai_compat.rs:614`、`:658`
- `llm/conversion/bedrock.rs:713`、`:1466`、`:2306`
- `llm/conversion/responses.rs:42`
- `llm/conversion/completions.rs:960`

你的场景（vLLM OpenAI 兼容）主要只需要 `completions.rs` 和 `openai_compat.rs`；bedrock 那几处
可以不动，但为保持一致建议一并处理。

**指标注册**（`metrics.rs`）：新增
`gen_ai_server_inter_token_latency`，bucket 直接复用 `OUTPUT_TOKEN_BUCKET`
（`metrics.rs:419`，1ms–2.5s，正合 ITL 量级）。标签沿用现有 `GenAILabels`，与 TTFT / TPOT 对齐，
便于同一套 Grafana 面板下钻。

### 第三步（可选）：暴露给 CEL / 访问日志

`cel/types.rs:1058` 目前明确把 `first_token` 排除在 CEL 之外：

```rust
// For now, not exposed to CEL; only used to piggy-back this field for metrics.
#[serde(skip)]
#[dynamic(skip)]
pub first_token: Option<Instant>,
```

若你希望在访问日志里落每请求的 ITL（做长尾排查、按租户归因），需要把计算好的**标量**
（如 `itl_avg_ms`、`itl_p95_ms`）而非 `Instant` 暴露出去——`Instant` 无法序列化，这也是它当前被
`skip` 的原因。

---

## 五、精度与语义注意事项

这几点会直接影响你和 EPP 旧数据、以及和 vLLM 自报指标的对齐，建议写进监控文档。

**chunk 间隔 ≠ 严格 token 间隔。** agentgateway 只能看到 SSE chunk 边界。一个 chunk 可能含多个
token（vLLM 侧批处理、或网络层聚合），此时测出的间隔偏大而样本数偏少。若要严格 per-token，需用
chunk 内 token 数做摊分。常规监控用 per-chunk 间隔足够，但**报表里必须标注是 chunk 间隔**，否则
和 EPP 旧数据对比会出现无法解释的偏差。

**网络与代理开销被计入。** gateway 测的是 chunk 到达 gateway 的时刻，含 vLLM→gateway 的网络
延迟。相比在 EPP 里测其实更接近用户真实体验，但会略高于 vLLM 自报的 ITL。

**TTFT 起算点。** 如前所述是"gateway 收到请求"而非"发出上游请求"，含排队和策略执行时间。

**非流式请求无 ITL。** `first_token` 只在流式路径设置，非流式（`stream: false`）请求不会产出
ITL/TPOT，只有 request_duration 和 token usage。分母口径要注意。

---

## 六、环境核查结果

按"环境只做参考"的要求，这部分仅作事实记录，不影响上述结论：

当前 kube 集群**没有 llm-d / EPP / vLLM 部署**，模型服务不可用：

- `inferencepool` CRD 不存在（`the server doesn't have a resource type "inferencepool"`），
  即未安装 Gateway API Inference Extension，因此没有 EPP。
- `polaris` 命名空间为空；全集群未发现 vllm / decode / prefill 相关工作负载。
- 唯一可应答的 `polaris-gateway` 返回 `{"error":"inference_id is required"}`，是自研网关私有
  协议，非 OpenAI 兼容后端。
- 本机 8080 / 8765 分别为 nginx 与一个 Python HTTP server，`/v1/models` 均 404。

因此上述分析全部基于代码走读，未做运行时验证。方案落地后建议的验证路径：起一个 vLLM（或
OpenAI 兼容 mock），按第一步改动配好 service 后端 + `inferenceRouting`，发流式请求后抓
`/metrics`，确认 `gen_ai_server_time_to_first_token`、
`gen_ai_server_time_per_output_token`、`gen_ai_server_inter_token_latency` 三者同时有值，
再与 vLLM 自报指标交叉核对偏移量。

---

## 七、结论

| 指标 | 现状 | 需要做的事 |
|---|---|---|
| TTFT | 代码已实现（`metrics.rs:266`） | 仅需第一步打通解析 |
| TPOT | 代码已实现（`metrics.rs:257`） | 仅需第一步打通解析 |
| ITL | 未实现 | 第一步 + 第二步 |

把 ITL 从 EPP 移到 agentgateway 是正确方向：EPP 侧每 token 两次 gRPC 往返的开销可以完全消除，
agentgateway 侧因为复用已有的 SSE 解析循环，边际成本可忽略。真正的工作量不在 ITL 本身（钩子位置
现成），而在第一步打通 `Backend::Service` + `inferenceRouting` 场景下的响应体解析——这是当前
版本的结构性缺口，也是顺带把 TTFT / TPOT 一起白拿的前提。
