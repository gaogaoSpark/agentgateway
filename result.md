# InferencePool (warmup/warmup-ip) LLM 指标启用测试记录

目标：让 agentgateway（llmd-gateway）在 EPP 路径上产出 `gen_ai_*` 指标（TTFT / TPOT /
token usage），为后续新增 ITL 打基础。

**方法依据**：`custom` provider + `providerBackend` 指向 InferencePool 的正规路径
（详见对话分析；官方样例
`controller/pkg/syncer/backend/testdata/Valid_custom_backend_with_InferencePool_backendRef.yaml`）。

---

## 环境基线

| 项目 | 值 |
|---|---|
| kube context | `admin-ppio@kubernetes` |
| 节点 | 3 台，k8s v1.33.6（`host-192-168-172-{125,132,133}`） |
| Gateway | `polaris-gateway/llmd-gateway`，class `agentgateway`，PROGRAMMED=True，VIP `10.96.3.56` |
| Gateway 副本 | 5 个 pod，容器名 `agentgateway`，metrics 端口 `15020` |
| InferencePool | `warmup/warmup-ip`（`inference.networking.k8s.io/v1`） |
| EPP | `warmup-epp-svc:9002`，failureMode=**FailClose** |
| 后端 | `warmup-decode-0`(5/5 Running)、`warmup-prefill-0`(1/1 Running)，targetPort 8000 |
| HTTPRoute | `warmup/warmup-httproute` → InferencePool `warmup-ip:8000` |
| **路由匹配条件** | header `X-Infer-ID: yaochen-dev-i00ir2pb` + path prefix `/` |

Gateway pod IP：`10.244.19.155`、`10.244.33.27`、`10.244.33.9`、`10.244.33.29`、`10.244.19.169`

注意：gateway 容器内**没有 curl/wget**，抓指标须用临时 pod 访问 `<podIP>:15020/metrics`。

---

## 第 0 轮：基线确认

**操作**：直接抓 5 个 gateway 副本中 2 个的 `/metrics`，统计 `gen_ai` 出现次数。

```
curl -s http://10.244.19.155:15020/metrics | grep -c "gen_ai"   → 0
curl -s http://10.244.33.27:15020/metrics  | grep -c "gen_ai"   → 0
```

**指标端点自身正常**（排除"抓取方式错误"这一干扰项）：

```
curl -s http://10.244.19.155:15020/metrics | wc -l              → 610
# TYPE agentgateway_xds_connection_terminations counter
# TYPE agentgateway_tokio_num_alive_tasks gauge
# TYPE agentgateway_cgroup_usage gauge
...（610 行，含 xds/tokio/cgroup 等，但无任何 gen_ai_*）
```

**结论**：现状确认——EPP 路径下 agentgateway 完全不解析响应体，
`gen_ai_server_time_to_first_token` / `gen_ai_server_time_per_output_token` /
`gen_ai_client_token_usage` 全部不存在（Prometheus Family 无 label 组合被创建时不输出）。
与代码分析一致。

**状态**：❌ 无指标（预期，作为改动前基线）

### 0.1 补充：确认模型链路可用（排除"没流量所以没指标"）

首次请求返回 `{"error":"Unauthorized"}`——这是**上游 vLLM** 返回的（gateway 的
`default-policy` 里只有 accessLog/tracing/http，无任何认证策略），说明请求已成功穿过
gateway 与 EPP 到达后端。

从 `warmup-decode-0` 的 vllm 容器 args 中取得实际参数：

- 模型名：**`Qwen/Qwen3-0.6B`**（`--served-model-name`；不是 InferencePool selector 标签
  上的 `glm51`）
- API key：`--api-key a3f8b1c7...8f9a`（下文命令中以 `$KEY` 代指）
- 架构：P/D 分离，decode pod 内 4 个 vllm 实例（端口 7080-7083）+ 4 个 routing-proxy
  （9080-9083，`--kv-connector=nixlv2`），InferencePool targetPorts 指向 8000/9080-9083

带认证重发，**成功**：

