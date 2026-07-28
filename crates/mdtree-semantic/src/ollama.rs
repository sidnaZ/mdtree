//! Native Ollama `/api/embed` client.

use std::time::Duration;

use async_trait::async_trait;
use mdtree_core::{
    EmbeddingMetric, EmbeddingProfile, SemanticError, SemanticErrorCode,
    SEMANTIC_INPUT_FORMAT_VERSION,
};
use reqwest::{Client, StatusCode, Url};
use serde::{Deserialize, Serialize};

use crate::EmbeddingProvider;

/// Default address of the user-managed local Ollama server.
pub const DEFAULT_OLLAMA_BASE_URL: &str = "http://localhost:11434";

/// Runtime-only connection settings for Ollama.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OllamaConfig {
    /// Explicit server base URL.
    pub base_url: String,
    /// Complete request deadline.
    pub timeout: Duration,
}

impl Default for OllamaConfig {
    fn default() -> Self {
        Self {
            base_url: DEFAULT_OLLAMA_BASE_URL.into(),
            timeout: Duration::from_secs(60),
        }
    }
}

/// Validating HTTP implementation of the embedding-provider boundary.
#[derive(Clone, Debug)]
pub struct OllamaProvider {
    client: Client,
    endpoint: Url,
}

impl OllamaProvider {
    /// Builds a provider without contacting Ollama.
    ///
    /// # Errors
    ///
    /// Returns a redacted configuration error for an invalid URL, embedded
    /// credentials, unsupported scheme, or unusable timeout.
    pub fn new(config: &OllamaConfig) -> Result<Self, SemanticError> {
        if config.timeout.is_zero() {
            return Err(error(
                SemanticErrorCode::OperationFailed,
                "Ollama timeout must be positive",
            ));
        }
        let mut base = Url::parse(&config.base_url).map_err(|_| {
            error(
                SemanticErrorCode::OperationFailed,
                "Ollama base URL is invalid",
            )
        })?;
        if !matches!(base.scheme(), "http" | "https") {
            return Err(error(
                SemanticErrorCode::OperationFailed,
                "Ollama base URL must use HTTP or HTTPS",
            ));
        }
        if !base.username().is_empty() || base.password().is_some() {
            return Err(error(
                SemanticErrorCode::OperationFailed,
                "Ollama base URL must not contain credentials",
            ));
        }
        base.set_path("/api/embed");
        base.set_query(None);
        base.set_fragment(None);
        let client = Client::builder()
            .timeout(config.timeout)
            .build()
            .map_err(|_| {
                error(
                    SemanticErrorCode::OperationFailed,
                    "Ollama HTTP client could not be configured",
                )
            })?;
        Ok(Self {
            client,
            endpoint: base,
        })
    }

    /// Probes one non-document input to discover a model's native dimensions.
    ///
    /// # Errors
    ///
    /// Returns the same stable provider errors as [`EmbeddingProvider::embed`]
    /// and rejects blank model names or empty/non-finite vectors.
    pub async fn discover_profile(&self, model: &str) -> Result<EmbeddingProfile, SemanticError> {
        if model.trim().is_empty() {
            return Err(error(
                SemanticErrorCode::NotConfigured,
                "Ollama embedding model is required",
            ));
        }
        let inputs = vec!["mdtree embedding profile probe".to_owned()];
        let response = self.embed_raw(model, &inputs).await?;
        if response.model != model
            || response.embeddings.len() != 1
            || response.embeddings[0].is_empty()
            || response.embeddings[0]
                .iter()
                .any(|value| !value.is_finite())
        {
            return Err(error(
                SemanticErrorCode::InvalidResponse,
                "Ollama returned an invalid profile probe",
            ));
        }
        let dimensions = u32::try_from(response.embeddings[0].len()).map_err(|_| {
            error(
                SemanticErrorCode::IncompatibleProfile,
                "Ollama embedding dimensions exceed the supported range",
            )
        })?;
        Ok(EmbeddingProfile {
            provider: "ollama".into(),
            model: response.model,
            dimensions,
            metric: EmbeddingMetric::Cosine,
            input_format_version: SEMANTIC_INPUT_FORMAT_VERSION,
        })
    }

