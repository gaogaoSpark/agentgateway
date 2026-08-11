# Fix: provider_backend 路径下 InferenceRouting policy 无法命中,EPP 被静默跳过

- **基线版本**: v1.4.1(tag `163ea214`)
- **分支**: `eppfix-v1.4.1`
- **镜像**: `image.paigpu.com/library/agentgateway:v1.4.1-eppfix`
- **改动文件**: `crates/agentgateway/src/proxy/httpproxy.rs`(1 处,+6/-2)
- **日期**: 2026-08-08(发现/修复), 2026-08-11(归档本文档)

## 现象

`AgentgatewayBackend` 使用 `custom` provider 且 `backendRef` 指向 InferencePool 时
(官方 Issue #288 / PR #1932 引入的正规用法):

```yaml
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
```

请求返回 503 `upstream call failed: Connect: Connection refused`。
EPP(endpoint picker)完全没有被调用(日志零输出);access log 显示请求被当作
普通 Service 负载均衡,直接打到了 selector 圈中 pod 的 targetPorts[0]
(本环境为 prefill pod 的 8000 端口,该端口无进程监听)。

对照:HTTPRoute 直连同一个 InferencePool 的路径,EPP 正常。

## 根因

policy 索引写入方与读取方的 key 不对称。`policies_by_target` 是精确哈希匹配
(`PolicyTargetRef` 的 `Hash`/`Eq`,见 `types/agent.rs`)。

**写入侧**(controller,`controller/pkg/agentgateway/plugins/inference_plugin.go:89`):
为 InferencePool 生成的 InferenceRouting policy,target 为:

```
Service { hostname: "warmup-ip.warmup.inference.cluster.local", port: None }
```

**读取侧**(数据面,`crates/agentgateway/src/proxy/httpproxy.rs`,
`Backend::AI` 分支的 `provider_backend` 处理):

```rust
sub_backend_policies(provider_backend.backend.target(), ...)
```

其中 `SimpleBackend::Service.target()` 返回:

```
Service { hostname: "...", port: Some(8000) }
```

`sub_backend_policies`(`store/binds.rs:1157`)只用这个带 port 的原始 key 查一次
(内部 `backend=None, sub_backend=Some(原始key)`),`Some(8000) != None` 查不到
→ `BackendPolicies.inference_routing` 为空 → `apply_inference_routing` 变 NO-OP
→ EPP 被静默跳过,请求落回 Service 兜底负载均衡。

HTTPRoute 直连 InferencePool 的路径之所以正常,是因为它走
`backend_policies()`(`store/binds.rs:1191`),该函数会**同时**用
`strip_section()` 去掉 port 的 key 和原始 key 各查一次,port=None 的 policy
能被去 port 的 key 命中。

### 官方测试为何没发现

`crates/agentgateway/src/http/ext_proc_tests.rs:2477`
(`custom_llm_provider_service_backend_runs_inference_routing`)中,测试自行构造的
policy target 写的是 `port: Some(STANDALONE_SERVICE_PORT)`,恰好与读取侧 key 一致。
测试没有覆盖"controller 真实生成的 port=None target"这一生产路径。

## 修复

`provider_backend` 分支改用与 HTTPRoute→InferencePool 路径相同的查询函数:

```diff
-				let provider_backend_policies = inputs.stores.read_binds().sub_backend_policies(
+				// Use backend_policies (not sub_backend_policies) so policies indexed without a
+				// port -- like the InferenceRouting policy the controller attaches to an
+				// InferencePool's synthesized service -- are found for the provider backend.
+				let provider_backend_policies = inputs.stores.read_binds().backend_policies(
 					provider_backend.backend.target(),
-					Some(&provider_backend.inline_policies),
+					&[&provider_backend.inline_policies],
+					None,
 				);
```

语义变化说明:`backend_policies` 除了修复 port=None 的命中问题外,还会让
Backend/Service 级(无 section/port)的其他 policy 也作用到 provider_backend 上。
这正是 InferenceRouting 与 BackendTLS(EPP 连接)所需要的语义,与直连路径行为
一致;但严格来说匹配面比原来宽,向上游提 PR 时应说明。

## 验证(集群 admin-ppio,warmup 命名空间)

修复后同一配置:

- EPP 日志完整链路:`EPP received request` → `sent request body` → `sent response body`
- access log 单条同时具备 `protocol=llm`、`inferencepool.selected_endpoint`、`gen_ai.*`
- 两条连续请求被 EPP 分到不同实例(9080/9082),证明动态选点生效
- 4 个指标 family 全部产出:`gen_ai_server_time_to_first_token`(87.7ms,需流式)、
  `gen_ai_server_time_per_output_token`(2.33ms)、`gen_ai_client_token_usage`、
  `gen_ai_server_request_duration`
- 回归:HTTPRoute 直连 InferencePool 的原路由不受影响(200)
- 延迟 A/B(15 次小请求 + 8 次 48KB prompt):与直连路径无统计学差异

完整过程见 `result.md` 第 4 轮。

## 构建

```bash
VERSION=v1.4.1-eppfix GIT_REVISION=$(git rev-parse HEAD) \
  cargo build --release --bin agentgateway
# 镜像:Ubuntu 24.04 + 单二进制,推 image.paigpu.com/library/agentgateway:v1.4.1-eppfix
# 部署:patch AgentgatewayParameters/llmd-gateway-gp 的 spec.image.tag
```

## 待办

- [ ] 上报上游 agentgateway(bug + 本补丁;并建议补一条 policy target 为
      port=None 的测试用例,以及考虑将索引 key 的匹配语义类型化以防再犯)
- [ ] 上游合入后回退到官方镜像,弃用自建 tag