```json
{"choices":[{"finish_reason":"length","message":{"content":"<think>\nOkay, the user just said \"hi\"...",
"role":"assistant"}}],"model":"Qwen/Qwen3-0.6B","usage":{"completion_tokens":16,...}}
```

请求后再抓全部 5 个副本：

```
10.244.19.155  gen_ai=0  requests_metric=23
10.244.33.27   gen_ai=0  requests_metric=32
10.244.33.9    gen_ai=0  requests_metric=21
10.244.33.29   gen_ai=0  requests_metric=30
10.244.19.169  gen_ai=0  requests_metric=24
```

**关键结论**：HTTP 层指标正常累积（`agentgateway_requests` 有值），但 `gen_ai_*` 依然为 0。
证明不是"无流量"，而是**agentgateway 确实没把这条流量当作 LLM 流量解析**——即
`llm_provider` 未设置。基线成立，与代码分析完全吻合。

**成功判据**（后续各轮以此为准）：`curl <podIP>:15020/metrics | grep gen_ai` 有输出，
且至少出现 `gen_ai_server_time_to_first_token`。

---

## 第 1 轮：尝试 `custom` provider + `providerBackend` —— 被版本阻断

**计划**：按分析结论创建 `AgentgatewayBackend`，用 `custom` provider +
`providerBackend` 指向 InferencePool。

**执行**：先探测 CRD 字段。

```
$ kubectl explain agentgatewaybackends.spec.ai.groups.providers | grep -E "^  [a-zA-Z]"
  anthropic   azure   azureopenai   bedrock   gemini
  host   name   openai   path   pathPrefix   policies   port   vertexai

$ kubectl explain agentgatewaybackends.spec.ai.groups.providers.custom
error: field "custom" does not exist
```

**`custom` 和 `providerBackend` 字段都不存在。** 查部署版本：

```
$ kubectl get pod -n polaris-gateway llmd-gateway-... -o jsonpath='{.spec.containers[*].image}'
image.paigpu.com/library/agentgateway:v1.2.1

$ kubectl get deploy -n agentgateway-system -o custom-columns=...
agentgateway   cr.agentgateway.dev/controller:v1.2.1
```

**集群部署的是 v1.2.1，不是我前面分析所依据的 v1.4.1。** 代码核实：

```
$ git grep -c "AIProvider::Custom" v1.2.1 -- '*.rs'
（无输出 → v1.2.1 完全没有 Custom provider）

$ git show v1.2.1:crates/agentgateway/src/llm/mod.rs | grep -n "pub enum AIProvider" -A 10
130:pub enum AIProvider {
131:	OpenAI(openai::Provider),
132:	Gemini(...)  Vertex  Anthropic  Bedrock  Azure  Copilot
（只有 7 个云厂商，无 Custom）
```

v1.2.1 中虽然 grep 到 `provider_backend`，但它在 `types/agent.rs:1985` 属于**别的结构**
（非 AI provider），与 v1.4.1 的 `NamedAIProvider.provider_backend` 不是同一个东西。
v1.2.1 的 `NamedAIProvider`（`llm/mod.rs:90`）字段只有：
`name` / `provider` / `host_override` / `path_override` / `path_prefix` / `tokenize` /
`inline_policies`——**没有 `provider_backend`**。

### 1.1 更关键的发现：v1.2.1 上 AI backend 与 EPP 真正互斥

v1.2.1 的 `Backend::AI` 分支（`proxy/httpproxy.rs:1521-1560`）：

```rust
let (target, provider_defaults) = match &provider.host_override {
    Some(target) => (target.clone(), BackendPolicies {
        llm_provider: Some(provider.clone()), ..Default::default() }),
    None => {
        let (tgt, mut pol) = provider.provider.default_connector();
        pol.llm_provider = Some(provider.clone());
        (tgt, pol)
    },
};
BackendCall { target, backend_policies: effective_policies, ... }
```

**target 直接来自 `host_override`（或 provider 默认值），完全不经过
`build_service_call` / `apply_inference_routing`。** 这与 v1.4.1 有本质区别：v1.4.1 的
AI backend 在有 `provider_backend` 时会回调 `build_simple_backend_call`，EPP 就在那里面
生效（`v1.4.1 httpproxy.rs:2043-2064`）；v1.2.1 没有这条回调路径。

