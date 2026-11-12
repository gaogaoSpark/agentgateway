# 交接：把前缀缓存 hash 计算下移到 agentgateway（EPP 只调度）

最后更新：2026-11-12

## 用户需求（原话要点）

> 我的想法更激进，
> - 在 agentgateway 把需要的 hash 算好（这样算 hash 的动作可以分摊到每个 agentgateway 实例）
> - epp 不需要 openai parse，只调度就行
> - epp 的代码（/data/code/llm-d-inference-scheduler-internal）同步改一下，
> 改代码的原则是不要大幅删减或者修改可以开一分支，或者加文件的方式

补充约束（此前已确认）：ext_proc 只给 EPP 发请求体投影；EPP 返回后只采纳 header 变更，
请求体用客户端原文；改动尽量收敛到少量文件或新建文件扩展。

## 当前状态

- **agentgateway 侧：已完成并验证。** `cargo check -p agentgateway --tests` 干净，
  `epp_prefix.rs` 的 12 个单测全过；两套 hash 算法已与 Go EPP 真实实现对拍，
  字节级一致（golden vectors 已固化进 Rust 测试 `golden_vectors_match_go_epp`）。
- **EPP 侧：已完成并验证。** `go build ./...`、`go vet`、受影响包 `go test` 全过；
  Go 单测已钉住与 Rust 相同的 5 组 golden vectors（见下）。样例配置
  `deploy/config/epp-gateway-precomputed-prefix-cache-config.yaml` 能被 `loader.LoadRawConfig`
  正常加载，所有插件类型均已注册。
- 分支：agentgateway 在 `yaochen/epp-prefix-cache-offload`（从 detached HEAD ea560864 开出），
  EPP 在 `yaochen/ext_proc_optimize`（基于 2a7d401b）。**两边都还没 commit。**

## 镜像（2026-11-12 本机构建，含当前全部未提交改动）

- agentgateway：`ghcr.io/agentgateway/agentgateway:v1.6.0-dirty`（`make docker`，Rust release 约 25 分钟）
- EPP：`ghcr.io/llm-d/llm-d-router-endpoint-picker:prefix-offload-2a7d401b-dirty`，同一 image 也打了 `:dev`
  （`make image-build-epp EPP_TAG=…`；注意变量名是 `EPP_TAG` 不是 `IMAGE_TAG`）。
  本机有外部 prune 任务会清镜像，`:dev` 被清后会从 ghcr 拉回旧的 6 月镜像，部署请用带 sha 的 tag。

## agentgateway 改动清单（/data/code/agentgateway）

| 文件 | 改动 |
|---|---|
| `crates/agentgateway/src/http/ext_proc/epp_prefix.rs` | **新建**。配置类型 `PrefixCache`（含 `approximateTokenSource`）、`PREFIX_CACHE_ENV` + `default_from_env()`、计算器 `PrefixCacheComputer`、信封 `Envelope`/`EnvelopePrompt`、approximate / precise 两条 hash 链、`estimate_tokens`/`pack_bytes`（复刻 EPP estimate 后端）、单测 |
| `crates/agentgateway/src/http/ext_proc/epp_body.rs` | `EPP_BODY_FIELDS` 增加 `cache_salt`；`project_for_epp` 增加 `prefix: Option<&PrefixCacheComputer>` 参数，算好后把信封塞进投影 |
| `crates/agentgateway/src/http/ext_proc.rs` | `EPP_BODY_PROJECTION_ENV` + `epp_body_projection_enabled()` 开关；`InferenceRouting` / `InferenceRoutingConfig` 增加 `prefix_cache`；`InferencePoolRouter` 持有 `PrefixCacheComputer`，`mutate_request` 在开关打开时走 `project_for_epp` / `restore_original`；`response_body_mode` 改为 `None`（不受开关控制）；`relay_response_trailers` 透传 trailers |
| `crates/agentgateway/src/http/ext_proc_tests.rs`、`tests/tests/dfp.rs` | 结构体字面量补 `prefix_cache: None` |
| `crates/agentgateway/src/types/agent_xds.rs` | xDS 转换 InferenceRouting 时 `prefix_cache: epp_prefix::default_from_env()` |
| 根 `Cargo.toml` | `twox-hash` 打开 `xxhash64` feature |

