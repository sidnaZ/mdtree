//! Opt-in runtime workspace selection for the MCP adapter.

use std::path::{Path, PathBuf};

use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::CallToolResult;
use rmcp::schemars;
use rmcp::{tool, tool_router, ErrorData};
use serde::Deserialize;

use crate::{MdtreeServer, WorkspaceBinding};

/// Filesystem policy governing runtime workspace selection.
#[derive(Clone, Debug)]
pub struct WorkspaceSwitchPolicy {
    roots: Vec<PathBuf>,
}

impl WorkspaceSwitchPolicy {
    /// Builds a policy from existing directory roots and canonicalizes each root.
    pub fn new(roots: Vec<PathBuf>) -> std::io::Result<Self> {
        if roots.is_empty() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "workspace switching requires at least one workspace root",
            ));
        }
        let roots = roots
            .into_iter()
            .map(|root| {
                let root = root.canonicalize()?;
                if !root.is_dir() {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidInput,
                        format!("workspace root is not a directory: {}", root.display()),
                    ));
                }
                Ok(root)
            })
            .collect::<std::io::Result<Vec<_>>>()?;
        Ok(Self { roots })
    }

    fn resolve(&self, requested: &Path, allow_missing: bool) -> Result<PathBuf, ErrorData> {
        let requested = if requested.is_absolute() {
            requested.to_path_buf()
        } else {
            std::env::current_dir()
                .map_err(|error| crate::mcp_error(error.to_string()))?
                .join(requested)
        };
        let resolved = if requested.exists() {
            requested
                .canonicalize()
                .map_err(|error| crate::invalid(error.to_string()))?
        } else {
            if !allow_missing {
                return Err(crate::invalid(format!(
                    "workspace does not exist: {}",
                    requested.display()
                )));
            }
            let parent = requested
                .parent()
                .ok_or_else(|| crate::invalid("workspace path has no parent"))?
                .canonicalize()
                .map_err(|error| crate::invalid(error.to_string()))?;
            let name = requested
                .file_name()
                .ok_or_else(|| crate::invalid("workspace path must name a file"))?;
            parent.join(name)
        };
        if !self.roots.iter().any(|root| resolved.starts_with(root)) {
            return Err(crate::invalid(format!(
                "workspace is outside configured roots: {}",
                resolved.display()
            )));
        }
        Ok(resolved)
    }
}

/// Input for selecting another authorized workspace.
#[derive(Debug, Deserialize, rmcp::schemars::JsonSchema)]
pub struct SwitchWorkspaceParams {
    /// Existing workspace path, or an uninitialized path in write mode.
    pub path: String,
}

#[tool_router(router = workspace_switch_tool_router)]
impl MdtreeServer {
    pub(crate) fn switch_tool_router() -> ToolRouter<Self> {
        Self::workspace_switch_tool_router()
    }

    #[tool(description = "Switch atomically to another authorized MDTree workspace")]
    async fn switch_workspace(
        &self,
        Parameters(params): Parameters<SwitchWorkspaceParams>,
    ) -> Result<CallToolResult, ErrorData> {
        if params.path.trim().is_empty() {
            return Err(crate::invalid("path must not be blank"));
        }
        let policy = self
            .workspace_switch_policy
            .as_ref()
            .ok_or_else(|| crate::invalid("workspace switching is not enabled"))?;
        let path = policy.resolve(Path::new(&params.path), self.access_mode.allows_write())?;
        let replacement = if path.exists() {
            Some(
                mdtree_sqlite::SqliteStore::open(&path)
                    .map_err(|error| crate::mcp_error(error.to_string()))?,
            )
        } else {
            None
        };
        let status = replacement
            .as_ref()
            .map(|store| mdtree_sqlite::workspace_status(store.connection(), &path))
            .transpose()
            .map_err(|error| crate::mcp_error(error.to_string()))?;
        let mut binding = self
            .binding
            .lock()
            .map_err(|_| crate::mcp_error("workspace lock poisoned"))?;
        let previous_path = binding.path.clone();
        *binding = WorkspaceBinding {
            path: path.clone(),
            store: replacement,
        };
        crate::json_result(serde_json::json!({
            "status": "switched",
            "previous_path": previous_path,
            "path": path,
            "workspace": status,
            "initialized": status.is_some(),
        }))
    }
}

#[cfg(test)]
mod tests {
    use rmcp::handler::server::wrapper::Parameters;
    use tempfile::tempdir;

    use super::{SwitchWorkspaceParams, WorkspaceSwitchPolicy};
    use crate::{McpAccessMode, MdtreeServer};

    fn workspaces() -> (tempfile::TempDir, std::path::PathBuf, std::path::PathBuf) {
        let directory = tempdir().expect("temporary directory");
        let first = directory.path().join("first.mdtree");
        let second = directory.path().join("second.mdtree");
        mdtree_sqlite::import_snapshot_new(&first, &mdtree_core::northstar_platform_snapshot())
            .expect("first workspace");
        mdtree_sqlite::import_snapshot_new(&second, &mdtree_core::developer_workspace_snapshot())
            .expect("second workspace");
        (directory, first, second)
    }