**因此在 v1.2.1 上，AI backend 与 EPP 选端点是真正互斥的**：
走 AI backend ⇒ 有 `llm_provider` 但 EPP 不生效；走 InferencePool ⇒ EPP 生效但无
`llm_provider`。我前面基于 v1.4.1 得出的"正规路径"结论**不适用于当前部署版本**。

**状态**：❌ 方案受阻。v1.2.1 无 `custom` provider、无 AI provider 级
`providerBackend`，且 AI backend 不走 EPP 路径。

**下一步**：改为在 v1.2.1 上验证是否存在其他可行路径（HTTPRoute 直连 Service +
AI policy、或 host_override 指向 EPP 已知端点等），逐一实测。

---

## 第 2 轮：AI backend + `host` 指向后端 Service —— ✅ 指标出现

### 2.1 先穷尽 v1.2.1 上设置 `llm_provider` 的入口

`git grep llm_provider v1.2.1` 只有 7 处，其中**写入点仅 2 处**，都在
`httpproxy.rs:1540/1546` 的 `Backend::AI` 分支内：

```
httpproxy.rs:1540:  llm_provider: Some(provider.clone()),      # host_override 分支
httpproxy.rs:1546:  pol.llm_provider = Some(provider.clone()); # default_connector 分支
binds.rs:200:       pub llm_provider: Option<...>              # 字段定义
binds.rs:235:       llm_provider: other.llm_provider.or(...)    # merge
httpproxy.rs:1679:  if let Some(llm) = &...llm_provider        # 请求侧消费
httpproxy.rs:1904:  (...llm_provider, llm_request)             # 响应侧消费
```

关键：`internal_backend_policies()`（`binds.rs:891-1010`）逐一匹配
`BackendTrafficPolicy` 的所有变体，其中 `BackendTrafficPolicy::AI(p)` 只写
`pol.llm`（**LLM policy**，即 prompt guard / rate limit 等），**从不写
`pol.llm_provider`**。

**结论**：在 v1.2.1 上 `llm_provider` 只能由 `Backend::AI` 产生，
任何 AgentgatewayPolicy（traffic 或 backend）都无法补上。CRD 侧亦印证——
`kubectl explain agentgatewaypolicies.spec.traffic` 无任何 provider 字段。

### 2.2 改动内容

既然必须走 `Backend::AI`，就用它的 `host` 字段（对应 `host_override`）直连后端 Service，
绕开 InferencePool：

```yaml
apiVersion: agentgateway.dev/v1alpha1
kind: AgentgatewayBackend
metadata:
  name: warmup-ai-metrics
  namespace: warmup
spec:
  ai:
    groups:
    - providers:
      - name: vllm
        openai: {}                                        # 用 openai 协议解析请求/响应
        host: warmup-decode.warmup.svc.cluster.local      # → host_override
        port: 9080                                        # routing-proxy 端口
---
apiVersion: gateway.networking.k8s.io/v1
kind: HTTPRoute
metadata:
  name: warmup-metrics-test
  namespace: warmup
spec:
  parentRefs:
  - {group: gateway.networking.k8s.io, kind: Gateway, name: llmd-gateway, namespace: polaris-gateway}
  rules:
  - backendRefs:
    - {group: agentgateway.dev, kind: AgentgatewayBackend, name: warmup-ai-metrics, weight: 1}
    matches:
    - headers: [{name: X-Metrics-Test, type: Exact, value: round2}]
      path: {type: PathPrefix, value: /}
    timeouts: {request: 0s}
```

两者 `ACCEPTED=True`。用独立 header `X-Metrics-Test: round2` 与原
`X-Infer-ID` 路由并存，互不干扰。

### 2.3 非流式请求 → token usage 出现

```
POST /v1/chat/completions  -H 'X-Metrics-Test: round2'
{"model":"Qwen/Qwen3-0.6B","messages":[{"role":"user","content":"hi"}],"max_tokens":16}
→ 200 {"usage":{"prompt_tokens":9,"completion_tokens":16,"total_tokens":25}}
```