## 开关：`AGENTGATEWAY_EPP_BODY_PROJECTION`

环境变量，默认**关**，进程启动时读一次。

- **关**：agentgateway 原始路径。客户端 body 原样流给 EPP，EPP 回的 body 转发上游，不算 hash。
  `prefixCache` 配了也忽略并打 warn。
- **开**（`1`/`true`/`yes`/`on`）：投影 + 算 hash + 发信封 + `restore_original`。**不需要再配
  `prefixCache` 才生效**：后端没配时用进程级默认参数（approximate 开、precise 关、estimate 伪
  token、blockSize 64、maxPrefixBlocks 2048），一次 /render 都不调。

链的开关在 `prefixCache` 里：`disableApproximate` 默认 false（approximate 开），`disablePrecise`
默认 **true**（precise 关）。要开 precise 必须同时给 `tokenizerUrl`，否则校验失败。

与开关**无关**、始终生效的：`response_body_mode = None` + `response_trailer_mode = Send`，即响应
body 不发 EPP、只透传 trailers（EPP 读 `grpc-status`）。这是相对上游（FullDuplexStreamed）的
固定改动。

## 在哪里配置

默认场景（approximate-only）只需要开关本身，Deployment env：

```yaml
env:
  - name: AGENTGATEWAY_EPP_BODY_PROJECTION
    value: "1"
```

要改参数（开 precise、调 blockSize 等）有两处落点：

1. **本地 YAML**（`agentgateway -f config.yaml`）：`routes[].backends[].policies.inferenceRouting.prefixCache`，
   和 `endpointPicker` 平级，按后端分别配。参考 `examples/llm-standalone-epp/config.yaml`。
2. **Kubernetes / xDS**（agentgateway 自己的控制面 + `AgentgatewayPolicy` CRD）：proto 的
   `BackendPolicySpec.InferenceRouting`（`crates/protos/proto/resource.proto:1572`）只有
   `endpoint_picker` 和 `failure_mode`，**没有 prefixCache 字段**。兜底：环境变量
   `AGENTGATEWAY_EPP_PREFIX_CACHE` 放一段 JSON（schema 同 YAML 的 `prefixCache`），作为所有没配
   `prefixCache` 的后端的默认值（`epp_prefix::default_from_env()`，读一次，非法则打 warn 用内置默认）。
   例：`'{"disablePrecise":false,"tokenizerUrl":"http://vllm:8000"}'`。

要让控制面按后端下发，需要给 proto 加 `PrefixCache` 消息并改 agentgateway 控制面的 CRD 与转换，
这是后续事项。

## approximate 链的 token 来源（重要）

EPP 的 `token-producer` 默认后端是 **`estimate`**（无 tokenizer，把请求文本按 4 字节打包成伪
token），不配 `vllm` 后端时 approximate producer 用的就是伪 token 算 hash。网关必须用同一种来源，
否则 hash 永远对不上、短路钩子采纳后查表全 miss。

网关配置 `approximateTokenSource`：
- `estimate`（默认）：Rust 复刻 `tokenizer/estimate.go` 的 `packBytes`，本地算，**不调 /render**。
  不支持的形状 fail-open（多模态 block、chat 带 `tools`、embeddings/responses）。
- `render`：真 token，对应 EPP 配了 `vllm` 后端。

precise 链始终用 /render 真 token。两条链都开且 approximate 是 `estimate` 时，网关**不发**
approximate hash（来源不一致），只发 precise；要两条都发请把 approximate 改成 `render`。
`disablePrecise: true` + `estimate` 时网关一次 /render 都不调，`tokenizerUrl` 可以不配。

