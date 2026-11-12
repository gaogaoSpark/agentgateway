//! Request body handling for the endpoint picker (EPP) used by inference routing.
//!
//! The EPP only needs a small subset of the OpenAI-style request body to select a
//! destination. Before the request is handed to ext_proc the body is replaced with a
//! projection containing only those fields. Once ext_proc returns, the client's original
//! body is put back so the upstream receives it untouched; only the header mutations
//! returned by the EPP (such as the destination header) are kept.

use bytes::Bytes;
use http_body::Body;
use serde_json::{Map, Value};

use super::epp_prefix::{self, PrefixCacheComputer};
use crate::http;

/// Body fields read by the EPP (llm-d inference scheduler / GIE) across the
/// `/chat/completions`, `/completions`, `/embeddings` and `/responses` shapes.
const EPP_BODY_FIELDS: &[&str] = &[
	"model",
	"prompt",
	"messages",
	"tools",
	"tool_choice",
	"input",
	"instructions",
	"max_tokens",
	"max_completion_tokens",
	"max_output_tokens",
	"stream",
	"cache_salt",
];

/// Fields the EPP `gateway-parser` fast path reads when the envelope is present. The prompt
/// content itself (`messages`, `prompt`, `tools`, ...) is only needed by the EPP to
/// tokenize and hash locally, which the envelope makes unnecessary, so it is dropped: the
/// projection then has a small fixed size regardless of prompt length.
const EPP_ENVELOPE_FIELDS: &[&str] = &[
	"model",
	"stream",
	"max_tokens",
	"max_completion_tokens",
	"max_output_tokens",
	"cache_salt",
];

/// The client's request body, held aside while ext_proc sees the projection.
pub(super) struct OriginalBody {
	body: http::Body,
	content_length: Option<http::HeaderValue>,
}

/// Replace the request body with the EPP projection and return the original so it can be
/// restored later. Returns `None` and leaves the request untouched when the body cannot be
/// projected: no body, non-identity `Content-Encoding`, body over the buffer limit, or
/// body that is not a JSON object.
///
/// When `prefix` is configured, the gateway tokenizes the prompt and computes the
/// prefix-cache hashes, embedding them in the projection under
/// [`epp_prefix::ENVELOPE_FIELD`]. Any failure there is logged and the plain projection
/// is sent instead, so the EPP falls back to computing hashes itself.
pub(super) async fn project_for_epp(
	req: &mut http::Request,
	prefix: Option<&PrefixCacheComputer>,
) -> anyhow::Result<Option<OriginalBody>> {
	if req.body().is_end_stream() || has_content_encoding(req.headers()) {
		return Ok(None);
	}
	let http::BodyInspection::Complete(bytes) = http::inspect_body(req).await? else {
		return Ok(None);
	};
	let Some(mut fields) = project(&bytes) else {
		return Ok(None);
	};
	if let Some(prefix) = prefix {
		match prefix.compute(&fields).await {
			Ok(Some(envelope)) => match serde_json::to_value(&envelope) {
				Ok(v) => {
					fields.retain(|k, _| EPP_ENVELOPE_FIELDS.contains(&k.as_str()));
					fields.insert(epp_prefix::ENVELOPE_FIELD.to_string(), v);
				},
				Err(e) => tracing::warn!("failed to encode prefix-cache envelope: {e}"),
			},
			Ok(None) => {},
			Err(e) => tracing::warn!("prefix-cache precompute failed, EPP will tokenize: {e}"),
		}
	}
	let Ok(projected) = serde_json::to_vec(&Value::Object(fields)).map(Bytes::from) else {
		return Ok(None);
	};
	let body = req.body_mut().take_content();
	let content_length = req.headers_mut().remove(http::header::CONTENT_LENGTH);
	req
		.headers_mut()
		.insert(http::header::CONTENT_LENGTH, projected.len().into());
	req.body_mut().replace_bytes(projected);
	Ok(Some(OriginalBody {
		body,
		content_length,
	}))
}

/// Put the client's original body (and its `Content-Length`) back on the request, discarding
/// whatever body ext_proc produced. Header mutations applied by ext_proc are preserved.
pub(super) fn restore_original(req: &mut http::Request, original: OriginalBody) {
	req.body_mut().restore_content(original.body);
	req.headers_mut().remove(http::header::CONTENT_LENGTH);
	if let Some(content_length) = original.content_length {
		req
			.headers_mut()
			.insert(http::header::CONTENT_LENGTH, content_length);
	}
}

fn has_content_encoding(headers: &http::HeaderMap) -> bool {
	headers
		.get(http::header::CONTENT_ENCODING)
		.and_then(|v| v.to_str().ok())
		.is_some_and(|v| !v.trim().eq_ignore_ascii_case("identity"))
}

/// Keep only [`EPP_BODY_FIELDS`] from a JSON object body.
fn project(body: &Bytes) -> Option<Map<String, Value>> {
	let Value::Object(fields) = serde_json::from_slice::<Value>(body).ok()? else {
		return None;
	};
	Some(
		fields
			.into_iter()
			.filter(|(k, _)| EPP_BODY_FIELDS.contains(&k.as_str()))
			.collect(),
	)
}

