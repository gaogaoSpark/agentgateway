# v1.4.1 版本核查增补：observability_mode 与方案 D 的最终结论

> 本文是 `a.md`、`b.md` 的**版本增补**，不重复前两篇的论证。
> 目的：在 v1.4.1 上核实 ext-proc 是否已支持异步/旁路观测（决定方案 D 是否可行），
> 并把方案 B 的前提条件在新版本上重新验证一遍。
>
> 核查方式：全部通过 `git grep <tag>` / `git show <tag>:<path>` 在 tag 上直接读取，
> **未切换分支、未改动工作区**，`a.md` / `b.md` 作为未跟踪文件不受影响。

---

## 0. 一个必须先说的意外：v1.4.1 比当前 HEAD 更新

| | commit | 日期 | 说明 |
|---|---|---|---|
| 当前工作区 HEAD | `6a2d02db` | 2026-05-15 | `api: allow validation on older k8s versions (#1838)` |
| v1.4.1 tag | `163ea214` | 2026-07-29 | `promptGuard: apply guard to merged consecutive text blocks (#2755)` |

```
git merge-base --is-ancestor v1.4.1 HEAD  →  NO: v1.4.1 has commits not in HEAD
```

**当前工作区落后于 v1.4.1 约两个半月**，且 v1.4.1 含有 HEAD 上不存在的提交。

这意味着 `a.md` / `b.md` 里引用的所有行号都基于旧版本。v1.4.1 做了两处大重构：

- `http/ext_proc.rs` 单文件 → 模块化：`http/ext_proc/{processing,mutation,buffering,channel,headers}.rs`
- LLM 相关代码抽出为独立 crate：`crates/llm/`（原 `crates/agentgateway/src/llm/`）

**结论：方案 B 的实现应基于 v1.4.1，而非当前 5 月的 HEAD。**

---

## 1. `observability_mode`：v1.4.1 仍未实现

这是本次核查的首要问题，答案是明确的"没有"。

v1.4.1 中该字段出现 7 次，**全部硬编码为 `false`**：

```
crates/agentgateway/src/http/ext_proc.rs:606:   observability_mode: false,
crates/agentgateway/src/http/ext_proc.rs:622:   observability_mode: false,
crates/agentgateway/src/http/ext_proc.rs:636:   observability_mode: false,
crates/agentgateway/src/http/ext_proc.rs:872:   observability_mode: false,
crates/agentgateway/src/http/ext_proc.rs:1264:  observability_mode: false,
crates/agentgateway/src/http/ext_proc.rs:1285:  observability_mode: false,
crates/agentgateway/src/http/ext_proc.rs:1400:  observability_mode: false,
```

配套证据：

- `ExtProc` 配置结构体（`ext_proc.rs:249-272`）中**没有** observability 相关字段。
- Go 侧（`api/`、`controller/`）grep `observability_mode` / `ObservabilityMode` **零命中**。
- 全库 `observability` 的其他命中均为无关内容（listener 可观测性字段分类测试、文档注释等）。

**相比当前 HEAD（4 处硬编码 false）没有任何功能性变化**，只是因为文件重构导致出现次数从 4 增至 7。

---

## 2. v1.4.1 的实质进展：可配置的 `ProcessingOptions`

这是本次核查发现的**真实新能力**，虽然不解决方案 D，但值得记录。

`crates/agentgateway/src/http/ext_proc/processing.rs:101`：

```rust
pub struct ProcessingOptions {
    /// How request bodies are sent to the external processing service.
    #[serde(default = "default_body_send_mode")]
    pub request_body_mode: BodySendMode,
    /// How response bodies are sent to the external processing service.
    #[serde(default = "default_body_send_mode")]
    pub response_body_mode: BodySendMode,
    pub request_header_mode: HeaderSendMode,
    pub response_header_mode: HeaderSendMode,
    pub request_trailer_mode: TrailerSendMode,
    pub response_trailer_mode: TrailerSendMode,
    /// Whether the external processing service can change processing modes during a request.
    pub allow_mode_override: bool,
}
```

`BodySendMode`（`processing.rs:27`）四档，直接映射 Envoy 语义：

| 档位 | 含义 |
|---|---|
| `None` | 不发送 body 给 ext-proc |
| `Buffered` | 缓冲完整 body 后发送 |
| `BufferedPartial` | 缓冲至配置上限后发送 |
| `FullDuplexStreamed` | 双向流式（默认值，见 `processing.rs:125`） |

**且已暴露到用户配置**（`ext_proc.rs:270-272`）：