默认值已改成和 EPP 对齐：`approximateBlockSizeTokens = 64`（EPP 最小 64）、
`maxPrefixBlocks = 2048`（EPP 默认 131072/64）、`disablePrecise = true`。

estimate golden（chat: system "You are a helpful assistant." + user "Hello, 世界! 1234567"，
model "m"，bs=4）：伪 token 15 个，approximate hash
`[1538716050029008075, 2159989601878301997, 9546404140426985413]`，Rust 测试
`compute_estimate_only_matches_go_without_render` 已钉住。

## 跨界协议（agentgateway → EPP）

网关在发给 EPP 的**投影 JSON**里附加保留键 `x-agentgateway-prefix-cache`：

```json
{
  "x-agentgateway-prefix-cache": {
    "version": 1,
    "model": "…",                      // = 客户端 body 里的 model
    "cacheSalt": "…",                  // 可空
    "blockSizeTokens": 16,
    "hashAlgorithm": "cbor-fnv",       // 或 "xxh64"
    "hashSeed": "",
    "approximateBlockSizeTokens": 16,
    "maxPrefixBlocks": 128,
    "prompts": [{
      "tokenCount":        1234,                   // 必有；EPP 所有消费者只读 TokenCount()
      "tokens":            "<base64 LE u32…>",     // 默认不发；shipTokens: true 才发
      "approximateHashes": "<base64 LE u64…>",   // 可空
      "preciseKeys":       "<base64 LE u64…>"    // 可空
    }]
  }
}
```

- u64 hash / u32 token 一律 **base64 小端字节**，绝不用 JSON number（64 位精度）。
- **默认只发 hash + tokenCount，不发 tokens**（2026-11-12 改）。estimate 伪 token 就是 prompt 字节，
  base64 后 1.33× prompt，发它会让 EPP 收到的 body 变成原文的 2.38×。EPP 侧没有任何插件读 token 值，
  全部只读 `TokenCount()`。EPP `TokenizedRequest()` 用零占位切片撑出长度；参数不匹配且无 token 时
  钩子返回**空非 nil**（"本请求无前缀块"），绝不拿占位算 hash。`shipTokens: true` 恢复旧行为。
- 信封 `model`（hash 种子）和 render 请求的 model 都用**客户端 body 里的 `model`**，这样一个
  网关配置能服务多个模型。没有 `tokenizerModel` 配置项。
- render 走 `POST {tokenizerUrl}/v1/chat/completions/render` 与 `/v1/completions/render`，
  回归结构是 `{token_ids, features{mm_hashes, mm_placeholders}}`；多模态一律 fail-open。
- 任何失败只打 warn，发不带信封的投影，EPP 自己兜底。

## 对拍基准（tokens=1..=10，block=4，model="m"）

- approximate：`[18390267952832273189, 15051379317946961216]`
- approximate + salt "s"：`[2832891143819933408, 7734888222984014360]`
- precise (cbor-fnv)：`[14686617092727577033, 9856792211163113858]`
- precise + salt "s"：`[18255695703984473629, 7912881359976539370]`
- precise xxh64，seed="seed" **且 salt="s"**：`[17109699452489004063, 9607951923782221908]`

## EPP 改动清单（/data/code/llm-d-inference-scheduler-internal）

module `github.com/llm-d/llm-d-router`。对已有文件只有 **17 行纯追加、0 删除**；其余全是新文件。