抓 5 个副本：`10.244.19.155 gen_ai=55`、`10.244.33.29 gen_ai=55`，其余 0
（只有承接了请求的副本会创建 metric family，符合预期）。

实际指标（已去 label）：

```
agentgateway_gen_ai_client_token_usage_sum{gen_ai_token_type="input", ...}   9.0
agentgateway_gen_ai_client_token_usage_count{gen_ai_token_type="input", ...}   1
agentgateway_gen_ai_client_token_usage_sum{gen_ai_token_type="output", ...}  16.0
agentgateway_gen_ai_client_token_usage_count{gen_ai_token_type="output", ...}  1
```

完整 label 集：`gen_ai_token_type` / `gen_ai_operation_name="chat"` /
`gen_ai_system="openai"` / `gen_ai_request_model` / `gen_ai_response_model` /
`bind` / `gateway` / `listener` / `route="warmup/warmup-metrics-test"` / `route_rule`。

### 2.4 流式请求 → TTFT / TPOT 出现

非流式请求只产出 token_usage 和 request_duration；**TTFT/TPOT 必须
`"stream": true`** 才会被记录。

```
{"model":"Qwen/Qwen3-0.6B","messages":[{"role":"user","content":"count to 20"}],
 "max_tokens":64,"stream":true}   → SSE 正常，data: [DONE]
```

`# TYPE` 行（`10.244.33.29`）——**4 个 family 全部到位**：

```
# TYPE agentgateway_gen_ai_client_token_usage           histogram
# TYPE agentgateway_gen_ai_server_request_duration      histogram
# TYPE agentgateway_gen_ai_server_time_per_output_token histogram
# TYPE agentgateway_gen_ai_server_time_to_first_token   histogram
```

实测值：

| 指标 | sum | count |
|---|---|---|
| `gen_ai_server_time_to_first_token` | 0.044242358 s (44.2 ms) | 1 |
| `gen_ai_server_time_per_output_token` | 0.002280235 s (2.28 ms) | 1 |
| `gen_ai_server_request_duration` | 0.29607479 s | 2 |

TTFT/TPOT 的 count=1（仅流式那一条），request_duration count=2（流式+非流式），
与 2.3/2.4 各发一次完全吻合。

**状态**：✅ **达到成功判据**——`gen_ai_server_time_to_first_token` 已产出。

### 2.5 代价：这条路径上 EPP 不生效（实测证实第 1 轮的代码结论）

对比两条路由的 access log：

**AI backend 路由**（本轮新建）——有 LLM 语义，无 EPP：

```json
{"route":"warmup/warmup-metrics-test",
 "endpoint":"warmup-decode.warmup.svc.cluster.local:9080",
 "protocol":"llm",
 "gen_ai.operation.name":"chat","gen_ai.provider.name":"openai",
 "gen_ai.request.model":"Qwen/Qwen3-0.6B","gen_ai.usage.input_tokens":9,
 "gen_ai.usage.output_tokens":16,"duration":"83ms"}
```

**InferencePool 路由**（原有）——有 EPP，无 LLM 语义：

```json
{"route":"warmup/warmup-httproute",
 "endpoint":"10.244.113.35:9083",
 "protocol":"http",
 "inferencepool.selected_endpoint":"10.244.113.35:9083","duration":"139ms"}
```

三点差异互为印证：

1. `protocol` 字段：`llm` vs `http`——只有 AI backend 路径被当作 LLM 流量解析。
2. `endpoint`：AI 路径是 **Service 域名**（`host_override` 直接生成 target，
   走 k8s Service 自身负载均衡）；InferencePool 路径是 **具体 Pod IP:port**（EPP 选出）。
3. AI 路径日志**没有** `inferencepool.selected_endpoint` 字段；EPP pod 在这几分钟内
   `kubectl logs --since=6m` **零输出**，确认根本没被调用。

这与第 1 轮读到的 `httpproxy.rs:1521-1560` 完全一致：`Backend::AI` 分支自己构造
`BackendCall{target}`，不经过 `build_service_call`，而 `service_override`
（承载 `inference_result.destination`）只在 `Backend::Service` 分支被消费。

---

## 阶段结论

