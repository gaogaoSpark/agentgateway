//! Prefix-cache precomputation for the endpoint picker (EPP).
//!
//! Instead of having the EPP tokenize every request (vLLM `/render`) and hash the token
//! blocks itself, the gateway does both and ships the results inside the body projection
//! sent to the EPP (see [`super::epp_body`]). The EPP then only schedules. The work is
//! spread across gateway instances instead of concentrating on the single EPP.
//!
//! Two hash chains are implemented, byte-for-byte compatible with the llm-d EPP:
//! - the "approximate" chain used by `approximate-prefix-cache` / `burst-prefix` producers
//!   (`xxh64` over `[model ‖ salt]`, then `xxh64(LE64(blockId) ‖ LE64(prev))`), and
//! - the "precise" chain used by `precise-prefix-cache-producer` (`kvblock` token
//!   processor: FNV-1a64 over canonical CBOR `[parent, tokens, extra]`, or the `xxh64`
//!   variant).
//!
//! The approximate chain does not need real tokens. The EPP's default `token-producer`
//! backend is `estimate`, which packs the request text into pseudo-tokens (4 bytes each)
//! without any tokenizer; the gateway reproduces that packing locally
//! ([`ApproximateTokenSource::Estimate`]) so the approximate chain costs no network call.
//! The precise chain must match vLLM's KV blocks and therefore always uses real tokens
//! from `/render`.
//!
//! Every failure (render endpoint unreachable, multimodal prompt, bad JSON) degrades to
//! "no precomputed data": the projection is sent without the envelope and the EPP falls
//! back to its own tokenization/hashing.

use std::hash::Hasher;
use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::*;

/// Key under which the envelope is embedded in the projected request body. The EPP's
/// gateway-aware parser reads it; the stock OpenAI parser ignores unknown keys.
pub const ENVELOPE_FIELD: &str = "x-agentgateway-prefix-cache";
/// Envelope protocol version. Bump on incompatible changes; the EPP ignores unknown versions.
pub const ENVELOPE_VERSION: u32 = 1;

pub const HASH_ALGORITHM_CBOR_FNV: &str = "cbor-fnv";
pub const HASH_ALGORITHM_XXH64: &str = "xxh64";

/// Optional environment variable holding a JSON [`PrefixCache`] that overrides the
/// defaults for backends without an explicit `prefixCache` (all xDS backends, since the
/// control plane has no field for it). Not needed for the default approximate-only setup;
/// use it to enable the precise chain or tune block parameters, for example
/// `AGENTGATEWAY_EPP_PREFIX_CACHE='{"disablePrecise":false,"tokenizerUrl":"http://vllm:8000"}'`.
pub const PREFIX_CACHE_ENV: &str = "AGENTGATEWAY_EPP_PREFIX_CACHE";

/// The process-wide [`PrefixCache`] used when a backend has none configured: the parsed
/// [`PREFIX_CACHE_ENV`] if set and valid, otherwise [`PrefixCache::default`]. Read once.
/// An invalid value is logged at warn and the default is used.
pub fn default_from_env() -> Arc<PrefixCache> {
	static DEFAULT: std::sync::OnceLock<Arc<PrefixCache>> = std::sync::OnceLock::new();
	DEFAULT
		.get_or_init(|| {
			let raw = match std::env::var(PREFIX_CACHE_ENV) {
				Ok(v) if !v.trim().is_empty() => v,
				_ => return Arc::new(PrefixCache::default()),
			};
			match serde_json::from_str::<PrefixCache>(&raw).map_err(anyhow::Error::from) {
				Ok(pc) => match pc.validate() {
					Ok(()) => {
						info!("{PREFIX_CACHE_ENV} set: overriding default EPP prefix-cache parameters");
						Arc::new(pc)
					},
					Err(e) => {
						warn!("ignoring {PREFIX_CACHE_ENV}, using defaults: {e}");
						Arc::new(PrefixCache::default())
					},
				},
				Err(e) => {
					warn!("ignoring {PREFIX_CACHE_ENV}, using defaults: invalid JSON: {e}");
					Arc::new(PrefixCache::default())
				},
			}
		})
		.clone()
}

const DEFAULT_BLOCK_SIZE_TOKENS: usize = 16;
/// EPP `approx-prefix-cache-producer` clamps anything below 64 up to 64 (llm-d #1158).
const DEFAULT_APPROXIMATE_BLOCK_SIZE_TOKENS: usize = 64;
/// EPP default `maxPrefixTokensToMatch` (131072) / `blockSizeTokens` (64).
const DEFAULT_MAX_PREFIX_BLOCKS: usize = 2048;
const DEFAULT_RENDER_TIMEOUT: Duration = Duration::from_secs(5);
/// `tokenizer.bytesPerToken` in the EPP estimate backend.
const ESTIMATE_BYTES_PER_TOKEN: usize = 4;

fn default_true() -> bool {
	true
}
fn default_block_size() -> usize {
	DEFAULT_BLOCK_SIZE_TOKENS
}
fn default_approximate_block_size() -> usize {
	DEFAULT_APPROXIMATE_BLOCK_SIZE_TOKENS
}
fn default_max_prefix_blocks() -> usize {
	DEFAULT_MAX_PREFIX_BLOCKS
}
fn default_hash_algorithm() -> String {
	HASH_ALGORITHM_CBOR_FNV.to_string()
}
fn default_render_timeout() -> Duration {
	DEFAULT_RENDER_TIMEOUT
}