| 文件 | 改动 |
|---|---|
| `pkg/epp/framework/interface/requesthandling/precomputed.go` (+test) | **新建**。`PrecomputedEnvelopeField`、`Precomputed`/`PrecomputedPrompt`（带 json tag，直接是 wire 形状）、`LEUint32s`/`LEUint64s`（自定义 `UnmarshalJSON`：base64 → 小端 → 切片，空串 = nil）、`ParsePrecomputed(json.RawMessage)`（sonic 解码 + 版本/长度校验）、`TokenizedRequest()`、`HasApproximate()`/`HasPrecise()`/`Matches(model, salt)` |
| `pkg/epp/framework/interface/requesthandling/types.go` | `TokenizedRequest` 追加字段 `Precomputed *Precomputed` |
| `pkg/epp/framework/plugins/requesthandling/parsers/gateway/gateway.go` (+test) | **新建** `gateway-parser`。`Claims()` 直接返回内嵌 openai 的。`ParseRequest` 用 **sonic 一次解码**到 `gatewayBody{model, stream, max_tokens*, Envelope json.RawMessage}`：有信封 → `ParsePrecomputed` 填 `TokenizedRequest`，`Payload = RawPayload(body)`、`Mutated=false`，**不建 map、不建类型化结构**；无信封 → 原样交给 openai parser（tokenizer 自己 render）；信封非法 → warning + openai 路径，并从 map 里删掉信封键 |
| `pkg/epp/framework/plugins/requestcontrol/dataproducer/prefixhash/precomputed.go` (+test) | **新建** `precomputedBlockHashes(ctx, request, blockSize, maxBlocks)`；`hashing.go` 的 `GetBlockHashes` 在空检查之后追加 3 行短路 |
| `pkg/epp/framework/plugins/requestcontrol/dataproducer/preciseprefixcache/blockkeys_precomputed.go` (+test) | **新建** `preciseChainParams{blockSizeTokens, hashAlgorithm, hashSeed}`、`newPreciseChainParams(cfg, bs)`、`precomputedBlockKeys(ctx, request, params)`；`producer.go` 追加字段 `preciseChain`、`New()` 里一行初始化、`Produce()` 里 3 行短路（命中直接 `produceFromBlockKeys(..., keys, nil)`） |
| `cmd/epp/runner/runner.go` | 追加 import + `fwkplugin.Register(gateway.ParserType, …)` |
| `parsers/gateway/gateway.go` 的 `rawPayload` | 快路径 payload 从 `fwkrh.RawPayload` 换成私有 `rawPayload struct{ fwkrh.RawPayload }` + `Marshal()` 返回原字节。原因：`director.modelRewriteIfNeeded` 先断言 `MarshalablePayload` 再填 `TargetModelName`，裸 `RawPayload` 导致 `TargetModelName`/`SchedulingRequest.TargetModel` 为空，采纳条件 `Precomputed.Model == TargetModel` 永远不成立（联调日志 `targetModelName:""` + "payload does not implement MarshalablePayload" 证实）。 |
| `parsers/openai/openai.go` + `openai_decode.go`、`openai_chat_fastpath.go`（+3 个 test、2 个 bench） | **从 `/data/llm-d-inference-scheduler-internal` 合并的 openai-parser 优化**：body map 用 sonic 解码（失败回退 encoding/json，语义不变）；chat completions 从已解码的 map 直接构造 `ChatCompletionsRequest`，省掉第二次全文解码（形状不匹配回退原路径）。`types.go` 的 `Content.UnmarshalJSON` 遇 `[` 直接走 block 分支。实测 100k 词 chat body 31.6 ms → 1.14 ms（28×），1k 词 372 µs → 33 µs。给没信封的请求（gateway-parser 回退、anthropic 等）兜底用。 |
| `deploy/config/epp-gateway-approximate-prefix-cache-config.yaml` | **新建**样例（**默认场景**：approximate-only、estimate 伪 token、无 /render）。`gateway-parser` 替代 openai-parser；`approximate-prefix-cache-producer` 显式 `autoTune: false, blockSizeTokens: 64, maxPrefixTokensToMatch: 131072`，与网关默认 64 / 2048 对齐 |
| `deploy/config/epp-gateway-precomputed-prefix-cache-config.yaml` | **新建**样例（precise 场景，需要网关 `disablePrecise: false` + `tokenizerUrl`），其余与默认链一致（anthropic / vllmhttp / passthrough） |

