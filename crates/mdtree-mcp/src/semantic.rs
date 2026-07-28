//! Opt-in semantic index mutation tools.

use std::time::{SystemTime, UNIX_EPOCH};

use mdtree_core::{
    EmbeddingMetric, EmbeddingProfile, SemanticError, SemanticErrorCode,
    SEMANTIC_INPUT_FORMAT_VERSION,
};
use mdtree_semantic::{
    build_semantic_index, clear_semantic_index, resume_semantic_index, retry_semantic_index,
    SemanticBuildOptions,
};
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::CallToolResult;
use rmcp::schemars;
use rmcp::{tool, tool_router, ErrorData};
use serde::Deserialize;

use crate::{json_result, semantic_error, MdtreeServer};

#[derive(Debug, Deserialize, rmcp::schemars::JsonSchema)]
pub struct SemanticBuildParams {
    /// Provider request batch size (1 through 100).
    #[serde(default = "default_batch_size")]
    pub batch_size: u32,
    /// Known native dimensions; omitted to probe the configured model once.
    pub dimensions: Option<u32>,
}

#[derive(Debug, Deserialize, rmcp::schemars::JsonSchema)]
pub struct SemanticResumeParams {
    /// Provider request batch size (1 through 100).
    #[serde(default = "default_batch_size")]
    pub batch_size: u32,
}

#[derive(Debug, Deserialize, rmcp::schemars::JsonSchema)]
pub struct SemanticClearParams {
    /// Preview the number of derived chunks without changing the workspace.
    #[serde(default)]
    pub dry_run: bool,
    /// Explicit confirmation required unless `dry_run` is true.
    #[serde(default)]
    pub confirm: bool,
}

const fn default_batch_size() -> u32 {
    32
}

fn checked_batch_size(value: u32) -> Result<u32, ErrorData> {
    if (1..=100).contains(&value) {
        Ok(value)
    } else {
        Err(ErrorData::invalid_params(
            "batch_size must be between 1 and 100",
            Some(serde_json::json!({"code":"invalid_batch_size"})),
        ))
    }
}

fn now_millis() -> Result<u64, ErrorData> {
    let value = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| ErrorData::internal_error("system clock is before Unix epoch", None))?
        .as_millis();
    u64::try_from(value)
        .map_err(|_| ErrorData::internal_error("system clock exceeds supported range", None))
}

fn build_result(
    result: Result<mdtree_semantic::SemanticBuildReport, mdtree_semantic::SemanticBuildFailure>,
) -> Result<CallToolResult, ErrorData> {
    match result {
        Ok(report) => json_result(serde_json::json!({"status":"complete","report":report})),
        Err(failure) => json_result(serde_json::json!({
            "status":"partial",
            "error":failure.error,
            "report":failure.report,
        })),
    }
}

#[tool_router(router = semantic_mutation_tool_router)]
impl MdtreeServer {
    pub(crate) fn semantic_write_tool_router() -> ToolRouter<Self> {
        Self::semantic_mutation_tool_router()
    }