/// Configuration for computing prefix-cache hashes in the gateway.
#[apply(schema!)]
pub struct PrefixCache {
	/// Base URL of a vLLM server exposing `/v1/completions/render` and
	/// `/v1/chat/completions/render` (for example `http://vllm-render:8000`). Only used by
	/// the precise chain and by `approximateTokenSource: render`.
	#[serde(default, skip_serializing_if = "String::is_empty")]
	pub tokenizer_url: String,
	/// Where the approximate chain gets its tokens. Must match the EPP `token-producer`
	/// backend: `estimate` (EPP default, no tokenizer, 4-byte pseudo-tokens) or `render`
	/// (EPP `vllm` backend, real tokens).
	#[serde(default)]
	pub approximate_token_source: ApproximateTokenSource,
	/// Optional bearer token for a vLLM started with `--api-key`.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub tokenizer_api_key: Option<String>,
	/// Timeout for each render call.
	#[serde(default = "default_render_timeout", with = "serde_dur")]
	pub render_timeout: Duration,
	/// Tokens per KV block. Must match the EPP's `blockSizeTokens` and vLLM's block size.
	#[serde(default = "default_block_size")]
	pub block_size_tokens: usize,
	/// Block size of the approximate chain. Must match the EPP producer's effective block
	/// size (its `blockSizeTokens`, or the pod's metric when `autoTune` is on; never below 64).
	#[serde(default = "default_approximate_block_size")]
	pub approximate_block_size_tokens: usize,
	/// Maximum number of prefix blocks hashed by the approximate chain. Must match the EPP
	/// producer's `maxPrefixTokensToMatch / blockSizeTokens` (or `maxPrefixBlocksToMatch`).
	#[serde(default = "default_max_prefix_blocks")]
	pub max_prefix_blocks: usize,
	/// Seed of the precise chain. Must match the EPP's `tokenProcessorConfig.hashSeed`.
	#[serde(default)]
	pub hash_seed: String,
	/// Precise chain algorithm: `cbor-fnv` (default) or `xxh64`. Must match the EPP.
	#[serde(default = "default_hash_algorithm")]
	pub hash_algorithm: String,
	/// Disable the approximate chain (saves bytes when only the precise producer is used).
	#[serde(default, skip_serializing_if = "crate::serdes::is_default")]
	pub disable_approximate: bool,
	/// Disable the precise chain. On by default: the precise chain needs vLLM `/render`
	/// (`tokenizerUrl`) and only pays off with `precise-prefix-cache-producer` on the EPP.
	#[serde(default = "default_true", skip_serializing_if = "Clone::clone")]
	pub disable_precise: bool,
	/// Also ship the token ids in the envelope. Off by default: every EPP consumer only
	/// reads the token count, and with `estimate` tokens the ids are the prompt bytes
	/// again (1.33x the prompt once base64-encoded), so shipping them more than doubles
	/// the body sent to the EPP. Turn on only to let the EPP recompute hashes locally
	/// when its parameters do not match the gateway's.
	#[serde(default, skip_serializing_if = "crate::serdes::is_default")]
	pub ship_tokens: bool,
}

impl Default for PrefixCache {
	/// Approximate chain only, estimate pseudo-tokens, EPP-aligned block parameters. This is
	/// what the projection channel uses when no `prefixCache` is configured.
	fn default() -> Self {
		serde_json::from_str("{}").expect("empty PrefixCache object deserializes")
	}
}

/// Token source for the approximate chain. Mirrors the EPP `token-producer` backends.
#[apply(schema!)]
#[derive(Default, Copy, PartialEq, Eq)]
pub enum ApproximateTokenSource {
	/// Pack request bytes into 4-byte pseudo-tokens locally (EPP `estimate` backend).
	#[default]
	Estimate,
	/// Real tokens from vLLM `/render` (EPP `vllm` backend).
	Render,
}

impl PrefixCache {
	pub fn validate(&self) -> anyhow::Result<()> {
		if self.block_size_tokens == 0 {
			anyhow::bail!("blockSizeTokens must be greater than 0");
		}
		if self.approximate_block_size_tokens == 0 {
			anyhow::bail!("approximateBlockSizeTokens must be greater than 0");
		}
		if self.disable_approximate && self.disable_precise {
			anyhow::bail!("disableApproximate and disablePrecise cannot both be set");
		}
		if self.hash_algorithm != HASH_ALGORITHM_CBOR_FNV && self.hash_algorithm != HASH_ALGORITHM_XXH64
		{
			anyhow::bail!(
				"unsupported hashAlgorithm {:?}: accepted values are {:?} and {:?}",
				self.hash_algorithm,
				HASH_ALGORITHM_CBOR_FNV,
				HASH_ALGORITHM_XXH64
			);
		}
		if self.needs_render() && self.tokenizer_url.is_empty() {
			anyhow::bail!(
				"tokenizerUrl is required unless disablePrecise is set and approximateTokenSource is estimate"
			);
		}
		Ok(())
	}

	fn approximate_block_size(&self) -> usize {
		self.approximate_block_size_tokens
	}

	/// Whether any configured chain needs real tokens from `/render`.
	fn needs_render(&self) -> bool {
		!self.disable_precise
			|| (!self.disable_approximate
				&& self.approximate_token_source == ApproximateTokenSource::Render)
	}
}

/// Runtime state: parsed config plus a shared HTTP client for the render endpoint.
#[derive(Debug, Clone)]
pub struct PrefixCacheComputer {
	config: Arc<PrefixCache>,
	client: reqwest::Client,
	precise_init: u64,
}

/// Wire format of the envelope. All `u64` hashes and `u32` tokens are base64 of their
/// little-endian byte representation; JSON numbers cannot carry 64-bit hashes exactly.
#[derive(Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Envelope {
	pub version: u32,
	pub model: String,
	#[serde(default, skip_serializing_if = "String::is_empty")]
	pub cache_salt: String,
	pub block_size_tokens: usize,
	pub hash_algorithm: String,
	pub hash_seed: String,
	pub approximate_block_size_tokens: usize,
	pub max_prefix_blocks: usize,
	pub prompts: Vec<EnvelopePrompt>,
}

