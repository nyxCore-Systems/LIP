//! HTTP embedding client for the LIP daemon.
//!
//! Talks to any OpenAI-compatible `/v1/embeddings` endpoint. Configure via:
//!
//! ```text
//! LIP_EMBEDDING_URL=http://localhost:11434/v1/embeddings   # e.g. Ollama
//! LIP_EMBEDDING_MODEL=nomic-embed-text                     # optional
//! ```
//!
//! When `LIP_EMBEDDING_URL` is unset, [`EmbeddingClient::from_env`] returns `None`
//! and all embedding requests return a sensible error to the caller.

use serde::{Deserialize, Serialize};

/// Classified failure from the embedding HTTP endpoint.
///
/// The variants map directly to [`crate::query_graph::ErrorCode`]
/// categories so the daemon can propagate a precise classification to
/// clients instead of collapsing every endpoint failure into `Internal`.
/// Callers that only need a display string should use the `Display` impl.
#[derive(Debug)]
pub enum EmbedError {
    /// The endpoint rejected the requested model name — either 404, or
    /// a 4xx whose body names the model. Maps to `ErrorCode::UnknownModel`.
    /// Retrying with the same model is pointless.
    UnknownModel(String),
    /// HTTP transport failure, timeout, or TLS error. Maps to
    /// `ErrorCode::Internal`. Retry is often safe.
    Transport(String),
    /// The endpoint returned a response we could not parse, or the
    /// vector count did not match the input count. Maps to
    /// `ErrorCode::Internal`. Indicates a backend misconfiguration.
    Protocol(String),
    /// Non-2xx status that does not clearly match any of the above.
    /// Maps to `ErrorCode::Internal`.
    Http(String),
}

impl std::fmt::Display for EmbedError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            EmbedError::UnknownModel(m)
            | EmbedError::Transport(m)
            | EmbedError::Protocol(m)
            | EmbedError::Http(m) => f.write_str(m),
        }
    }
}

impl std::error::Error for EmbedError {}

/// Classify an embedding endpoint's non-2xx response into the narrowest
/// applicable [`EmbedError`] variant.
///
/// Heuristic: 404 is always an unknown-model signal (OpenAI, Ollama, and
/// most compatible backends 404 on an unrecognised model). Other 4xx are
/// classified as `UnknownModel` only when the body mentions the model —
/// OpenAI-compatible errors typically carry `"code":"model_not_found"`
/// or a message containing `"model"` for this case. Everything else
/// (5xx, 4xx without model keyword) falls through to `Http`.
fn classify_http_error(status: reqwest::StatusCode, body: &str) -> EmbedError {
    let msg = format!("embedding endpoint returned {status}: {body}");
    if status == reqwest::StatusCode::NOT_FOUND {
        return EmbedError::UnknownModel(msg);
    }
    if status.is_client_error() {
        let lower = body.to_ascii_lowercase();
        if lower.contains("model_not_found") || lower.contains("unknown model") {
            return EmbedError::UnknownModel(msg);
        }
        // Conservative: generic 4xx with "model" mention, treat as model issue
        // only when combined with a "not found" / "invalid" / "unsupported" hint,
        // to avoid misclassifying auth / rate-limit errors.
        let looks_model_shaped = lower.contains("model")
            && (lower.contains("not found")
                || lower.contains("invalid")
                || lower.contains("unsupported")
                || lower.contains("does not exist"));
        if looks_model_shaped {
            return EmbedError::UnknownModel(msg);
        }
    }
    EmbedError::Http(msg)
}

/// Thin client around a single OpenAI-compatible embedding endpoint.
pub struct EmbeddingClient {
    url: String,
    default_model: String,
    http: reqwest::Client,
}

// ─── Wire types ───────────────────────────────────────────────────────────────

#[derive(Serialize)]
struct EmbedRequest<'a> {
    model: &'a str,
    input: &'a [String],
}

#[derive(Deserialize)]
struct EmbedResponse {
    data: Vec<EmbedData>,
    model: String,
}

#[derive(Deserialize)]
struct EmbedData {
    embedding: Vec<f32>,
    index: usize,
}

// ─── Implementation ───────────────────────────────────────────────────────────