### 采纳条件（任一不符即回退本地计算，debug 日志含 "recomputing"）

- 两条链共同：`Precomputed.Model == request.TargetModel`、`Precomputed.CacheSalt == TokenizedRequest.CacheSalt`、
  prompt 数一致、每个 prompt 的 token 数一致、无 `MultiModalFeatures`。
- approximate：`ApproximateBlockSizeTokens == blockSize`、`MaxPrefixBlocks == maxBlocks`（都是调用方传入的实际值）。
- precise：`BlockSizeTokens`、`HashAlgorithm`（空视为 `cbor-fnv`）、`HashSeed` 与 `tokenProcessorConfig` 一致。

### gateway-parser 快路径的两个前提（必须满足）

1. **只能配在 agentgateway 后面。** 快路径下 EPP 不重新序列化 body（`RawPayload` + `Mutated=false`），
   信封原样留在 EPP 回传的 body 里。agentgateway 的 `restore_original` 会丢掉 EPP 的 body、换回客户端
   原文，所以 vLLM 收不到信封。换成会转发 EPP body 的网关，信封就会漏给 vLLM。
2. **EPP 的 model 改写不会落到 body。** `RewriteModelName` 遇到非 `PayloadMap` 直接跳过。同样因为
   `restore_original`，这在 agentgateway 下本来也不生效，没有行为变化。

快路径下 `Body.ChatCompletions` / `Body.Completions` 为 nil，`Payload` 不是 map：`hashcache` 系列、
`preciseprefixcache/legacy_producer` 等直接读这些字段的插件不能和 gateway-parser 同时配。

### parser 性能（amd64 单核，chat body 含 messages 文本 + 信封）

| tokens | body | gateway 快路径 | openai-parser | 倍数 |
|---|---|---|---|---|
| 512 | 5 KB | 19 µs / 27 allocs | 123 µs / 86 allocs | 6.4× |
| 4096 | 41 KB | 105 µs / 27 | 810 µs / 89 | 7.7× |
| 32768 | 328 KB | 688 µs / 27 | 6.0 ms / 92 | 8.7× |

sonic 已是仓库直接依赖（sidecar 在用），没有新增依赖。

### 关于 parser 选择的更正

交接文档早先说「必须在 runner.go 里把 gateway 注册在 openai 之前」—— **这是错的**。
`fwkplugin.Register` 只是登记工厂；真正决定用哪个 parser 的是配置里的 `requestHandler.parsers`
（没配时 `ensureParsers` 默认 openai→anthropic→vllmhttp→passthrough）。所以用法是
**在配置里用 `gateway-parser` 替换 `openai-parser`**，两者不要同时配。

## EPP 侧原始方案（已按此实现，留作对照）

原则：**只加文件 / 加字段，不改现有逻辑的主干**。

1. **新建** `pkg/epp/framework/interface/requesthandling/precomputed.go`
   - `const PrecomputedEnvelopeField = "x-agentgateway-prefix-cache"`、版本、算法常量（复用
     `kvblock.HashAlgorithmCBORFNV` / `HashAlgorithmXXH64` 的字符串值）。
   - `type Precomputed struct { … }` + `PrecomputedPrompt`，字段与上面信封一一对应；
     `Prompts []PrecomputedPrompt` 里分别放解码后的 `TokenIDs`、`ApproximateHashes`、
     `PreciseKeys`。
   - `func ParsePrecomputed(raw map[string]any) (*Precomputed, error)`：做 base64/LE 解码、
     版本与长度校验（hash 数量 × blockSize + 余数 与 token 数一致）。
   - 校验辅助：`Precomputed.Accepts(model, cacheSalt string, blockSize int) bool`。