    #[tokio::test]
    async fn switches_atomically_and_failed_switch_preserves_the_current_workspace() {
        let (directory, first, second) = workspaces();
        let policy = WorkspaceSwitchPolicy::new(vec![directory.path().to_path_buf()])
            .expect("workspace policy");
        let server = MdtreeServer::open_or_uninitialized_with_mode_and_policy(
            &first,
            McpAccessMode::ReadOnly,
            Some(policy),
        )
        .expect("server");
        assert!(server
            .tool_router
            .list_all()
            .iter()
            .any(|tool| tool.name == "switch_workspace"));

        server
            .switch_workspace(Parameters(SwitchWorkspaceParams {
                path: second.display().to_string(),
            }))
            .await
            .expect("switch workspace");
        {
            let binding = server.binding.lock().expect("binding");
            assert_eq!(
                binding.path,
                second.canonicalize().expect("canonical second")
            );
            assert_eq!(
                binding
                    .store
                    .as_ref()
                    .expect("store")
                    .root()
                    .expect("root")
                    .fields()
                    .metadata
                    .title,
                "Developer Workspace"
            );
        }

        let outside = tempdir().expect("outside directory");
        let outside_workspace = outside.path().join("outside.mdtree");
        mdtree_sqlite::import_snapshot_new(
            &outside_workspace,
            &mdtree_core::northstar_platform_snapshot(),
        )
        .expect("outside workspace");
        let error = server
            .switch_workspace(Parameters(SwitchWorkspaceParams {
                path: outside_workspace.display().to_string(),
            }))
            .await
            .expect_err("outside root must be rejected");
        assert!(error.message.contains("outside configured roots"));
        assert_eq!(
            server.binding.lock().expect("binding").path,
            second.canonicalize().expect("canonical second")
        );
    }

    #[tokio::test]
    async fn write_mode_can_select_and_initialize_an_authorized_missing_workspace() {
        let (directory, first, _second) = workspaces();
        let missing = directory.path().join("new.mdtree");
        let policy = WorkspaceSwitchPolicy::new(vec![directory.path().to_path_buf()])
            .expect("workspace policy");
        let server = MdtreeServer::open_or_uninitialized_with_mode_and_policy(
            &first,
            McpAccessMode::ReadWrite,
            Some(policy),
        )
        .expect("server");
        server
            .switch_workspace(Parameters(SwitchWorkspaceParams {
                path: missing.display().to_string(),
            }))
            .await
            .expect("select missing workspace");
        let binding = server.binding.lock().expect("binding");
        assert_eq!(binding.path, missing);
        assert!(binding.store.is_none());
    }

    #[tokio::test]
    async fn concurrent_observers_never_see_a_path_store_mismatch() {
        let (directory, first, second) = workspaces();
        let first = first.canonicalize().expect("canonical first");
        let second = second.canonicalize().expect("canonical second");
        let policy = WorkspaceSwitchPolicy::new(vec![directory.path().to_path_buf()])
            .expect("workspace policy");
        let server = MdtreeServer::open_or_uninitialized_with_mode_and_policy(
            &first,
            McpAccessMode::ReadOnly,
            Some(policy),
        )
        .expect("server");
        let switcher = server.clone();
        let first_for_switcher = first.clone();
        let second_for_switcher = second.clone();
        let switch_task = tokio::spawn(async move {
            for index in 0..100 {
                let path = if index % 2 == 0 {
                    &second_for_switcher
                } else {
                    &first_for_switcher
                };
                switcher
                    .switch_workspace(Parameters(SwitchWorkspaceParams {
                        path: path.display().to_string(),
                    }))
                    .await
                    .expect("switch workspace");
                tokio::task::yield_now().await;
            }
        });
        let observer = server.clone();
        let observe_task = tokio::spawn(async move {
            for _ in 0..200 {
                {
                    let binding = observer.binding.lock().expect("binding");
                    let title = binding
                        .store
                        .as_ref()
                        .expect("store")
                        .root()
                        .expect("root")
                        .fields()
                        .metadata
                        .title
                        .clone();
                    if binding.path == first {
                        assert_eq!(title, "Northstar Platform");
                    } else {
                        assert_eq!(binding.path, second);
                        assert_eq!(title, "Developer Workspace");
                    }
                }
                tokio::task::yield_now().await;
            }
        });
        switch_task.await.expect("switch task");
        observe_task.await.expect("observe task");
    }

    #[test]
    fn policy_rejects_empty_roots_and_normalizes_relative_paths() {
        assert!(WorkspaceSwitchPolicy::new(Vec::new()).is_err());
        let directory = tempdir().expect("temporary directory");
        let policy = WorkspaceSwitchPolicy::new(vec![directory.path().to_path_buf()])
            .expect("workspace policy");
        let target = directory.path().join("future.mdtree");
        assert_eq!(
            policy.resolve(&target, true).expect("authorized path"),
            target
        );
        assert!(policy.resolve(&target, false).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn policy_rejects_symlink_escape() {
        use std::os::unix::fs::symlink;

        let allowed = tempdir().expect("allowed directory");
        let outside = tempdir().expect("outside directory");
        let workspace = outside.path().join("outside.mdtree");
        mdtree_sqlite::import_snapshot_new(&workspace, &mdtree_core::northstar_platform_snapshot())
            .expect("outside workspace");
        let link = allowed.path().join("escape.mdtree");
        symlink(&workspace, &link).expect("workspace symlink");
        let policy = WorkspaceSwitchPolicy::new(vec![allowed.path().to_path_buf()])
            .expect("workspace policy");
        assert!(policy.resolve(&link, false).is_err());
    }
}