在**当前部署的 v1.2.1** 上，两项能力二选一，无法同时获得：

| 路径 | `gen_ai_*` 指标 | EPP 端点选择 | 说明 |
|---|---|---|---|
| HTTPRoute → InferencePool | ❌ | ✅ | 现网 `warmup-httproute` 的行为 |
| HTTPRoute → AgentgatewayBackend(ai, host) | ✅ | ❌ | 本轮 `warmup-metrics-test` |

根因（v1.2.1 代码，非配置问题）：

- `llm_provider` 的**唯一**写入点是 `Backend::AI` 分支（`httpproxy.rs:1540/1546`），
  policy 层无法注入；
- `Backend::AI` 分支不调用 `build_service_call`，因此 `inference_result.destination`
  被丢弃，EPP 不生效。

**要两者兼得，必须升级到 ≥ v1.3.0**：`provider_backend` 字段自 v1.3.0
引入 `llm/mod.rs`（v1.2.1 为 0 处，v1.3.0/v1.3.1/v1.4.0/v1.4.1 均存在），
配合 `custom` provider 即可让 AI backend 把 InferencePool 当作
`providerBackend`，从而在同一条请求上同时拿到 `llm_provider` 与 EPP。
升级后按第 1 轮原计划的 `custom` + `providerBackend` 配置即可。

对**新增 ITL 指标**的意义：本轮已证明 agentgateway 的 LLM 响应解析链路
（`process_response` → 流式 chunk 计时）在此环境可正常工作并产出 TTFT/TPOT，
ITL 可以在同一处（`llm` 模块的流式统计）扩展，指标基础设施无需改动。

---

## 第 3 轮：两跳回环——✅ EPP 生效的同时产出 `gen_ai_*` 指标

第 2 轮的配置绕开了 EPP，不满足"在使用 EPP 的前提下暴露指标"的要求。
本轮改用**两跳回环**：请求先进 `Backend::AI` 分支（拿 `llm_provider`、出指标），
但 `host` 指回 gateway 自己的 Service；第二跳命中原有的 InferencePool 路由，
EPP 在那里真实选端点。响应回流时穿过第一跳的 `process_response`，指标照样产出。

### 3.1 改动内容

```yaml
apiVersion: agentgateway.dev/v1alpha1
kind: AgentgatewayBackend
metadata:
  name: warmup-ai-epp
  namespace: warmup
spec:
  ai:
    groups:
    - providers:
      - name: vllm
        openai: {}
        host: llmd-gateway.polaris-gateway.svc.cluster.local   # ← 指回 gateway 自身
        port: 80
---
apiVersion: gateway.networking.k8s.io/v1
kind: HTTPRoute
metadata:
  name: warmup-epp-metrics
  namespace: warmup
spec:
  parentRefs:
  - {group: gateway.networking.k8s.io, kind: Gateway, name: llmd-gateway, namespace: polaris-gateway}
  rules:
  - backendRefs:
    - {group: agentgateway.dev, kind: AgentgatewayBackend, name: warmup-ai-epp, weight: 1}
    filters:
    - type: RequestHeaderModifier
      requestHeaderModifier:
        remove: ["X-Metrics-Test"]          # 防回环：去掉本路由的匹配 header
        add:
        - name: X-Infer-ID                  # 让第二跳命中原 InferencePool 路由
          value: yaochen-dev-i00ir2pb
    matches:
    - headers: [{name: X-Metrics-Test, type: Exact, value: round3}]
      path: {type: PathPrefix, value: /}
    timeouts: {request: 0s}
```

防回环靠 header 重写：进入时匹配 `X-Metrics-Test: round3`，转发前删掉它并加上
`X-Infer-ID`，第二跳只会命中 `warmup-httproute`（InferencePool），不会再进本路由。

### 3.2 验证：EPP 真实参与

流式请求（`X-Metrics-Test: round3`）后，EPP pod 日志立即出现（第 2 轮同窗口为零输出）：

```
"msg":"EPP received request","x-request-id":"1b98a450-..."
"msg":"EPP sent request body response(s) to proxy","modelName":"Qwen/Qwen3-0.6B"
"msg":"EPP sent response body back to proxy"
```