impl EmbeddingClient {
    /// Build from environment variables.
    ///
    /// Returns `None` when `LIP_EMBEDDING_URL` is not set.
    pub fn from_env() -> Option<Self> {
        let url = std::env::var("LIP_EMBEDDING_URL").ok()?;
        let model = std::env::var("LIP_EMBEDDING_MODEL")
            .unwrap_or_else(|_| "text-embedding-3-small".to_owned());
        let http = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(60))
            .build()
            .expect("reqwest client build should not fail");
        Some(Self {
            url,
            default_model: model,
            http,
        })
    }

    /// The default model name this client was configured with.
    pub fn default_model(&self) -> &str {
        &self.default_model
    }

    /// Embed a batch of `texts`.
    ///
    /// Returns one vector per input in the same order. The model name actually
    /// used (after any override) is returned alongside the vectors.
    ///
    /// # Errors
    ///
    /// Returns an [`EmbedError`] classified so the daemon can map directly
    /// to a [`crate::query_graph::ErrorCode`] without inspecting the
    /// message string.
    pub async fn embed_texts(
        &self,
        texts: &[String],
        model_override: Option<&str>,
    ) -> Result<(Vec<Vec<f32>>, String), EmbedError> {
        if texts.is_empty() {
            return Ok((vec![], self.default_model.clone()));
        }
        let model = model_override.unwrap_or(&self.default_model);
        let body = EmbedRequest {
            model,
            input: texts,
        };
        let resp = self
            .http
            .post(&self.url)
            .json(&body)
            .send()
            .await
            .map_err(|e| EmbedError::Transport(format!("embedding HTTP request failed: {e}")))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            return Err(classify_http_error(status, &text));
        }

        let parsed: EmbedResponse = resp.json().await.map_err(|e| {
            EmbedError::Protocol(format!("failed to parse embedding response: {e}"))
        })?;

        // Re-order by index field to match the input order.
        let mut data = parsed.data;
        data.sort_by_key(|d| d.index);

        if data.len() != texts.len() {
            return Err(EmbedError::Protocol(format!(
                "embedding endpoint returned {} vectors for {} inputs",
                data.len(),
                texts.len()
            )));
        }

        let vectors = data.into_iter().map(|d| d.embedding).collect();
        Ok((vectors, parsed.model))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use reqwest::StatusCode;

    #[test]
    fn classify_404_is_unknown_model() {
        let e = classify_http_error(StatusCode::NOT_FOUND, "model not found");
        assert!(matches!(e, EmbedError::UnknownModel(_)));
    }

    #[test]
    fn classify_openai_model_not_found_code() {
        // OpenAI API shape.
        let body =
            r#"{"error":{"code":"model_not_found","message":"The model 'foo' does not exist"}}"#;
        let e = classify_http_error(StatusCode::BAD_REQUEST, body);
        assert!(matches!(e, EmbedError::UnknownModel(_)));
    }

    #[test]
    fn classify_ollama_model_unknown() {
        let body = r#"{"error":"model 'nomic-embed-text' not found, try pulling it first"}"#;
        let e = classify_http_error(StatusCode::NOT_FOUND, body);
        assert!(matches!(e, EmbedError::UnknownModel(_)));
    }

    #[test]
    fn classify_auth_error_stays_http() {
        // 401 unauthorized must not be misclassified as UnknownModel just
        // because a token payload might mention "model".
        let body = "Unauthorized";
        let e = classify_http_error(StatusCode::UNAUTHORIZED, body);
        assert!(matches!(e, EmbedError::Http(_)));
    }

    #[test]
    fn classify_rate_limit_stays_http() {
        let e = classify_http_error(StatusCode::TOO_MANY_REQUESTS, "rate limit");
        assert!(matches!(e, EmbedError::Http(_)));
    }

    #[test]
    fn classify_5xx_stays_http() {
        let e = classify_http_error(StatusCode::INTERNAL_SERVER_ERROR, "backend died");
        assert!(matches!(e, EmbedError::Http(_)));
    }

    #[test]
    fn classify_4xx_mentioning_model_without_not_found_keyword_stays_http() {
        // "model temperature too high" would mention "model" but is not
        // an unknown-model signal. Conservative classifier keeps it Http.
        let body = "model temperature parameter rejected";
        let e = classify_http_error(StatusCode::BAD_REQUEST, body);
        assert!(matches!(e, EmbedError::Http(_)));
    }
}
