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

use anyhow::Context;
use serde::{Deserialize, Serialize};

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
    /// Propagates HTTP, serialisation, and API errors.
    pub async fn embed_texts(
        &self,
        texts: &[String],
        model_override: Option<&str>,
    ) -> anyhow::Result<(Vec<Vec<f32>>, String)> {
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
            .context("embedding HTTP request failed")?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            anyhow::bail!("embedding endpoint returned {status}: {text}");
        }

        let parsed: EmbedResponse = resp
            .json()
            .await
            .context("failed to parse embedding response")?;

        // Re-order by index field to match the input order.
        let mut data = parsed.data;
        data.sort_by_key(|d| d.index);

        anyhow::ensure!(
            data.len() == texts.len(),
            "embedding endpoint returned {} vectors for {} inputs",
            data.len(),
            texts.len()
        );

        let vectors = data.into_iter().map(|d| d.embedding).collect();
        Ok((vectors, parsed.model))
    }
}