#[cfg(test)]
mod tests {
	use http_body_util::BodyExt;

	use super::*;

	fn request(body: &'static str) -> http::Request {
		let mut req = ::http::Request::builder()
			.method("POST")
			.uri("http://example.com/v1/chat/completions")
			.body(http::Body::from(body))
			.unwrap();
		req
			.headers_mut()
			.insert(http::header::CONTENT_LENGTH, body.len().into());
		req
	}

	#[test]
	fn project_keeps_only_epp_fields() {
		let body = Bytes::from_static(
			br#"{"model":"m","messages":[{"role":"user","content":"hi"}],"temperature":0.7,"top_p":1,"max_tokens":8,"user":"u"}"#,
		);
		let obj = project(&body).unwrap();
		assert_eq!(obj.len(), 3);
		assert_eq!(obj["model"], "m");
		assert_eq!(obj["max_tokens"], 8);
		assert!(obj.contains_key("messages"));
	}

	#[test]
	fn project_rejects_non_object_or_invalid_json() {
		assert!(project(&Bytes::from_static(b"[1,2]")).is_none());
		assert!(project(&Bytes::from_static(b"not json")).is_none());
	}

	#[tokio::test]
	async fn project_then_restore_round_trips_original_body() {
		let original = r#"{"model":"m","prompt":"p","temperature":0.5}"#;
		let mut req = request(original);
		let held = project_for_epp(&mut req, None).await.unwrap().unwrap();

		let projected = req.body().known_bytes().unwrap().clone();
		let projected_json: Value = serde_json::from_slice(&projected).unwrap();
		assert!(projected_json.get("temperature").is_none());
		assert_eq!(
			req.headers()[http::header::CONTENT_LENGTH],
			projected.len().to_string()
		);

		// Simulate ext_proc swapping in its own body and stripping Content-Length.
		req
			.body_mut()
			.replace_bytes(Bytes::from_static(b"from epp"));
		req.headers_mut().remove(http::header::CONTENT_LENGTH);
		req.headers_mut().insert("x-from-epp", "1".parse().unwrap());

		restore_original(&mut req, held);
		assert_eq!(req.headers()["x-from-epp"], "1");
		assert_eq!(
			req.headers()[http::header::CONTENT_LENGTH],
			original.len().to_string()
		);
		let bytes = req.into_body().collect().await.unwrap().to_bytes();
		assert_eq!(bytes, original.as_bytes());
	}

	/// With hashes computed, the prompt content is dead weight for the EPP: the projection
	/// must shrink to the fixed set of scalar fields plus the envelope.
	#[tokio::test]
	async fn envelope_replaces_prompt_content() {
		use std::sync::Arc;
		let pc = PrefixCacheComputer::new(Arc::new(epp_prefix::PrefixCache::default())).unwrap();
		let text = "y".repeat(5000);
		let original = format!(
			r#"{{"model":"m","messages":[{{"role":"user","content":"{text}"}}],"max_tokens":8,"stream":true,"tool_choice":"auto","temperature":0.2}}"#
		);
		let mut req = request(Box::leak(original.clone().into_boxed_str()));
		project_for_epp(&mut req, Some(&pc)).await.unwrap().unwrap();
		let projected = req.body().known_bytes().unwrap().clone();
		let obj: Map<String, Value> = serde_json::from_slice(&projected).unwrap();
		assert!(
			obj.get("messages").is_none(),
			"prompt content must be stripped"
		);
		assert!(obj.get("tool_choice").is_none());
		assert_eq!(obj["model"], "m");
		assert_eq!(obj["max_tokens"], 8);
		assert_eq!(obj["stream"], true);
		assert!(obj.contains_key(epp_prefix::ENVELOPE_FIELD));
		assert!(
			projected.len() < 1024,
			"projection should be small and independent of prompt length, got {}",
			projected.len()
		);
		assert!(projected.len() * 4 < original.len());

		// Fail-open (no hashes computable): prompt content stays so the EPP can tokenize.
		let mut req = request(
			r#"{"model":"m","messages":[{"role":"user","content":[{"type":"image_url","image_url":{"url":"x"}}]}]}"#,
		);
		project_for_epp(&mut req, Some(&pc)).await.unwrap().unwrap();
		let obj: Map<String, Value> =
			serde_json::from_slice(req.body().known_bytes().unwrap()).unwrap();
		assert!(obj.contains_key("messages"));
		assert!(obj.get(epp_prefix::ENVELOPE_FIELD).is_none());
	}

	#[tokio::test]
	async fn project_skips_empty_and_encoded_bodies() {
		let mut empty = ::http::Request::builder()
			.body(http::Body::empty())
			.unwrap();
		assert!(project_for_epp(&mut empty, None).await.unwrap().is_none());

		let mut encoded = request(r#"{"model":"m"}"#);
		encoded
			.headers_mut()
			.insert(http::header::CONTENT_ENCODING, "gzip".parse().unwrap());
		assert!(project_for_epp(&mut encoded, None).await.unwrap().is_none());
	}
}