#[derive(Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct EnvelopePrompt {
	/// Number of tokens hashed. This is all the EPP needs for length-based scoring.
	pub token_count: usize,
	/// Token ids as base64(LE u32…). Empty unless `shipTokens` is set.
	#[serde(default, skip_serializing_if = "String::is_empty")]
	pub tokens: String,
	/// Approximate chain block hashes as base64(LE u64…). Empty when disabled.
	#[serde(default, skip_serializing_if = "String::is_empty")]
	pub approximate_hashes: String,
	/// Precise chain block keys as base64(LE u64…). Empty when disabled.
	#[serde(default, skip_serializing_if = "String::is_empty")]
	pub precise_keys: String,
}

#[derive(Deserialize)]
struct RenderResponse {
	#[serde(default)]
	token_ids: Vec<u32>,
	#[serde(default)]
	features: Option<RenderFeatures>,
}

#[derive(Deserialize)]
struct RenderFeatures {
	#[serde(default)]
	mm_hashes: Map<String, Value>,
	#[serde(default)]
	mm_placeholders: Map<String, Value>,
}

impl RenderFeatures {
	fn is_multimodal(&self) -> bool {
		!self.mm_hashes.is_empty() || !self.mm_placeholders.is_empty()
	}
}

impl PrefixCacheComputer {
	pub fn new(config: Arc<PrefixCache>) -> anyhow::Result<Self> {
		config.validate()?;
		let client = reqwest::Client::builder()
			.timeout(config.render_timeout)
			// vLLM's render endpoint is served over HTTP/1.1.
			.http1_only()
			.build()?;
		let precise_init = if config.hash_algorithm == HASH_ALGORITHM_XXH64 {
			xxh64(config.hash_seed.as_bytes())
		} else {
			fnv1a64(config.hash_seed.as_bytes())
		};
		Ok(Self {
			config,
			client,
			precise_init,
		})
	}

	/// Compute the envelope for a projected OpenAI-style body (`messages` or `prompt`).
	/// Returns `Ok(None)` when the request shape is not supported (multimodal, embeddings,
	/// responses API, ...). Transport and decode errors are returned so the caller can log
	/// them and fall back.
	///
	/// The envelope's `tokens` carry the token ids the EPP's `token-producer` would have
	/// produced itself: real tokens when any chain needs `/render`, otherwise the estimate
	/// backend's pseudo-tokens. The two sources are never mixed in one envelope.
	pub async fn compute(&self, body: &Map<String, Value>) -> anyhow::Result<Option<Envelope>> {
		let cfg = &self.config;
		// Hash seed is the client's model name: the EPP seeds with `request.TargetModel`,
		// which is the incoming body model unless an InferenceModel rewrite is configured.
		// `tokenizerModel` is only what we stamp into /render calls.
		let model = body
			.get("model")
			.and_then(Value::as_str)
			.unwrap_or_default()
			.to_string();
		let cache_salt = body
			.get("cache_salt")
			.and_then(Value::as_str)
			.unwrap_or_default()
			.to_string();

		let prompts = if cfg.needs_render() {
			self.render_tokens(body).await?
		} else {
			estimate_tokens(body)
		};
		let Some(prompts) = prompts else {
			return Ok(None);
		};
		let prompts: Vec<Vec<u32>> = prompts.into_iter().filter(|p| !p.is_empty()).collect();
		if prompts.is_empty() {
			return Ok(None);
		}
		// The approximate chain can only be shipped when its tokens come from the same
		// source the EPP producer will see. With `estimate` configured but `/render` used
		// for the precise chain, the approximate hashes would never match the EPP index.
		let ship_approximate = !cfg.disable_approximate
			&& (cfg.approximate_token_source == ApproximateTokenSource::Render || !cfg.needs_render());

		let envelope_prompts = prompts
			.iter()
			.map(|tokens| EnvelopePrompt {
				token_count: tokens.len(),
				tokens: if cfg.ship_tokens {
					encode_u32s(tokens)
				} else {
					String::new()
				},
				approximate_hashes: if ship_approximate {
					encode_u64s(&approximate_block_hashes(
						tokens,
						&model,
						&cache_salt,
						cfg.approximate_block_size(),
						cfg.max_prefix_blocks,
					))
				} else {
					String::new()
				},
				precise_keys: if cfg.disable_precise {
					String::new()
				} else {
					encode_u64s(&self.precise_block_keys(tokens, &model, &cache_salt))
				},
			})
			.collect();

		Ok(Some(Envelope {
			version: ENVELOPE_VERSION,
			model,
			cache_salt,
			block_size_tokens: cfg.block_size_tokens,
			hash_algorithm: cfg.hash_algorithm.clone(),
			hash_seed: cfg.hash_seed.clone(),
			approximate_block_size_tokens: cfg.approximate_block_size(),
			max_prefix_blocks: cfg.max_prefix_blocks,
			prompts: envelope_prompts,
		}))
	}

	/// Real tokens from vLLM `/render`. `None` when the body shape is unsupported.
	async fn render_tokens(
		&self,
		body: &Map<String, Value>,
	) -> anyhow::Result<Option<Vec<Vec<u32>>>> {
		if body.contains_key("messages") {
			if has_structured_content(body) {
				// Array-shaped content may be multimodal; let the EPP handle it.
				return Ok(None);
			}
			Ok(self.render_chat(body).await?.map(|t| vec![t]))
		} else if body.contains_key("prompt") {
			if prompt_is_token_ids(body) {
				Ok(prompt_token_ids(body))
			} else {
				Ok(Some(self.render_completions(body).await?))
			}
		} else {
			Ok(None)
		}
	}

	async fn render_chat(&self, body: &Map<String, Value>) -> anyhow::Result<Option<Vec<u32>>> {
		let resp: RenderResponse = self
			.post_render("/v1/chat/completions/render", body)
			.await?;
		if resp
			.features
			.as_ref()
			.is_some_and(RenderFeatures::is_multimodal)
		{
			return Ok(None);
		}
		Ok(Some(resp.token_ids))
	}