2. **加字段** `TokenizedRequest.Precomputed *Precomputed`（types.go，纯追加，`json:"-"` 由
   所在结构体已处理）。tokenizer 插件的 `adoptsPrepopulatedBody()` 路径会原样保留该结构体，
   不用改 tokenizer。
3. **新建** `pkg/epp/framework/plugins/requesthandling/parsers/gateway/`
   - 仿 `vllmhttp`：内嵌 `openai.OpenAIParser`，`Claims()` 用与 openai 完全相同的 path 列表
     （即注册后靠「同长度后缀」不覆盖 openai —— **注意**：`NewParserRegistry` 是
     「更长后缀优先，同长度先注册者胜」，所以必须在 `runner.go` 里把 gateway 版
     **注册在 openai 之前**，否则永远命中 openai）。
   - `ParseRequest`：先调内嵌 openai 的 `ParseRequest`，然后从 `Payload`（`fwkrh.PayloadMap`）
     取出 `x-agentgateway-prefix-cache` 键 → `ParsePrecomputed` → 填 `Body.TokenizedRequest`
     （tokens 直接来自信封，不再需要 render）+ `Body.TokenizedRequest.CacheSalt`；
     **从 PayloadMap 里删掉该键**，避免被转发给推理后端。
     解析失败只打日志、继续（EPP 走原来的 tokenize 路径）。
   - `ParseResponse` / `RewriteModelName` 直接委托内嵌 openai。
4. **早期返回钩子（加文件式）**
   - `prefixhash`：新建 `pkg/epp/framework/plugins/requestcontrol/dataproducer/prefixhash/precomputed.go`，
     在 `GetBlockHashes` 开头加 3 行：`if h := precomputedBlockHashes(request, blockSizeTokens, maxPrefixBlocks); h != nil { return h }`
     （同包新文件，不动 hashing.go 的主体）。
   - `preciseprefixcache`：新建 `blockkeys_precomputed.go`，同理在 `computeBlockKeys` 开头短路；
     `Producer` 需要把 `tokenProcessor` 的 hash 算法也存下来（`blockSizeTokens` 已有），才能校验
     `hashAlgorithm` 一致；这是 producer.go 里唯一的一行加法。
   - 校验条件：`version == 1`、`model == request.TargetModel`、`cacheSalt` 一致、`blockSizeTokens`
     一致、`maxPrefixBlocks` 一致、算法一致、prompt 数量与 tokens 数量一致、MM 特征为空。
     任一不符 → 返回 nil，走原有计算。
5. **注册 + 样例**：`cmd/epp/runner/runner.go` 在 openai 之前加
   `fwkplugin.Register(gateway.ParserType, fwkplugin.StabilityBeta, gateway.ParserPluginFactory)`；
   `deploy/config/` 下加一份启用它的样例 config。

## 关键结论（侦察已确认）

- `fwkrh.InferenceRequestBody.TokenizedRequest *TokenizedRequest`（types.go:119）——
  parser 填好它，tokenizer 插件的 `Produce` 会直接采用，不再调 `/render`。
- `TokenizedRequest{CacheSalt string, Prompts []PromptTokens}`；`PromptTokens{TokenIDs []uint32, MultiModalFeatures []MultiModalFeature}`。
- `prefixhash.GetBlockHashes(ctx, request, blockSizeTokens, maxPrefixBlocks)` 用
  `request.TargetModel` + `tp.CacheSalt` 做种子；`BlockHash` = `xxhash.Sum64(LE u32 bytes)`；
  链式 `xxh64(LE64(blockID) ‖ LE64(prev))`；截断到整块、最多 `maxPrefixBlocks` 块。
- `preciseprefixcache` 的 `computeBlockKeys(ctx, idx, request, blockSizeTokens)` 逐 prompt 调
  `computeBlockKeysForTokens(...)` → `idx.ComputeBlockKeysFromTokens`；salt 走
  `foldCacheSalt` 塞进 block 0 的 `MMHashes`（`kvblock.MMHash{Hash: salt}`）。