    #[tool(
        description = "Build or refresh the semantic index using the server-configured Ollama model"
    )]
    async fn semantic_index_build(
        &self,
        Parameters(p): Parameters<SemanticBuildParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let batch_size = checked_batch_size(p.batch_size)?;
        let model = self.semantic.model.as_deref().ok_or_else(|| {
            semantic_error(SemanticError {
                code: SemanticErrorCode::NotConfigured,
                message: "Ollama embedding model is not configured at server startup".into(),
            })
        })?;
        let provider = self.semantic_provider()?;
        let profile = if let Some(dimensions) = p.dimensions {
            EmbeddingProfile {
                provider: "ollama".into(),
                model: model.into(),
                dimensions,
                metric: EmbeddingMetric::Cosine,
                input_format_version: SEMANTIC_INPUT_FORMAT_VERSION,
            }
        } else {
            provider
                .discover_profile(model)
                .await
                .map_err(semantic_error)?
        };
        let mut store = self.independent_store()?;
        build_result(
            build_semantic_index(
                &mut store,
                &provider,
                &profile,
                SemanticBuildOptions {
                    batch_size,
                    ..SemanticBuildOptions::default()
                },
                now_millis()?,
            )
            .await,
        )
    }

    #[tool(description = "Resume pending or interrupted semantic index work")]
    async fn semantic_index_resume(
        &self,
        Parameters(p): Parameters<SemanticResumeParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let batch_size = checked_batch_size(p.batch_size)?;
        let mut store = self.independent_store()?;
        let profile = self.active_semantic_profile(&store)?;
        let provider = self.semantic_provider()?;
        build_result(
            resume_semantic_index(&mut store, &provider, &profile, batch_size, now_millis()?).await,
        )
    }

    #[tool(description = "Requeue failed chunks and resume semantic index work")]
    async fn semantic_index_retry(
        &self,
        Parameters(p): Parameters<SemanticResumeParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let batch_size = checked_batch_size(p.batch_size)?;
        let mut store = self.independent_store()?;
        let profile = self.active_semantic_profile(&store)?;
        let provider = self.semantic_provider()?;
        build_result(
            retry_semantic_index(&mut store, &provider, &profile, batch_size, now_millis()?).await,
        )
    }

    #[tool(description = "Preview or explicitly clear all derived semantic index data")]
    async fn semantic_index_clear(
        &self,
        Parameters(p): Parameters<SemanticClearParams>,
    ) -> Result<CallToolResult, ErrorData> {
        if !p.dry_run && !p.confirm {
            return Err(ErrorData::invalid_params(
                "confirm must be true unless dry_run is true",
                Some(serde_json::json!({"code":"confirmation_required"})),
            ));
        }
        let mut store = self.independent_store()?;
        let before = store
            .semantic_index_status()
            .map_err(|error| ErrorData::internal_error(error.to_string(), None))?;
        if p.dry_run {
            return json_result(serde_json::json!({
                "status":"planned",
                "would_clear_chunks":before.coverage.total,
                "index":before,
            }));
        }
        let after = clear_semantic_index(&mut store).map_err(semantic_error)?;
        json_result(serde_json::json!({"status":"cleared","index":after}))
    }
}

#[cfg(test)]
mod tests {
    use rmcp::handler::server::wrapper::Parameters;
    use tempfile::tempdir;

    use super::{SemanticBuildParams, SemanticClearParams};
    use crate::{McpAccessMode, MdtreeServer};

    #[tokio::test]
    async fn lifecycle_requires_configuration_and_explicit_clear_confirmation() {
        let directory = tempdir().expect("directory");
        let path = directory.path().join("semantic-tools.mdtree");
        let snapshot = mdtree_sqlite::plan_json_import(include_bytes!(
            "../../../examples/northstar-platform.snapshot.json"
        ))
        .expect("snapshot");
        mdtree_sqlite::import_snapshot_new(&path, &snapshot.snapshot).expect("workspace");
        let server = MdtreeServer::open_with_mode(&path, McpAccessMode::ReadWrite).expect("server");

        let build = server
            .semantic_index_build(Parameters(SemanticBuildParams {
                batch_size: 4,
                dimensions: Some(2),
            }))
            .await
            .expect_err("model is required");
        assert!(serde_json::to_string(&build)
            .expect("build error")
            .contains("not_configured"));

        let unconfirmed = server
            .semantic_index_clear(Parameters(SemanticClearParams {
                dry_run: false,
                confirm: false,
            }))
            .await
            .expect_err("confirmation is required");
        assert!(serde_json::to_string(&unconfirmed)
            .expect("clear error")
            .contains("confirmation_required"));

        let preview = server
            .semantic_index_clear(Parameters(SemanticClearParams {
                dry_run: true,
                confirm: false,
            }))
            .await
            .expect("preview");
        assert_ne!(preview.is_error, Some(true));
    }
}