	async fn render_completions(&self, body: &Map<String, Value>) -> anyhow::Result<Vec<Vec<u32>>> {
		let resp: Vec<RenderResponse> = self.post_render("/v1/completions/render", body).await?;
		Ok(resp.into_iter().map(|r| r.token_ids).collect())
	}

	async fn post_render<T: serde::de::DeserializeOwned>(
		&self,
		path: &str,
		body: &Map<String, Value>,
	) -> anyhow::Result<T> {
		// The body already carries the client's `model`; vLLM's render endpoint resolves it the
		// same way the inference endpoint would.
		let mut render_body = body.clone();
		// `stream` is irrelevant to rendering and rejected by some versions.
		render_body.remove("stream");
		render_body.remove(ENVELOPE_FIELD);
		let url = format!(
			"{}{}",
			self.config.tokenizer_url.trim_end_matches('/'),
			path
		);
		let mut req = self.client.post(&url).json(&render_body);
		if let Some(key) = &self.config.tokenizer_api_key {
			req = req.bearer_auth(key);
		}
		let resp = req.send().await.context("vLLM render request")?;
		let status = resp.status();
		if !status.is_success() {
			let text = resp.text().await.unwrap_or_default();
			let snippet: String = text.chars().take(256).collect();
			anyhow::bail!("vLLM render returned status {status}: {snippet}");
		}
		resp
			.json::<T>()
			.await
			.context("decode vLLM render response")
	}

	/// Precise chain: mirrors `kvblock.chunkedTokenDatabase.TokensToKVBlockKeys` with
	/// `EmptyBlockHash` as parent, plus `preciseprefixcache.foldCacheSalt` (salt folded into
	/// block 0's extra keys as `[]MMHash{{Hash: salt}}`).
	fn precise_block_keys(&self, tokens: &[u32], model: &str, cache_salt: &str) -> Vec<u64> {
		let bs = self.config.block_size_tokens;
		let chunks: Vec<&[u32]> = tokens.chunks_exact(bs).collect();
		if chunks.is_empty() {
			return Vec::new();
		}
		let xxh = self.config.hash_algorithm == HASH_ALGORITHM_XXH64;
		let mut parent = if xxh {
			precise_xxh64(self.precise_init, &[], &[model])
		} else {
			precise_cbor_fnv(self.precise_init, &[], CborExtra::Str(model))
		};
		let mut out = Vec::with_capacity(chunks.len());
		for (i, chunk) in chunks.into_iter().enumerate() {
			let salt_extra: &[&str] = if i == 0 && !cache_salt.is_empty() {
				std::slice::from_ref(&cache_salt)
			} else {
				&[]
			};
			parent = if xxh {
				precise_xxh64(parent, chunk, salt_extra)
			} else {
				let extra = if salt_extra.is_empty() {
					// Go passes a nil []MMHash slice which CBOR-encodes as null.
					CborExtra::Null
				} else {
					CborExtra::MmHashes(salt_extra)
				};
				precise_cbor_fnv(parent, chunk, extra)
			};
			out.push(parent);
		}
		out
	}
}

/// Pseudo-tokens exactly as the EPP `token-producer` `estimate` backend produces them
/// (`tokenizer/estimate.go`): the request text is flattened to bytes, zero-padded to a
/// 4-byte boundary and read as little-endian `u32`s. Same prefix text ⇒ same pseudo-tokens
/// ⇒ same approximate hashes, which is all the approximate index needs.
///
/// Returns `None` for shapes whose byte flattening is not reproduced here (multimodal
/// content blocks, chat requests carrying `tools`, embeddings, responses API, ...); the EPP
/// then estimates on its own.
fn estimate_tokens(body: &Map<String, Value>) -> Option<Vec<Vec<u32>>> {
	if let Some(messages) = body.get("messages") {
		// `chatCompletionsBytes` prepends json.Marshal(tools); reproducing Go's struct
		// field order byte-for-byte is brittle, so fail open instead.
		if body.get("tools").is_some_and(|t| !t.is_null()) {
			return None;
		}
		let messages = messages.as_array()?;
		let mut out = Vec::new();
		for msg in messages {
			let msg = msg.as_object()?;
			if let Some(role) = msg.get("role").and_then(Value::as_str) {
				out.extend_from_slice(role.as_bytes());
			}
			match msg.get("content") {
				None | Some(Value::Null) => {},
				Some(Value::String(content)) => out.extend_from_slice(content.as_bytes()),
				Some(Value::Array(blocks)) => {
					// Only text blocks; anything else is multimodal and handled by the EPP.
					for block in blocks {
						let block = block.as_object()?;
						if block.get("type").and_then(Value::as_str) != Some("text") {
							return None;
						}
						if let Some(text) = block.get("text").and_then(Value::as_str) {
							out.extend_from_slice(text.as_bytes());
						}
					}
				},
				Some(_) => return None,
			}
		}
		return Some(vec![pack_bytes(&out)]);
	}
	if let Some(prompt) = body.get("prompt") {
		return match prompt {
			Value::String(s) => Some(vec![pack_bytes(s.as_bytes())]),
			Value::Array(items) if items.iter().all(Value::is_string) => {
				let strings: Vec<&str> = items.iter().filter_map(Value::as_str).collect();
				if strings.len() > 1 {
					// `estimateMultiStringCompletions`: one prompt per string.
					Some(strings.iter().map(|s| pack_bytes(s.as_bytes())).collect())
				} else {
					// Single-element array goes through `Prompt.PlainText()` = the string itself.
					Some(vec![pack_bytes(
						strings.first().copied().unwrap_or_default().as_bytes(),
					)])
				}
			},
			// Token-id prompts are passed through unchanged by the estimate backend too.
			Value::Array(_) if prompt_is_token_ids(body) => prompt_token_ids(body),
			_ => None,
		};
	}
	None
}