access log 两跳同 `trace.id=1feb0fa938084c7f52b77d40415ec788`：

```json
// 第一跳：AI backend，protocol=llm，产出 gen_ai 字段
{"route":"warmup/warmup-epp-metrics",
 "endpoint":"llmd-gateway.polaris-gateway.svc.cluster.local:80",
 "protocol":"llm","gen_ai.usage.input_tokens":13,"gen_ai.usage.output_tokens":64,
 "duration":"287ms"}
// 第二跳：InferencePool 路由，EPP 选中具体 Pod
{"route":"warmup/warmup-httproute",
 "endpoint":"10.244.113.35:9083","protocol":"http",
 "inferencepool.selected_endpoint":"10.244.113.35:9083","duration":"282ms"}
```

### 3.3 验证：指标产出

承接第一跳的副本 `10.244.33.9`（第 2 轮时 gen_ai=0，本轮新出现，排除残留干扰）：

```
agentgateway_gen_ai_server_time_to_first_token_sum    0.139561618   count 1
agentgateway_gen_ai_server_time_per_output_token_sum  0.002310611   count 1
agentgateway_gen_ai_server_request_duration_sum       0.287440725   count 1
```

4 个 family（token_usage / request_duration / TTFT / TPOT）全部到位。

**状态**：✅ **EPP 生效 + `gen_ai_*` 指标产出，同一条请求上同时成立。**

### 3.4 代价与注意事项

1. **额外一跳延迟**：请求在 gateway 内多走一次完整代理（含第二次路由匹配 + EPP
   ext_proc 调用）。本轮实测两跳 duration 287ms vs 282ms，额外开销约 5ms。
2. **TTFT 略被高估**：第一跳测到的 TTFT 包含第二跳的转发开销（本轮 139.6ms，
   对比第 2 轮直连的 44.2ms——注意两轮后端负载不同，不能直接相减）。
3. **回环风险**：防回环完全依赖 header 重写正确配置。若 `remove` 写错，请求会
   无限循环直到耗尽连接。上线前必须验证。
4. **第二跳负载均衡**：第一跳到 gateway Service 会再经过一次 5 副本负载均衡，
   两跳可能落在不同 gateway pod（本轮即如此），指标分散在承接第一跳的 pod 上。
5. 这是 v1.2.1 上的**权宜之计**；升级 ≥ v1.3.0 后应改用 `custom` provider +
   `providerBackend` 的单跳正规路径（见阶段结论）。

---

## 最终结论

| 轮次 | 配置 | `gen_ai_*` | EPP | 状态 |
|---|---|---|---|---|
| 0 | HTTPRoute → InferencePool（现网） | ❌ | ✅ | 基线 |
| 2 | AI backend `host` 直连 decode Service | ✅ | ❌ | 不满足要求 |
| 3 | AI backend `host` 回环 gateway → InferencePool 路由 | ✅ | ✅ | **达成** |

v1.2.1 单跳无法两全（代码级互斥）；两跳回环是当前版本唯一同时满足
"EPP 生效 + LLM 指标"的配置。长期方案仍是升级 ≥ v1.3.0 用 `providerBackend`。

---

## 第 4 轮：升级 v1.4.1 + `custom` provider `backendRef` 指向 InferencePool

用户否决两跳回环（额外一跳生产不可接受），要求升级 v1.4.1 走单跳正规路径。

### 4.1 升级过程（2026-08-07/08）

| 组件 | 从 | 到 | 方式 |
|---|---|---|---|
| CRDs | v1.2.1 | v1.4.1 | helm 反复因 API server `http2 connection lost` 失败（CRD 对象大，patch 超时）；改为 `kubectl apply --server-side --force-conflicts` 逐个 CRD 应用，成功 |
| controller | v1.2.1 | v1.4.1 | `helm upgrade`（注意 v1.4.1 chart 需要 `agentgatewayModels` 值，`--reuse-values` 会 nil pointer，需显式传值）|
| 数据面镜像 | v1.2.1 | v1.4.1 | 官方镜像私仓没有 → `skopeo copy` 从 `cr.agentgateway.dev/agentgateway:v1.4.1` 同步到 `image.paigpu.com/library`，再 patch `AgentgatewayParameters/llmd-gateway-gp` 的 `image.tag`，5 副本滚动完成 |