```rust
/// Controls which request and response parts are sent to the external processing service.
#[serde(default)]
pub processing_options: ProcessingOptions,
```

即 v1.4.1 起，你可以通过配置精确控制第二个 ext-proc 订阅哪些部分。**这是当前 HEAD 完全没有的
能力**（旧版连 `ProcessingMode` 配置入口都不存在，发什么阶段全是写死逻辑）。

---

## 3. 方案 D 最终结论：仍然不成立

### 3.1 回吐机制一字未改

`crates/agentgateway/src/http/ext_proc/mutation.rs:274-277`：

```rust
Mutation::StreamedResponse(bb) => {
    let eos = bb.end_of_stream;
    let _ = body_tx.send(Ok(Frame::data(bb.body))).await;
    trace!(%eos, "got body chunk");
```

下游客户端收到的每个 chunk，**来源仍然只能是 ext-proc 回包里的 `StreamedResponse`**。
ext-proc 不是"旁路复制一份给你看"，而是"代理把 body 交出去、等你还回来才能继续下发"。

### 3.2 由此形成的死循环

| 配置 | 能否看到 chunk 到达时刻 | 是否挡在数据通路上 | 能否算 ITL |
|---|---|---|---|
| `response_body_mode: FullDuplexStreamed` | ✅ 能 | ❌ 必须逐 chunk 回吐 | 能算，但成本 = EPP 原样 |
| `response_body_mode: None` | ❌ 看不到 | ✅ 不挡 | **算不出** |
| `Buffered` / `BufferedPartial` | 只看到聚合后的整体 | 挡（且引入缓冲延迟） | 算不出逐 chunk 间隔 |

**看得到 chunk 就必须挡在路上；不挡在路上就看不到 chunk。**
`observability_mode` 正是协议为打破这个循环而设的字段——而它没实现（第 1 节）。

所以"EPP 保持独立 + 另加一个 ext-proc 异步计算"在 v1.4.1 上：

- 若订阅 body → 每 token 两次 gRPC 往返 + 两次序列化，成本与 EPP 完全相同，**只是换了个进程**，
  端到端 token 间隔抖动一分未减，甚至因多一跳而略增。
- 若不订阅 body → 拿不到 ITL，方案失去意义。

**判决：方案 D 在 v1.4.1 上依然不可行，且不是配置问题，是协议实现缺口。**

### 3.3 顺带发现：EPP 自己的 body 模式是硬编码的

`InferenceRouting::build`（`ext_proc.rs:188-197`）：

```rust
ProcessingOptions {
    request_body_mode: BodySendMode::FullDuplexStreamed,
    response_body_mode: BodySendMode::FullDuplexStreamed,
    request_trailer_mode: TrailerSendMode::Send,
    response_trailer_mode: TrailerSendMode::Send,
    ..Default::default()
}
```

EPP 路径**不读用户配置**，写死 `FullDuplexStreamed`。含义有两层：

1. 你无法通过 agentgateway 侧配置让 EPP 停止订阅响应体——**EPP 的响应体成本必须在 EPP
   自己的 processing mode 声明里卸载**（EPP 侧代码改动，非 gateway 配置）。
2. 反过来说，只要 EPP 不主动声明放弃 response body，gateway 就会一直把每个 chunk 泵给它。
   这正是你观察到的 EPP 高负载的来源，也是方案 B 落地后**必须同步做的一件事**：删除 EPP
   的 ITL 逻辑时，一并把它的 response body 订阅关掉，否则成本照旧。

---

## 4. 方案 B 前提条件在 v1.4.1 上的复核

三项前提**全部依然成立**，行号已更新为 v1.4.1 基准。

### 4.1 TTFT / TPOT 指标仍在（可白拿）

```
crates/agentgateway/src/telemetry/metrics.rs:208:  pub gen_ai_time_per_output_token: Histogram<GenAILabels>,
crates/agentgateway/src/telemetry/metrics.rs:209:  pub gen_ai_time_to_first_token: Histogram<GenAILabels>,
crates/agentgateway/src/telemetry/metrics.rs:329:  "gen_ai_server_request_duration",
crates/agentgateway/src/telemetry/metrics.rs:338:  "gen_ai_server_time_per_output_token",
crates/agentgateway/src/telemetry/metrics.rs:347:  "gen_ai_server_time_to_first_token",
```

### 4.2 ITL 仍未实现（需新增）

`inter_token` / `itl` / `interTokenLatency` 在 v1.4.1 全库 **零命中**。

### 4.3 `llm_provider` 死结仍在（必须改代码）

`validate_inference_routing_scope` 位置从 `local.rs:1484` 移到
`crates/agentgateway/src/types/local.rs:2447`，**三条 bail 一字未改**：