/// `tokenizer.packBytes`: zero-pad to 4 bytes, read little-endian `u32`s.
fn pack_bytes(raw: &[u8]) -> Vec<u32> {
	if raw.is_empty() {
		return Vec::new();
	}
	let mut out = Vec::with_capacity(raw.len().div_ceil(ESTIMATE_BYTES_PER_TOKEN));
	for chunk in raw.chunks(ESTIMATE_BYTES_PER_TOKEN) {
		let mut word = [0u8; ESTIMATE_BYTES_PER_TOKEN];
		word[..chunk.len()].copy_from_slice(chunk);
		out.push(u32::from_le_bytes(word));
	}
	out
}

/// Approximate chain: mirrors `prefixhash.GetBlockHashes` for a single prompt.
pub fn approximate_block_hashes(
	tokens: &[u32],
	model: &str,
	cache_salt: &str,
	block_size: usize,
	max_prefix_blocks: usize,
) -> Vec<u64> {
	if block_size == 0 {
		return Vec::new();
	}
	let mut seed = twox_hash::XxHash64::with_seed(0);
	seed.write(model.as_bytes());
	if !cache_salt.is_empty() {
		seed.write(cache_salt.as_bytes());
	}
	let mut prev = seed.finish();
	let mut out = Vec::new();
	for block in tokens.chunks_exact(block_size).take(max_prefix_blocks) {
		let block_id = xxh64(&le_u32s(block));
		let mut h = twox_hash::XxHash64::with_seed(0);
		h.write(&block_id.to_le_bytes());
		h.write(&prev.to_le_bytes());
		prev = h.finish();
		out.push(prev);
	}
	out
}

fn xxh64(bytes: &[u8]) -> u64 {
	let mut h = twox_hash::XxHash64::with_seed(0);
	h.write(bytes);
	h.finish()
}

fn le_u32s(tokens: &[u32]) -> Vec<u8> {
	let mut v = Vec::with_capacity(tokens.len() * 4);
	for t in tokens {
		v.extend_from_slice(&t.to_le_bytes());
	}
	v
}

fn encode_u32s(tokens: &[u32]) -> String {
	B64.encode(le_u32s(tokens))
}

fn encode_u64s(hashes: &[u64]) -> String {
	let mut v = Vec::with_capacity(hashes.len() * 8);
	for h in hashes {
		v.extend_from_slice(&h.to_le_bytes());
	}
	B64.encode(v)
}

/// FNV-1a 64-bit, identical to Go's `hash/fnv.New64a`.
fn fnv1a64(bytes: &[u8]) -> u64 {
	const OFFSET: u64 = 0xcbf29ce484222325;
	const PRIME: u64 = 0x100000001b3;
	let mut h = OFFSET;
	for b in bytes {
		h ^= *b as u64;
		h = h.wrapping_mul(PRIME);
	}
	h
}

/// `hashXXH64(parent, tokens, extras)` from `kvblock/token_processor.go`.
fn precise_xxh64(parent: u64, tokens: &[u32], extras: &[&str]) -> u64 {
	let mut h = twox_hash::XxHash64::with_seed(0);
	h.write(&parent.to_le_bytes());
	h.write(&le_u32s(tokens));
	for e in extras {
		h.write(&(e.len() as u64).to_le_bytes());
		h.write(e.as_bytes());
	}
	h.finish()
}

/// The `extra` argument of `chunkedTokenDatabase.hash` as the Go side passes it.
enum CborExtra<'a> {
	/// `nil` interface / nil slice.
	Null,
	/// A `string` (the model name in `getInitHash`).
	Str(&'a str),
	/// A non-empty `[]MMHash` where `MMHash{Hash string}` has no json/cbor tags.
	MmHashes(&'a [&'a str]),
}

/// `hash(parent, tokens, extra)` from `kvblock/token_processor.go`: FNV-1a64 over the
/// canonical CBOR encoding of `[]interface{}{parent, tokens, extra}`.
fn precise_cbor_fnv(parent: u64, tokens: &[u32], extra: CborExtra<'_>) -> u64 {
	let mut buf = Vec::with_capacity(16 + tokens.len() * 5);
	cbor_head(&mut buf, 4, 3); // array(3)
	cbor_head(&mut buf, 0, parent);
	match tokens.len() {
		// A nil []uint32 encodes as CBOR null; a non-nil empty slice as array(0). Callers
		// only pass nil for the init hash, which is the sole empty-token call.
		0 => buf.push(0xf6),
		n => {
			cbor_head(&mut buf, 4, n as u64);
			for t in tokens {
				cbor_head(&mut buf, 0, *t as u64);
			}
		},
	}
	match extra {
		CborExtra::Null => buf.push(0xf6),
		CborExtra::Str(s) => cbor_text(&mut buf, s),
		CborExtra::MmHashes(hashes) => {
			cbor_head(&mut buf, 4, hashes.len() as u64);
			for h in hashes {
				// struct MMHash { Hash string } → map(1) { "Hash": h }
				cbor_head(&mut buf, 5, 1);
				cbor_text(&mut buf, "Hash");
				cbor_text(&mut buf, h);
			}
		},
	}
	fnv1a64(&buf)
}

fn cbor_text(buf: &mut Vec<u8>, s: &str) {
	cbor_head(buf, 3, s.len() as u64);
	buf.extend_from_slice(s.as_bytes());
}

/// Canonical (shortest-form) CBOR head for a major type and argument.
fn cbor_head(buf: &mut Vec<u8>, major: u8, arg: u64) {
	let mt = major << 5;
	if arg < 24 {
		buf.push(mt | arg as u8);
	} else if arg <= u8::MAX as u64 {
		buf.push(mt | 24);
		buf.push(arg as u8);
	} else if arg <= u16::MAX as u64 {
		buf.push(mt | 25);
		buf.extend_from_slice(&(arg as u16).to_be_bytes());
	} else if arg <= u32::MAX as u64 {
		buf.push(mt | 26);
		buf.extend_from_slice(&(arg as u32).to_be_bytes());
	} else {
		buf.push(mt | 27);
		buf.extend_from_slice(&arg.to_be_bytes());
	}
}