清理了第 2/3 轮的 4 个测试对象（`warmup-metrics-test`/`warmup-ai-metrics`/
`warmup-epp-metrics`/`warmup-ai-epp`）。原 `warmup-httproute`（InferencePool 路由）
升级后实测仍正常。

### 4.2 新配置（正规单跳路径）

注意实际 CRD 字段是 `spec.ai.provider.custom`（单 provider 形式），不是文档示例里的
`spec.ai.llm`：

```yaml
apiVersion: agentgateway.dev/v1alpha1
kind: AgentgatewayBackend
metadata:
  name: warmup-llm
  namespace: warmup
spec:
  ai:
    provider:
      custom:
        backendRef:
          group: inference.networking.k8s.io
          kind: InferencePool
          name: warmup-ip
        formats:
        - type: Completions
---
# HTTPRoute warmup-llm-route: 匹配 X-Metrics-Test: round4 → backendRef 指向上面
```

两者 Accepted。config_dump 确认数据面收到了正确翻译：

```json
"provider": {"custom": {"formats": [{"type": "completions"}]}},
"providerBackend": {"service": {"name": "warmup/warmup-ip.warmup.inference.cluster.local", "port": 8000}}
```

### 4.3 结果：请求 503，EPP 未被调用 —— 发现 v1.4.1 的 bug

```
POST (X-Metrics-Test: round4) → 503 "upstream call failed: Connect: Connection refused"
access log: endpoint=10.244.19.137:8000 (= warmup-prefill-0 Pod IP), protocol=llm,
            有 gen_ai.request.* 字段但无 inferencepool.selected_endpoint
EPP 日志: 零输出
```

请求确实走了 LLM 解析(`protocol=llm`)并把 InferencePool 当 Service 负载均衡
（挑了 prefill pod 的 8000 端口，连接被拒），**但 EPP 完全没被咨询**。

### 4.4 根因分析（v1.4.1 代码级）

policy 存取双方的 key 不一致：

**写入侧**（controller，`inference_plugin.go:89`）：InferenceRouting policy 的 target 是
`ServiceTargetWithHostname(ns, "warmup-ip.warmup.inference.cluster.local", nil)` ——
**port=None**。config_dump 印证：`"target":{"backend":{"service":{"hostname":"warmup-ip.warmup.inference.cluster.local","namespace":"warmup"}}}`（无 port）。

**读取侧**（数据面，`httpproxy.rs:2043-2049`）：`provider_backend` 分支调用
`sub_backend_policies(provider_backend.backend.target(), ...)`，其中
`SimpleBackend::Service.target()` 返回 `BackendTargetRef::Service{..., port: Some(8000)}`
（`agent.rs:1565`），且 `sub_backend_policies` **不做 strip_section/port**
（`binds.rs:1157`，传 `backend=None, sub_backend=Some(带端口)`）。

`PolicyTargetRef` 是精确哈希匹配（`agent.rs` `Equivalent` impl）——
**`port: Some(8000)` ≠ `port: None`，查不到 InferenceRouting policy**，
`build_simple_backend_call` 里的 `apply_inference_routing` 拿到空配置，EPP 被跳过。

对照组：HTTPRoute 直连 InferencePool 之所以能命中，是因为那条路走
`backend_policies()`（`binds.rs:1191`），它同时用 `strip_section()`（去掉 port）和
原值查两次索引，port=None 的 policy 能被去 port 的 key 命中。

官方测试 `custom_llm_provider_service_backend_runs_inference_routing`
（`ext_proc_tests.rs:2477`）之所以通过，是因为测试自己构造的 policy target 是
`port: Some(STANDALONE_SERVICE_PORT)`（带端口），恰好和读取侧一致——
**测试没有覆盖"controller 生成的 port=None target"这一真实路径**。

### 4.5 修复与自建镜像

补丁（1 处）：`httpproxy.rs` `provider_backend` 分支改用 `backend_policies()`：