    async fn embed_raw(
        &self,
        model: &str,
        inputs: &[String],
    ) -> Result<OllamaEmbedResponse, SemanticError> {
        let request = OllamaEmbedRequest {
            model,
            input: inputs,
            truncate: false,
        };
        let response = self
            .client
            .post(self.endpoint.clone())
            .json(&request)
            .send()
            .await
            .map_err(|source| classify_transport(&source))?;
        let status = response.status();
        if !status.is_success() {
            let detail = response
                .json::<OllamaErrorResponse>()
                .await
                .ok()
                .map(|body| body.error);
            return Err(classify_status(status, detail.as_deref()));
        }
        response.json::<OllamaEmbedResponse>().await.map_err(|_| {
            error(
                SemanticErrorCode::InvalidResponse,
                "Ollama returned malformed embedding JSON",
            )
        })
    }
}

#[async_trait]
impl EmbeddingProvider for OllamaProvider {
    async fn embed(
        &self,
        profile: &EmbeddingProfile,
        inputs: &[String],
    ) -> Result<Vec<Vec<f32>>, SemanticError> {
        if profile.provider != "ollama" {
            return Err(error(
                SemanticErrorCode::IncompatibleProfile,
                "embedding profile does not select Ollama",
            ));
        }
        if inputs.is_empty() {
            return Err(error(
                SemanticErrorCode::OperationFailed,
                "embedding batch must not be empty",
            ));
        }
        let response = self.embed_raw(&profile.model, inputs).await?;
        validate_response(profile, inputs.len(), response)
    }
}

#[derive(Serialize)]
struct OllamaEmbedRequest<'a> {
    model: &'a str,
    input: &'a [String],
    truncate: bool,
}

#[derive(Deserialize)]
struct OllamaEmbedResponse {
    model: String,
    embeddings: Vec<Vec<f32>>,
}

#[derive(Deserialize)]
struct OllamaErrorResponse {
    error: String,
}

fn validate_response(
    profile: &EmbeddingProfile,
    input_count: usize,
    response: OllamaEmbedResponse,
) -> Result<Vec<Vec<f32>>, SemanticError> {
    if response.model != profile.model {
        return Err(error(
            SemanticErrorCode::IncompatibleProfile,
            "Ollama returned a different embedding model",
        ));
    }
    if response.embeddings.len() != input_count {
        return Err(error(
            SemanticErrorCode::InvalidResponse,
            "Ollama returned the wrong number of embeddings",
        ));
    }
    let expected_dimensions = usize::try_from(profile.dimensions).map_err(|_| {
        error(
            SemanticErrorCode::IncompatibleProfile,
            "embedding dimensions exceed this platform",
        )
    })?;
    if response
        .embeddings
        .iter()
        .any(|embedding| embedding.len() != expected_dimensions)
    {
        return Err(error(
            SemanticErrorCode::IncompatibleProfile,
            "Ollama embedding dimensions changed",
        ));
    }
    if response
        .embeddings
        .iter()
        .flatten()
        .any(|value| !value.is_finite())
    {
        return Err(error(
            SemanticErrorCode::InvalidResponse,
            "Ollama returned a non-finite embedding",
        ));
    }
    Ok(response.embeddings)
}

fn classify_transport(source: &reqwest::Error) -> SemanticError {
    if source.is_timeout() {
        error(
            SemanticErrorCode::Timeout,
            "Ollama embedding request timed out",
        )
    } else {
        error(
            SemanticErrorCode::ProviderUnavailable,
            "Ollama embedding server is unavailable",
        )
    }
}