fn has_structured_content(body: &Map<String, Value>) -> bool {
	body
		.get("messages")
		.and_then(Value::as_array)
		.is_some_and(|msgs| {
			msgs.iter().any(|m| {
				m.get("content")
					.is_some_and(|c| c.as_array().is_some_and(|a| !a.is_empty()))
			})
		})
}

fn prompt_is_token_ids(body: &Map<String, Value>) -> bool {
	match body.get("prompt") {
		Some(Value::Array(a)) => a.iter().any(|v| v.is_number() || v.is_array()),
		_ => false,
	}
}

/// `prompt` given as token ids: `[1,2,3]` or `[[1,2],[3]]`.
fn prompt_token_ids(body: &Map<String, Value>) -> Option<Vec<Vec<u32>>> {
	let arr = body.get("prompt")?.as_array()?;
	if arr.iter().all(Value::is_number) {
		return Some(vec![to_u32s(arr)?]);
	}
	arr
		.iter()
		.map(|v| v.as_array().and_then(|a| to_u32s(a)))
		.collect()
}

fn to_u32s(arr: &[Value]) -> Option<Vec<u32>> {
	arr
		.iter()
		.map(|v| v.as_u64().and_then(|n| u32::try_from(n).ok()))
		.collect()
}

#[cfg(test)]
mod tests {
	use super::*;

	fn config(algo: &str, seed: &str) -> PrefixCache {
		PrefixCache {
			tokenizer_url: "http://localhost:8000".into(),
			tokenizer_api_key: None,
			render_timeout: DEFAULT_RENDER_TIMEOUT,
			block_size_tokens: 4,
			approximate_token_source: ApproximateTokenSource::Render,
			approximate_block_size_tokens: 4,
			max_prefix_blocks: DEFAULT_MAX_PREFIX_BLOCKS,
			hash_seed: seed.into(),
			hash_algorithm: algo.into(),
			disable_approximate: false,
			disable_precise: false,
			ship_tokens: true,
		}
	}

	fn body(json: &str) -> Map<String, Value> {
		serde_json::from_str::<Value>(json)
			.unwrap()
			.as_object()
			.unwrap()
			.clone()
	}