```rust
- let provider_backend_policies = inputs.stores.read_binds().sub_backend_policies(
-     provider_backend.backend.target(),
-     Some(&provider_backend.inline_policies),
- );
+ let provider_backend_policies = inputs.stores.read_binds().backend_policies(
+     provider_backend.backend.target(),
+     &[&provider_backend.inline_policies],
+     None,
+ );
```

`backend_policies` 内部会同时以 strip 掉 port 的 key 和原 key 查询，
与 HTTPRoute→InferencePool 路径行为一致。在 v1.4.1 tag 上打补丁本地构建，
镜像推 `image.paigpu.com/library/agentgateway:v1.4.1-eppfix`。

### 4.6 修复验证：✅ 单跳路径达成"EPP 生效 + 全部 gen_ai 指标"

数据面滚动到 `v1.4.1-eppfix`（5 副本）后重发同样的请求（`X-Metrics-Test: round4`）：

**非流式** → 200，usage 正常；**流式** → SSE 正常。

**EPP 真实参与**（日志完整链路，两条请求各一轮）：

```
"EPP received request" → "EPP sent request body response(s) to proxy" → "EPP sent response body back to proxy"
```

**access log 单条同时具备两组字段**（这是前 4 轮从未实现的状态）：

```json
{"route":"warmup/warmup-llm-route",
 "endpoint":"10.244.113.35:9080",
 "protocol":"llm",
 "inferencepool.selected_endpoint":"10.244.113.35:9080",    ← EPP 选点
 "gen_ai.operation.name":"chat","gen_ai.provider.name":"custom",
 "gen_ai.request.model":"Qwen/Qwen3-0.6B",
 "gen_ai.usage.input_tokens":9,"gen_ai.usage.output_tokens":16,
 "duration":"144ms"}
```

两条请求被 EPP 分到了不同实例（9080 / 9082），说明选点是动态的，不是 Service 兜底。

**指标**（副本 `10.244.19.145`，4 个 family 全部产出）：

```
agentgateway_gen_ai_server_time_to_first_token_sum    0.087721641   count 1
agentgateway_gen_ai_server_time_per_output_token_sum  0.002333922   count 1
agentgateway_gen_ai_server_request_duration_sum       0.237092644   count 1
agentgateway_gen_ai_client_token_usage                (input/output histogram)
```

**回归**：原 `warmup-httproute`（X-Infer-ID 路由）→ 200，不受补丁影响。

---

## 最终结论（更新）

| 轮次 | 版本 | 配置 | `gen_ai_*` | EPP | 额外跳数 |
|---|---|---|---|---|---|
| 0 | v1.2.1 | HTTPRoute → InferencePool | ❌ | ✅ | 0 |
| 2 | v1.2.1 | AI backend `host` 直连 Service | ✅ | ❌ | 0 |
| 3 | v1.2.1 | 两跳回环 | ✅ | ✅ | **+1（生产不可接受）** |
| 4 | v1.4.1 官方 | `custom` + `backendRef`→InferencePool | ✅(请求侧) | ❌（**bug**） | 0 |
| 5 | v1.4.1-eppfix | 同上 + 1 处补丁 | ✅ | ✅ | **0** |

**生产配置** = 第 4.2 节的 `AgentgatewayBackend`（`spec.ai.provider.custom.backendRef`
→ InferencePool）+ 数据面镜像 `v1.4.1-eppfix`。

**待办**：
1. 该 bug 应上报 agentgateway 上游（`httpproxy.rs` `provider_backend` 分支
   `sub_backend_policies` 与 controller 生成的 port=None policy target 不匹配；
   官方测试只覆盖了带 port 的 target）。上游合入后可回官方镜像。
2. 现网切换：把 `warmup-httproute` 的 backendRef 从 InferencePool 改为
   AgentgatewayBackend `warmup-llm`（或直接在其上加 rule），即可让 X-Infer-ID
   流量获得指标。本轮未动现网路由，留给用户决策。
3. ITL 指标：现在单跳路径上 `process_response` 的流式计时已工作，可以在
   `crates/llm` 的流式统计处直接加 inter-token latency。