fn classify_status(status: StatusCode, detail: Option<&str>) -> SemanticError {
    let normalized = detail.unwrap_or_default().to_ascii_lowercase();
    if status == StatusCode::NOT_FOUND
        || normalized.contains("model")
            && (normalized.contains("not found") || normalized.contains("does not exist"))
    {
        error(
            SemanticErrorCode::ModelUnavailable,
            "Ollama embedding model is unavailable",
        )
    } else if normalized.contains("context length")
        || normalized.contains("input length")
        || normalized.contains("too long")
        || normalized.contains("truncate")
    {
        error(
            SemanticErrorCode::InputTooLarge,
            "embedding input exceeds the Ollama model context",
        )
    } else {
        error(
            SemanticErrorCode::InvalidResponse,
            "Ollama rejected the embedding request",
        )
    }
}

fn error(code: SemanticErrorCode, message: &str) -> SemanticError {
    SemanticError {
        code,
        message: message.into(),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use axum::body::Body;
    use axum::extract::State;
    use axum::http::{Response, StatusCode};
    use axum::routing::post;
    use axum::Router;
    use mdtree_core::{
        EmbeddingMetric, EmbeddingProfile, SemanticErrorCode, SEMANTIC_INPUT_FORMAT_VERSION,
    };
    use serde_json::{json, Value};

    use super::{EmbeddingProvider, OllamaConfig, OllamaProvider};

    struct FixtureState {
        status: StatusCode,
        body: String,
        delay: Duration,
        requests: Mutex<Vec<Value>>,
    }

    async fn embed_fixture(
        State(state): State<Arc<FixtureState>>,
        axum::Json(request): axum::Json<Value>,
    ) -> Response<Body> {
        state.requests.lock().expect("request lock").push(request);
        tokio::time::sleep(state.delay).await;
        Response::builder()
            .status(state.status)
            .header("content-type", "application/json")
            .body(Body::from(state.body.clone()))
            .expect("response")
    }

    async fn server(
        status: StatusCode,
        body: impl Into<String>,
        delay: Duration,
    ) -> (String, Arc<FixtureState>, tokio::task::JoinHandle<()>) {
        let state = Arc::new(FixtureState {
            status,
            body: body.into(),
            delay,
            requests: Mutex::new(Vec::new()),
        });
        let app = Router::new()
            .route("/api/embed", post(embed_fixture))
            .with_state(Arc::clone(&state));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("fixture listener");
        let address = listener.local_addr().expect("fixture address");
        let handle = tokio::spawn(async move {
            axum::serve(listener, app).await.expect("fixture server");
        });
        (format!("http://{address}"), state, handle)
    }

    fn profile() -> EmbeddingProfile {
        EmbeddingProfile {
            provider: "ollama".into(),
            model: "embeddinggemma".into(),
            dimensions: 2,
            metric: EmbeddingMetric::Cosine,
            input_format_version: SEMANTIC_INPUT_FORMAT_VERSION,
        }
    }

    #[tokio::test]
    async fn sends_one_ordered_batch_with_truncation_disabled() {
        let response = json!({
            "model": "embeddinggemma",
            "embeddings": [[1.0, 0.0], [0.0, 1.0]]
        });
        let (base_url, state, server) =
            server(StatusCode::OK, response.to_string(), Duration::ZERO).await;
        let provider = OllamaProvider::new(&OllamaConfig {
            base_url,
            timeout: Duration::from_secs(1),
        })
        .expect("provider");
        let inputs = vec!["first".to_owned(), "second".to_owned()];

        let vectors = provider
            .embed(&profile(), &inputs)
            .await
            .expect("embeddings");

        assert_eq!(vectors, vec![vec![1.0, 0.0], vec![0.0, 1.0]]);
        assert_eq!(
            *state.requests.lock().expect("requests"),
            vec![json!({
                "model": "embeddinggemma",
                "input": ["first", "second"],
                "truncate": false
            })]
        );
        server.abort();
    }

    #[tokio::test]
    async fn discovers_native_model_dimensions_without_document_content() {
        let response = json!({
            "model": "embeddinggemma",
            "embeddings": [[1.0, 0.0, 0.5]]
        });
        let (base_url, state, server) =
            server(StatusCode::OK, response.to_string(), Duration::ZERO).await;
        let provider = OllamaProvider::new(&OllamaConfig {
            base_url,
            timeout: Duration::from_secs(1),
        })
        .expect("provider");

        let profile = provider
            .discover_profile("embeddinggemma")
            .await
            .expect("profile");

        assert_eq!(profile.dimensions, 3);
        let request = &state.requests.lock().expect("requests")[0];
        assert_eq!(request["truncate"], false);
        assert_eq!(request["input"], json!(["mdtree embedding profile probe"]));
        server.abort();
    }

    #[tokio::test]
    async fn classifies_missing_model_context_overflow_and_timeout() {
        for (status, body, expected) in [
            (
                StatusCode::NOT_FOUND,
                r#"{"error":"model not found"}"#,
                SemanticErrorCode::ModelUnavailable,
            ),
            (
                StatusCode::BAD_REQUEST,
                r#"{"error":"input length exceeds context length; truncate is false"}"#,
                SemanticErrorCode::InputTooLarge,
            ),
        ] {
            let (base_url, _, server) = server(status, body, Duration::ZERO).await;
            let provider = OllamaProvider::new(&OllamaConfig {
                base_url,
                timeout: Duration::from_secs(1),
            })
            .expect("provider");
            let error = provider
                .embed(&profile(), &["secret input".into()])
                .await
                .expect_err("provider error");
            assert_eq!(error.code, expected);
            assert!(!error.message.contains("secret input"));
            server.abort();
        }

        let (base_url, _, server) = server(
            StatusCode::OK,
            r#"{"model":"embeddinggemma","embeddings":[[1.0,0.0]]}"#,
            Duration::from_millis(100),
        )
        .await;
        let provider = OllamaProvider::new(&OllamaConfig {
            base_url,
            timeout: Duration::from_millis(10),
        })
        .expect("provider");
        let error = provider
            .embed(&profile(), &["secret input".into()])
            .await
            .expect_err("timeout");
        assert_eq!(error.code, SemanticErrorCode::Timeout);
        assert!(!error.message.contains("secret input"));
        server.abort();
    }

    #[tokio::test]
    async fn rejects_malformed_count_model_and_dimension_drift_without_content_leaks() {
        let cases = [
            ("{broken", SemanticErrorCode::InvalidResponse),
            (
                r#"{"model":"other","embeddings":[[1.0,0.0]]}"#,
                SemanticErrorCode::IncompatibleProfile,
            ),
            (
                r#"{"model":"embeddinggemma","embeddings":[]}"#,
                SemanticErrorCode::InvalidResponse,
            ),
            (
                r#"{"model":"embeddinggemma","embeddings":[[1.0]]}"#,
                SemanticErrorCode::IncompatibleProfile,
            ),
        ];
        for (body, expected) in cases {
            let (base_url, _, server) = server(StatusCode::OK, body, Duration::ZERO).await;
            let provider = OllamaProvider::new(&OllamaConfig {
                base_url,
                timeout: Duration::from_secs(1),
            })
            .expect("provider");
            let error = provider
                .embed(&profile(), &["private document text".into()])
                .await
                .expect_err("invalid response");
            assert_eq!(error.code, expected);
            assert!(!error.message.contains("private document text"));
            server.abort();
        }
    }

    #[test]
    fn rejects_credentials_and_invalid_runtime_configuration() {
        for config in [
            OllamaConfig {
                base_url: "file:///tmp/ollama".into(),
                timeout: Duration::from_secs(1),
            },
            OllamaConfig {
                base_url: "http://user:password@localhost:11434".into(),
                timeout: Duration::from_secs(1),
            },
            OllamaConfig {
                base_url: "http://localhost:11434".into(),
                timeout: Duration::ZERO,
            },
        ] {
            let error = OllamaProvider::new(&config).expect_err("invalid configuration");
            assert!(!error.message.contains("password"));
            assert!(!error.message.contains("user:"));
            assert!(!error.message.contains(&config.base_url));
        }
    }
}