	/// Pinned against the Go EPP `estimateBackend.produce` (tokenizer/estimate.go) output.
	#[test]
	fn estimate_tokens_match_go_epp() {
		let chat = body(
			r#"{"messages":[{"role":"system","content":"You are a helpful assistant."},{"role":"user","content":"Hello, 世界! 1234567"}]}"#,
		);
		assert_eq!(
			estimate_tokens(&chat),
			Some(vec![vec![
				1953724787, 1868131685, 1918967925, 543236197, 1886152040, 543978854, 1769173857,
				1851880563, 1937059444, 1699246693, 745499756, 2528699424, 562861543, 858927392, 926299444
			]])
		);
		let structured =
			body(r#"{"messages":[{"role":"user","content":[{"type":"text","text":"hi there"}]}]}"#);
		assert_eq!(
			estimate_tokens(&structured),
			Some(vec![vec![1919251317, 1948281192, 1701995880]])
		);
		let comp = body(r#"{"prompt":"abcdefghij"}"#);
		assert_eq!(
			estimate_tokens(&comp),
			Some(vec![vec![1684234849, 1751606885, 27241]])
		);
		let multi = body(r#"{"prompt":["abcde","xy"]}"#);
		assert_eq!(
			estimate_tokens(&multi),
			Some(vec![vec![1684234849, 101], vec![31096]])
		);
		let single = body(r#"{"prompt":["abcde"]}"#);
		assert_eq!(estimate_tokens(&single), Some(vec![vec![1684234849, 101]]));
		let ids = body(r#"{"prompt":[1,2,3]}"#);
		assert_eq!(estimate_tokens(&ids), Some(vec![vec![1, 2, 3]]));
	}

	#[test]
	fn estimate_tokens_fail_open_on_unsupported_shapes() {
		// tools: Go prepends json.Marshal(tools); not reproduced.
		assert_eq!(
			estimate_tokens(&body(
				r#"{"messages":[{"role":"user","content":"x"}],"tools":[{"type":"function"}]}"#
			)),
			None
		);
		// multimodal block
		assert_eq!(
			estimate_tokens(&body(
				r#"{"messages":[{"role":"user","content":[{"type":"image_url","image_url":{"url":"u"}}]}]}"#
			)),
			None
		);
		// neither messages nor prompt
		assert_eq!(estimate_tokens(&body(r#"{"input":"x"}"#)), None);
		// tools: null is treated as absent
		assert!(
			estimate_tokens(&body(
				r#"{"messages":[{"role":"user","content":"x"}],"tools":null}"#
			))
			.is_some()
		);
	}

	#[test]
	fn pack_bytes_pads_and_reads_little_endian() {
		assert_eq!(pack_bytes(b""), Vec::<u32>::new());
		assert_eq!(pack_bytes(b"a"), vec![0x61]);
		assert_eq!(pack_bytes(b"abcd"), vec![0x64636261]);
		assert_eq!(pack_bytes(b"abcde"), vec![0x64636261, 0x65]);
	}

	#[test]
	fn needs_render_and_validation() {
		let mut c = config(HASH_ALGORITHM_CBOR_FNV, "");
		c.approximate_token_source = ApproximateTokenSource::Estimate;
		// precise on → render needed regardless of approximate source
		assert!(c.needs_render());
		c.disable_precise = true;
		assert!(!c.needs_render());
		c.tokenizer_url = String::new();
		assert!(c.validate().is_ok(), "estimate-only needs no tokenizerUrl");
		c.approximate_token_source = ApproximateTokenSource::Render;
		assert!(c.needs_render());
		assert!(c.validate().is_err(), "render needs tokenizerUrl");
		c.tokenizer_url = "http://x".into();
		c.disable_approximate = true;
		assert!(c.validate().is_err(), "both chains disabled is rejected");
	}

	#[test]
	fn approximate_token_source_serde_is_lowercase() {
		assert_eq!(
			serde_json::to_string(&ApproximateTokenSource::Estimate).unwrap(),
			"\"estimate\""
		);
		assert_eq!(
			serde_json::from_str::<ApproximateTokenSource>("\"render\"").unwrap(),
			ApproximateTokenSource::Render
		);
	}

	/// Estimate-only config: `compute` must not touch the network and the approximate
	/// hashes must equal what the Go EPP computes over its own estimate pseudo-tokens
	/// (prefixhash.GetBlockHashes, bs=4, max=2048, model "m").
	#[tokio::test]
	async fn compute_estimate_only_matches_go_without_render() {
		let mut c = config(HASH_ALGORITHM_CBOR_FNV, "");
		c.tokenizer_url = "http://127.0.0.1:1".into(); // nothing listens; must never be called
		c.approximate_token_source = ApproximateTokenSource::Estimate;
		c.disable_precise = true;
		c.approximate_block_size_tokens = 4;
		let x = PrefixCacheComputer::new(Arc::new(c)).unwrap();
		let b = body(
			r#"{"model":"m","messages":[{"role":"system","content":"You are a helpful assistant."},{"role":"user","content":"Hello, 世界! 1234567"}]}"#,
		);
		let env = x.compute(&b).await.unwrap().expect("envelope");
		assert_eq!(env.model, "m", "hash seed is the client's model field");
		assert_eq!(env.prompts.len(), 1);
		assert_eq!(
			decode_u64s(&env.prompts[0].approximate_hashes),
			vec![
				1538716050029008075,
				2159989601878301997,
				9546404140426985413
			]
		);
		assert!(env.prompts[0].precise_keys.is_empty());
	}

	/// With the precise chain on, tokens come from /render; shipping approximate hashes
	/// computed over real tokens while the EPP estimates would never match, so they are
	/// withheld unless the source is explicitly `render`.
	#[test]
	fn approximate_withheld_when_sources_would_differ() {
		let mut c = config(HASH_ALGORITHM_CBOR_FNV, "");
		c.approximate_token_source = ApproximateTokenSource::Estimate;
		c.disable_precise = false;
		assert!(c.needs_render());
		let ship = !c.disable_approximate
			&& (c.approximate_token_source == ApproximateTokenSource::Render || !c.needs_render());
		assert!(!ship);
		c.approximate_token_source = ApproximateTokenSource::Render;
		let ship = !c.disable_approximate
			&& (c.approximate_token_source == ApproximateTokenSource::Render || !c.needs_render());
		assert!(ship);
	}

	fn decode_u64s(b64: &str) -> Vec<u64> {
		let bytes = B64.decode(b64).unwrap();
		bytes
			.as_chunks::<8>()
			.0
			.iter()
			.map(|c| u64::from_le_bytes(*c))
			.collect()
	}

	/// The JSON accepted by `AGENTGATEWAY_EPP_PREFIX_CACHE` is the same `PrefixCache` schema
	/// as the local YAML `prefixCache` field. `default_from_env` itself is a process-global
	/// OnceLock, so the parse + validate path is exercised directly here.
	#[test]
	fn prefix_cache_env_json_shape() {
		// Defaults: approximate on (estimate), precise off, no /render needed.
		let pc = PrefixCache::default();
		assert!(pc.validate().is_ok());
		assert!(!pc.disable_approximate);
		assert!(pc.disable_precise);
		assert_eq!(
			pc.approximate_token_source,
			ApproximateTokenSource::Estimate
		);
		assert_eq!(pc.approximate_block_size_tokens, 64);
		assert_eq!(pc.max_prefix_blocks, 2048);
		assert!(!pc.needs_render());

		// `{}` from the env var is the same as the default.
		let from_env: PrefixCache = serde_json::from_str("{}").unwrap();
		assert_eq!(from_env.disable_precise, pc.disable_precise);

		// Turning precise on without a URL is rejected by validate.
		let pc: PrefixCache = serde_json::from_str(r#"{"disablePrecise":false}"#).unwrap();
		assert!(pc.validate().is_err());
		let pc: PrefixCache =
			serde_json::from_str(r#"{"disablePrecise":false,"tokenizerUrl":"http://x"}"#).unwrap();
		assert!(pc.validate().is_ok());
		assert!(pc.needs_render());

		// Unknown keys are rejected (schema! is deny_unknown_fields) so typos surface.
		assert!(serde_json::from_str::<PrefixCache>(r#"{"foo":1}"#).is_err());
	}

	#[test]
	fn fnv1a64_matches_go() {
		// Go: fnv.New64a() with no writes.
		assert_eq!(fnv1a64(b""), 0xcbf29ce484222325);
		// Go: h.Write([]byte("a")); h.Sum64() == 0xaf63dc4c8601ec8c
		assert_eq!(fnv1a64(b"a"), 0xaf63dc4c8601ec8c);
	}

	#[test]
	fn xxh64_matches_go() {
		// xxhash.Sum64String("") == 0xef46db3751d8e999
		assert_eq!(xxh64(b""), 0xef46db3751d8e999);
		// xxhash.Sum64String("a") == 0xd24ec4f1a98c6e5b
		assert_eq!(xxh64(b"a"), 0xd24ec4f1a98c6e5b);
	}

	#[test]
	fn cbor_encoding_is_canonical() {
		let mut buf = Vec::new();
		cbor_head(&mut buf, 4, 3);
		cbor_head(&mut buf, 0, 0x1234_5678_9abc_def0);
		cbor_head(&mut buf, 4, 2);
		cbor_head(&mut buf, 0, 23);
		cbor_head(&mut buf, 0, 300);
		cbor_text(&mut buf, "m");
		assert_eq!(
			buf,
			[
				0x83, 0x1b, 0x12, 0x34, 0x56, 0x78, 0x9a, 0xbc, 0xde, 0xf0, 0x82, 0x17, 0x19, 0x01, 0x2c,
				0x61, 0x6d
			]
		);
	}

	/// Golden vectors verified against the Go EPP (`prefixhash.GetBlockHashes` and
	/// `kvblock.chunkedTokenDatabase.TokensToKVBlockKeys`); the EPP repo asserts the same
	/// values in `pkg/epp/framework/plugins/requesthandling/parsers/gateway/vectors_test.go`.
	#[test]
	fn golden_vectors_match_go_epp() {
		let tokens: Vec<u32> = (1..=10).collect();
		assert_eq!(
			approximate_block_hashes(&tokens, "m", "", 4, 128),
			[18390267952832273189, 15051379317946961216]
		);
		assert_eq!(
			approximate_block_hashes(&tokens, "m", "s", 4, 128),
			[2832891143819933408, 7734888222984014360]
		);

		let c = PrefixCacheComputer::new(Arc::new(config(HASH_ALGORITHM_CBOR_FNV, ""))).unwrap();
		assert_eq!(
			c.precise_block_keys(&tokens, "m", ""),
			[14686617092727577033, 9856792211163113858]
		);
		assert_eq!(
			c.precise_block_keys(&tokens, "m", "s"),
			[18255695703984473629, 7912881359976539370]
		);

		let x = PrefixCacheComputer::new(Arc::new(config(HASH_ALGORITHM_XXH64, "seed"))).unwrap();
		assert_eq!(
			x.precise_block_keys(&tokens, "m", "s"),
			[17109699452489004063, 9607951923782221908]
		);
	}

	#[test]
	fn approximate_drops_partial_blocks_and_caps() {
		let tokens: Vec<u32> = (1..=10).collect();
		assert_eq!(approximate_block_hashes(&tokens, "m", "", 4, 1).len(), 1);
		assert_eq!(
			approximate_block_hashes(&tokens[..3], "m", "", 4, 128).len(),
			0
		);
	}

	#[test]
	fn prompt_token_ids_shapes() {
		let body: Map<String, Value> = serde_json::from_str(r#"{"prompt":[1,2,3]}"#).unwrap();
		assert!(prompt_is_token_ids(&body));
		assert_eq!(prompt_token_ids(&body), Some(vec![vec![1, 2, 3]]));
		let body: Map<String, Value> = serde_json::from_str(r#"{"prompt":[[1,2],[3]]}"#).unwrap();
		assert_eq!(prompt_token_ids(&body), Some(vec![vec![1, 2], vec![3]]));
		let body: Map<String, Value> = serde_json::from_str(r#"{"prompt":["a","b"]}"#).unwrap();
		assert!(!prompt_is_token_ids(&body));
	}

	#[test]
	fn structured_content_detected() {
		let body: Map<String, Value> = serde_json::from_str(
			r#"{"messages":[{"role":"user","content":[{"type":"text","text":"x"}]}]}"#,
		)
		.unwrap();
		assert!(has_structured_content(&body));
		let body: Map<String, Value> =
			serde_json::from_str(r#"{"messages":[{"role":"user","content":"x"}]}"#).unwrap();
		assert!(!has_structured_content(&body));
	}

	#[test]
	fn envelope_round_trips() {
		let e = Envelope {
			version: 1,
			model: "m".into(),
			cache_salt: String::new(),
			block_size_tokens: 16,
			hash_algorithm: "cbor-fnv".into(),
			hash_seed: String::new(),
			approximate_block_size_tokens: 16,
			max_prefix_blocks: 128,
			prompts: vec![EnvelopePrompt {
				token_count: 2,
				tokens: encode_u32s(&[1, 2]),
				approximate_hashes: encode_u64s(&[u64::MAX]),
				precise_keys: String::new(),
			}],
		};
		let v = serde_json::to_value(&e).unwrap();
		assert!(v["prompts"][0].get("preciseKeys").is_none());
		assert_eq!(v["prompts"][0]["tokenCount"], 2);
		let back: Envelope = serde_json::from_value(v).unwrap();
		assert_eq!(back, e);
	}

	/// Default config ships hashes plus the token count only; the token ids would be the
	/// prompt bytes again and more than double the body sent to the EPP.
	#[tokio::test]
	async fn default_ships_token_count_not_tokens() {
		let cfg = PrefixCache::default();
		assert!(!cfg.ship_tokens);
		let c = PrefixCacheComputer::new(Arc::new(cfg)).unwrap();
		let text = "x".repeat(64 * 4 * 3 + 5);
		let b = body(&format!(
			r#"{{"model":"m","messages":[{{"role":"user","content":"{text}"}}]}}"#
		));
		let env = c.compute(&b).await.unwrap().unwrap();
		let p = &env.prompts[0];
		assert!(p.tokens.is_empty(), "tokens must not be shipped by default");
		assert_eq!(p.token_count, ("user".len() + text.len()).div_ceil(4));
		assert_eq!(decode_u64s(&p.approximate_hashes).len(), 3);
		let v = serde_json::to_value(&env).unwrap();
		assert!(v["prompts"][0].get("tokens").is_none());

		let mut cfg = PrefixCache::default();
		cfg.ship_tokens = true;
		let c = PrefixCacheComputer::new(Arc::new(cfg)).unwrap();
		let env = c.compute(&b).await.unwrap().unwrap();
		assert!(!env.prompts[0].tokens.is_empty());
	}
}