- `kvblock.BlockHash` 是 `uint64`；算法常量 `kvblock.HashAlgorithmCBORFNV = "cbor-fnv"`、
  `HashAlgorithmXXH64 = "xxh64"`。
- `Producer` 已持有 `blockSizeTokens`（= `tokenProcessor.BlockSize()`），但没有算法字段。

## 性能分析：为什么下移后"反而慢"（2026-11-12）

压测现象：网关换成投影镜像后 TPM 不升反降，两个网关 pod 在 16Gi 上限下 OOMKilled（22 分钟各重启 4~5 次），
EPP 日志出现 `error sending response`（对端已断，就是网关被杀）。旧镜像 `v1.6.0-dirty-nobody` 同压测无 OOM。

**CPU 不是瓶颈。** release 实测投影路径每请求开销（估算 + hash + 序列化 + 恢复）：

| body | 投影耗时 | 投影+hash（只发 hash） | 旧：投影+hash（发 tokens） |
|---|---|---|---|
| 7 KB | 26 µs | 59 µs | — |
| 69 KB | 0.3 ms | 0.6 ms | — |
| 221 KB | 1.0 ms | 1.5 ms | — |
| 690 KB (100k token) | 1.7 ms | 4.5 ms | ~2× |

512 QPS × 100k token 约 2.3 核，摊到两个副本，相对 32 核上限可忽略。

**内存是瓶颈，且是结构性的。** 旧路径网关对 body 是**流式**的：读一块发一块，排队的请求只占几十 KB。
新路径必须 `inspect_body` 把整个 body 读进内存才能算 hash，然后在 EPP 往返期间同时持有：
原始 body（等 EPP 回来后 `restore_original` 发给 vLLM）+ 投影后的 body（发给 EPP）。
发 tokens 时投影是 2.38× 原文，每个在途请求约 3.4× body；改成只发 hash 后约 2× body。
100k token 请求 690 KB，每个在途请求从旧路径的几十 KB 变成 1.4~2.3 MB，**放大 30~50 倍**。

在途请求数由 EPP 的处理速度决定。`optimize.md` 实测 EPP 在 `indexer.Add` 全局写锁上积压 2.5 万个
goroutine，对应网关侧就有同量级请求在等 EPP 回 header，两个副本各分一半。
按**每副本**算：1.25 万 × 1.4 MB（发 tokens 时）≈ 17 GiB，正好打到 16Gi。
剥正文后每在途 ≈ 1 份原文：1.25 万 × 690 KB ≈ 4.3 GiB/副本，暂时不 OOM，但 2.5 万只是那轮压测的快照，
EPP 卡得更久就更多，仍无上界。
所以 OOM 是 EPP 锁慢 → 网关在途堆积 → 每个在途请求比以前大 30 倍 三者叠加，不是单一改动的锅。

**用户两点看法的核对：**
1. "EPP 返回、转发下游时 body 就可以释放" —— 代码里就是这样：`restore_original` 把原始 body 装回请求，
   投影 body 随之 drop；原始 body 流向 vLLM 后释放。但这是在途请求的**结束**时刻，问题在于 EPP 回得慢，
   结束时刻被推后。要提前释放只能不算 hash（即不开投影），没有中间态。
2. "EPP 单节点能扛住，分摊到多个网关压力应该更小" —— CPU 维度成立（上表证实）。内存维度不成立：
   EPP 本来也 buffer 整个 body（`RequestBody.EndOfStream` 才处理），旧路径是 EPP 一份；新路径是网关两份
   （原始 + 投影）+ EPP 一份，总量变成三份，只是其中两份换了地方。网关副本多了分摊的是份数，不是倍数。