```
local.rs:2457:  bail!("inferenceRouting is only supported on service route backends")
local.rs:2460:  bail!("inferenceRouting is only supported on service route backends, not named backends")
local.rs:2464:  "inferenceRouting is only supported on service route backends, not AI provider policies"
```

并且 v1.4.1 **新增了对应的回归测试**（`types/local_tests.rs:1334`、`:1702`、`:1728`），说明这是
社区**有意维持**的约束，不是待修的疏漏——改动时需要同步调整这些测试，并考虑上游是否愿意接受。

### 4.4 新便利：TPOT 已暴露到 CEL

`crates/agentgateway/src/cel/types.rs:1475`：

```rust
pub time_per_output_token: Option<CelDuration>,
```

这是当前 HEAD 没有的。含义：**做 per-request 的 TPOT 归因（访问日志、按租户下钻）不再需要
自己改 CEL 暴露层**，直接可用。对应地，`b.md` 第 6.3 节"顺手要做的两件小事"里关于 CEL 暴露的
描述可以简化——ITL 若也想进 CEL，照 `time_per_output_token` 的现成模式加即可（注意仍需暴露为
标量 `CelDuration`，不能是 `Instant`）。

---

## 5. 一个反向观察：v1.4.1 的重构让"实现 observability_mode"变容易了

如果未来仍想走独立统计服务路线（方案 D），v1.4.1 是比旧版更好的起点。新增的状态机基础设施：

```
processing.rs 导出：BodyPath, BodyStreamDirection, FirstExtProcMessage, HeaderPhase,
                    ModeStateMachine, RequestFlowFsm, RequestLoopStep, RequestPhase,
                    ResponseFlowFsm, ResponseLoopMessage, ResponsePhase
```

`ResponseFlowFsm` / `ModeStateMachine` 已经把"响应流转在哪个阶段、是否等待回包"抽象出来了
（见 `ext_proc.rs:1372-1440` 的 `response_body_streamed_before_header_response` 门控逻辑），
加一个 fire-and-forget 分支比在旧的单文件代码里改要干净得多。

**但这不改变优先级判断**：

| | 方案 D（补 observability_mode + 独立服务） | 方案 B（gateway 内统计） |
|---|---|---|
| agentgateway 改动 | 加配置字段 + FSM 加旁路分支 + 4~7 处硬编码改读配置 | 补 `llm_provider` + ITL hook |
| 额外组件 | 需新部署运维一个统计服务 | 无 |
| SSE 解析 / token 计数 | **要在新服务里重新实现一遍** | 复用现成的 `passthrough_stream` |
| 观测开销 | 每 chunk 一次单向 gRPC + 序列化 | 每 chunk 一次 `Instant::now()` |

改动量同一量级，方案 B 省掉一整个组件和一遍重复实现。

**唯一值得重新考虑方案 D 的场景**：ITL 统计需要复杂有状态聚合（跨请求关联、直写外部时序库、
自定义采样策略），不适合放进数据面进程。若只是产出 Prometheus 直方图，方案 B 明确更优。

---

## 6. 结论与下一步

1. **`observability_mode` 在 v1.4.1 未实现**，7 处硬编码 `false`，无配置入口。
2. **方案 D 不可行**：body 必须回吐（`mutation.rs:276`）→ 看得到 chunk 就必须挡在路上，
   成本与 EPP 等同，只是换了个进程。
3. **v1.4.1 的真实进展是 `ProcessingOptions` 可配置**，但四档 `BodySendMode` 没有一档能同时
   满足"看到 chunk"且"不挡路"。
4. **EPP 的 body 模式硬编码**（`ext_proc.rs:192`），卸载 EPP 成本必须动 EPP 自身声明，
   这是方案 B 落地时不能漏的一步。
5. **方案 B 三项前提在 v1.4.1 全部成立**，且 TPOT 已进 CEL，实现环境比旧版更好。
6. **实现基线切到 v1.4.1**（当前 HEAD 落后两个半月），注意代码已重构：
   ext_proc 模块化、LLM 迁到 `crates/llm/`——`b.md` 第 6 节的文件路径需按此调整
   （如 `llm/conversion/completions.rs` → `crates/llm/src/conversion/completions.rs`）。

**建议下一步**：在 v1.4.1 基线上实施方案 B 第一步（打通 `Backend::Service` +
`inferenceRouting` 的响应体解析），先验证 TTFT / TPOT 出数，再补 ITL。同步准备 EPP 侧
关闭 response body 订阅的改动——性能收益主体在那里。
