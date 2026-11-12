# agentgateway 侧优化说明：EPP 前缀缓存 hash 下移

分支 `yaochen/epp-prefix-cache-offload`（基于 ea560864）。配套 EPP 改动见
`/data/code/llm-d-inference-scheduler-internal/optimize.md`。详细设计与交接见 `HANDOFF-epp-prefix-cache.md`。

## 目标

把 EPP（llm-d inference scheduler）里"解析请求体 → 估算 token → 算前缀 hash"这段工作挪到网关做，
EPP 只接 hash 做调度。网关有多个副本可以摊这部分 CPU，EPP 单实例不再对每个请求做全文解析。

## 改了什么

### 1. 投影通道（`AGENTGATEWAY_EPP_BODY_PROJECTION=1`）

默认关，环境变量开启。开启后 InferencePool 路由发给 EPP 的不再是客户端原始 body，而是一个投影：

- 网关先把 body 读完，用 EPP `estimate` 后端同样的方式把文本打成 4 字节伪 token，
  按 EPP `approximate-prefix-cache-producer` 同样的链式 xxh64 算块 hash（默认 blockSize 64、最多 2048 块，
  与 EPP 默认对齐）。
- 发给 EPP 的投影只有 **6 个标量字段** `model / stream / max_tokens / max_completion_tokens /
  max_output_tokens / cache_salt` + 一个信封键 `x-agentgateway-prefix-cache`，信封里放每个 prompt 的
  `tokenCount` 和 `approximateHashes`（base64 小端 u64）。**不发 messages / prompt / tools 正文，不发 token ids。**
- EPP 返回后只采纳它的 header 变更（目标 endpoint），body 换回客户端原文发给 vLLM。vLLM 永远看不到信封。
- 算不出 hash 的形状（多模态 content、带 tools 的 chat、embeddings、responses）fail-open：
  发带正文的投影、不带信封，EPP 自己算，功能不受影响。

关掉开关就是上游原始行为，一行代码路径都不进。

### 2. 响应侧不再发 body 给 EPP

`response_body_mode` 从上游的 `FullDuplexStreamed` 改为 `None`，只透传 trailers（EPP 读 `grpc-status`）。
不受开关控制。vLLM 的 HTTP/1.1 响应没有 trailer，所以 EPP 在响应阶段实际收不到任何消息。

### 3. 配置

- 默认参数不需要任何配置，开开关即可。
- 要改参数（开 precise 链、改 blockSize 等）：本地 YAML 走 `inferenceRouting.prefixCache`；
  Kubernetes 走环境变量 `AGENTGATEWAY_EPP_PREFIX_CACHE='{...}'`（xDS proto 还没有这个字段）。
- `shipTokens: true` 可以让信封带回 token ids（让 EPP 在参数不匹配时能本地重算），默认关。

## 效果

### 发给 EPP 的字节数（release 实测，chat 请求）

| 客户端 body | 旧路径（原文直发） | 第一版（正文 + tokens + hash） | 现在（6 标量 + hash） |
|---|---|---|---|
| 7 KB | 7 KB | 17 KB（2.38×） | **621 B**（0.09×） |
| 69 KB | 69 KB | 164 KB | **3.2 KB**（0.05×） |
| 690 KB（100k token） | 690 KB | 1.63 MB | **22 KB**（0.03×） |

剩下的 22 KB 就是 2048 块 hash 的 base64。

### 网关每请求新增 CPU（release 实测，含读 body、解析、估算、hash、序列化、恢复原文）

| body | 耗时 |
|---|---|
| 7 KB | 53 µs |
| 69 KB | 0.4 ms |
| 221 KB | 1.3 ms |
| 690 KB | 3.4 ms |

512 QPS × 100k token 约 1.7 核，两副本分摊。

### EPP 侧对应收益

EPP 每请求从"收 690 KB → 解析 → 估算 → hash"变成"收 22 KB → 解 6 个字段 → base64 解 hash"。
实测 EPP 插件耗时：gateway-parser 85 µs，token-producer 2 µs，approx-prefix-cache-producer 61 µs
（旧路径 openai-parser 2~24 ms + producer 9~13 ms）。

## 代价与边界（必须知道）

1. **网关要缓冲整个 body。** 旧路径是流式转发，排队请求只占几十 KB；新路径必须读完才能算 hash，
   并在等 EPP 回复期间持有原文（之后要发给 vLLM）。每个在途请求 ≈ 1 份原文。
   在途数由 EPP 处理速度决定：EPP 一慢，网关内存线性涨。那轮压测 EPP 在 `indexer.Add` 锁上积压 2.5 万
   请求，两副本各 1.25 万 × 1.4 MB（第一版）≈ 17 GiB，正好 OOM；现在 1.25 万 × 690 KB ≈ 4.3 GiB，
   不 OOM 但仍无上界。**兜底靠配置**：`AgentgatewayPolicy.spec.frontend.http.maxConcurrentRequests`
   或 HTTPRoute `timeouts.request`（当前线上两个都没配 = 不限）。
2. **吞吐不一定涨。** 这项优化释放的是 EPP 的 CPU、内存和 GC；端到端 TPM 的瓶颈在 EPP
   `indexer.Add` 全局写锁，parser 从 24 ms 到 2 ms 时 TPM 没动，再降到 0.1 ms 大概率还是不动。
3. **网关和 EPP 要一起换。** 信封格式 `tokens` → `tokenCount` 两边同步改过；旧 EPP 收新信封会校验失败
   回退 openai 路径（功能不坏，下移失效）。
4. 网关 `approximateBlockSizeTokens`/`maxPrefixBlocks` 必须和 EPP producer 实际生效值一致
   （EPP 默认 autoTune 会按 pod 指标漂移），否则 EPP 静默丢弃信封 hash。

## 文件

| 文件 | 内容 |
|---|---|
| `crates/agentgateway/src/http/ext_proc/epp_prefix.rs` | 新建。`PrefixCache` 配置、`PrefixCacheComputer`、信封类型、estimate 伪 token、approximate / precise 两条 hash 链、24 个单测（含与 Go 对拍的 golden vectors） |
| `crates/agentgateway/src/http/ext_proc/epp_body.rs` | 新建。`project_for_epp`（投影 + 有信封时剥正文）/ `restore_original` |
| `crates/agentgateway/src/http/ext_proc.rs` | 开关、`InferencePoolRouter` 持有 computer、`mutate_request` 接投影、`response_body_mode=None`、`relay_response_trailers` |
| `crates/agentgateway/src/types/agent_xds.rs` 等 | 结构体补字段 |
| `Cargo.toml` | `twox-hash` 开 `xxhash64` feature |

## 验证

`cargo test -p agentgateway --lib ext_proc`：127 个测试通过。clippy / fmt 干净。
集群联调（dev182，`polaris-gateway/llmd-gateway` + `yaochen-simulator-epp`）：3 条同前缀请求，EPP 索引
5 个 pod 各 7 块，与本地按字节算出的公共前缀块数一致，hash 字节级对上。