**应对（按顺序）：**
1. 只发 hash + tokenCount（已做）：投影 2.38× → 1.03×。
1b. **有信封时剥掉正文**（已做，2026-11-12 晚）：`epp_body.rs` 新增 `EPP_ENVELOPE_FIELDS`
   = `model/stream/max_tokens/max_completion_tokens/max_output_tokens/cache_salt`，hash 算出来后
   `fields.retain(...)` 只留这 6 个标量 + 信封；算不出来（多模态、tools 等 fail-open）才保留
   `messages/prompt/tools` 让 EPP 自己算。EPP gateway-parser 快路径只读这 6 个字段 + 信封，已核实
   （`gatewayBody` 结构体 + director 只读 `Body.Model/Stream/MaxOutputTokens/Payload`）。
   实测发给 EPP 的 body：7 KB → 621 B，69 KB → 3.2 KB，690 KB → 22 KB（**0.03×**，只剩 hash 数组）。
   在途请求内存从 2× body 回到 ≈1× body（只剩等 EPP 期间持有的原始 body）。
2. 真正的根因是 EPP `indexer.Add` 锁，网关侧再怎么压都只是缩小放大系数。用户的 sharded indexer /
   `optimize.md` 4.1 有界队列落地后，在途数掉下来，内存问题自然消失。
3. **OOM 的直接解法（配置，不改代码）**。现状核实：HTTPRoute `timeouts.request: 0s`（= 不限时），
   `default-policy` 的 `frontend.http` 没配 `maxConcurrentRequests`（= 不限并发）。两个都不限，
   EPP 一慢在途就无上界，每个在途至少持一份 body，必然打到 limit。两处任选或都配：
   - `AgentgatewayPolicy default-policy` → `spec.frontend.http.maxConcurrentRequests: N`（CRD 已有该字段，
     实现在 `proxy/gateway.rs:962`，超限直接 503 shed，不进入 body 读取）。N 按内存算：
     `limit / 平均 body` 留余量，16Gi、690 KB body 取 8000 左右；在 EPP 健康时 512 QPS × 100 ms ≈ 50 在途，
     8000 远够用，只在 EPP 卡死时起作用。
   - HTTPRoute `timeouts.request` 从 `0s` 改成有限值（如 `60s`）。`request_timeout` 包住整个
     `make_backend_call`（含 EPP 往返，`httpproxy.rs:1626`），超时后 body 随请求 drop。
     注意这个超时也覆盖 vLLM 生成时间，长输出要给够。
   两者都只是让"EPP 慢"表现为快速失败而不是网关 OOM，不提升吞吐。
4. 下一轮优化点（未做）：`project` 走 `serde_json::Value` 全树解析再序列化，100k token 要 1.7 ms；
   换成 `RawValue` 按字段切片能省一半。只在 CPU 成为瓶颈时才值得做。

## 待办 / 下一步

1. **commit**：两边分支各提一个 commit（用户尚未要求，没有自动提交）。
   agentgateway 侧还要补 `relay_response_trailers` 的测试（trailers 到 ext_proc、mutation 回客户端、
   ext_proc 不回时原 trailers 仍到达）。
2. 端到端联调：网关 + EPP，确认 EPP 不再打 `/render`（看 EPP 日志 / vLLM access log），
   且调度结果与未下发时一致；验证转发给 vLLM 的 body 里没有 `x-agentgateway-prefix-cache`。
3. approximate 链对齐陷阱：`approximate-prefix-cache-producer` 默认 `autoTune: true`，blockSize 取
   pod 指标 `CachePrefixMatchUnit`/`CacheBlockSize`（下限 64），maxBlocks = `maxPrefixTokensToMatch`(131072)/blockSize。
   pod 上报 16 时正好落到 64/2048 与网关默认一致，但上报 128 就会漂成 128/1024 导致永远回退。
   样例里已把 `autoTune: false` 钉死；要改 blockSize 两边同时改。
4. （更早的遗留，优先级低）trailer 透传测试（`response_body_mode = None`）、镜像重建、
   `AgentgatewayPolicy`（`default-policy`，Gateway `llmd-gateway`）配置落点。
