//! `MDTree` MCP stdio executable.

use rmcp::ServiceExt;

const DEFAULT_WORKSPACE: &str = ".mdtree";

fn resolve_workspace(
    argument: Option<std::ffi::OsString>,
    environment: Option<std::ffi::OsString>,
    fallback_argument: Option<std::ffi::OsString>,
    fallback_environment: Option<std::ffi::OsString>,
    default: &std::path::Path,
) -> std::ffi::OsString {
    argument.or(environment).unwrap_or_else(|| {
        if default.exists() {
            default.as_os_str().to_owned()
        } else {
            fallback_argument
                .or(fallback_environment)
                .unwrap_or_else(|| default.as_os_str().to_owned())
        }
    })
}

fn workspace_switch_policy(
    allow: bool,
    roots: Vec<std::ffi::OsString>,
) -> anyhow::Result<Option<mdtree_mcp::WorkspaceSwitchPolicy>> {
    if !allow && !roots.is_empty() {
        return Err(anyhow::anyhow!(
            "--workspace-root requires --allow-workspace-switch"
        ));
    }
    allow
        .then(|| {
            mdtree_mcp::WorkspaceSwitchPolicy::new(
                roots.into_iter().map(std::path::PathBuf::from).collect(),
            )
        })
        .transpose()
        .map_err(Into::into)
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .init();
    let mut workspace = None;
    let mut fallback_workspace = None;
    let mut workspace_roots = Vec::new();
    let mut allow_workspace_switch = false;
    let mut ollama_url = std::env::var("MDTREE_OLLAMA_URL")
        .unwrap_or_else(|_| mdtree_semantic::DEFAULT_OLLAMA_BASE_URL.into());
    let mut ollama_model = std::env::var("MDTREE_OLLAMA_MODEL").ok();
    let mut ollama_timeout_seconds = std::env::var("MDTREE_OLLAMA_TIMEOUT_SECONDS")
        .ok()
        .map(|value| value.parse::<u64>())
        .transpose()?
        .unwrap_or(60);
    let mut allow_write = std::env::var("MDTREE_MCP_ALLOW_WRITE")
        .is_ok_and(|value| matches!(value.as_str(), "1" | "true" | "yes"));
    let mut arguments = std::env::args_os().skip(1);
    while let Some(argument) = arguments.next() {
        if argument == "--allow-write" {
            allow_write = true;
        } else if argument == "--allow-workspace-switch" {
            allow_workspace_switch = true;
        } else if argument == "--workspace-root" {
            workspace_roots.push(
                arguments
                    .next()
                    .ok_or_else(|| anyhow::anyhow!("--workspace-root requires a path"))?,
            );
        } else if argument == "--fallback-workspace" {
            let path = arguments
                .next()
                .ok_or_else(|| anyhow::anyhow!("--fallback-workspace requires a workspace path"))?;
            if fallback_workspace.replace(path).is_some() {
                return Err(anyhow::anyhow!(
                    "only one fallback workspace path may be supplied"
                ));
            }
        } else if argument == "--ollama-url" {
            ollama_url = arguments
                .next()
                .ok_or_else(|| anyhow::anyhow!("--ollama-url requires a URL"))?
                .into_string()
                .map_err(|_| anyhow::anyhow!("--ollama-url must be valid UTF-8"))?;
        } else if argument == "--ollama-model" {
            ollama_model = Some(
                arguments
                    .next()
                    .ok_or_else(|| anyhow::anyhow!("--ollama-model requires a model"))?
                    .into_string()
                    .map_err(|_| anyhow::anyhow!("--ollama-model must be valid UTF-8"))?,
            );
        } else if argument == "--ollama-timeout-seconds" {
            ollama_timeout_seconds = arguments
                .next()
                .ok_or_else(|| anyhow::anyhow!("--ollama-timeout-seconds requires a value"))?
                .into_string()
                .map_err(|_| anyhow::anyhow!("--ollama-timeout-seconds must be valid UTF-8"))?
                .parse()?;
        } else if workspace.replace(argument).is_some() {
            return Err(anyhow::anyhow!("only one workspace path may be supplied"));
        }
    }
    let workspace = resolve_workspace(
        workspace,
        std::env::var_os("MDTREE_WORKSPACE"),
        fallback_workspace,
        std::env::var_os("MDTREE_FALLBACK_WORKSPACE"),
        std::path::Path::new(DEFAULT_WORKSPACE),
    );
    let mode = if allow_write {
        mdtree_mcp::McpAccessMode::ReadWrite
    } else {
        mdtree_mcp::McpAccessMode::ReadOnly
    };
    let workspace_switch_policy = workspace_switch_policy(allow_workspace_switch, workspace_roots)?;
    let semantic = mdtree_mcp::McpSemanticConfig {
        ollama: mdtree_semantic::OllamaConfig {
            base_url: ollama_url,
            timeout: std::time::Duration::from_secs(ollama_timeout_seconds),
        },
        model: ollama_model,
    };
    mdtree_semantic::OllamaProvider::new(&semantic.ollama)
        .map_err(|error| anyhow::anyhow!(error.to_string()))?;
    let server = mdtree_mcp::MdtreeServer::open_or_uninitialized_with_mode_policy_and_semantic(
        std::path::Path::new(&workspace),
        mode,
        workspace_switch_policy,
        semantic,
    )?;
    server
        .serve(rmcp::transport::stdio())
        .await?
        .waiting()
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::{ffi::OsString, fs::File};

    use tempfile::tempdir;

    use super::{resolve_workspace, workspace_switch_policy, DEFAULT_WORKSPACE};

    #[test]
    fn explicit_workspace_uses_argument_then_environment() {
        let directory = tempdir().expect("temporary directory");
        let default = directory.path().join(DEFAULT_WORKSPACE);
        File::create(&default).expect("local workspace marker");

        assert_eq!(
            resolve_workspace(
                Some("argument.mdtree".into()),
                Some("env.mdtree".into()),
                Some("fallback.mdtree".into()),
                None,
                &default,
            ),
            OsString::from("argument.mdtree")
        );
        assert_eq!(
            resolve_workspace(
                None,
                Some("env.mdtree".into()),
                Some("fallback.mdtree".into()),
                None,
                &default,
            ),
            OsString::from("env.mdtree")
        );
    }

    #[test]
    fn local_default_takes_precedence_over_fallback_workspace() {
        let directory = tempdir().expect("temporary directory");
        let default = directory.path().join(DEFAULT_WORKSPACE);
        File::create(&default).expect("local workspace marker");

        assert_eq!(
            resolve_workspace(
                None,
                None,
                Some("fallback.mdtree".into()),
                Some("environment-fallback.mdtree".into()),
                &default,
            ),
            default.as_os_str().to_owned()
        );
    }

    #[test]
    fn fallback_workspace_is_used_only_when_local_default_is_missing() {
        let directory = tempdir().expect("temporary directory");
        let default = directory.path().join(DEFAULT_WORKSPACE);

        assert_eq!(
            resolve_workspace(
                None,
                None,
                Some("fallback.mdtree".into()),
                Some("environment-fallback.mdtree".into()),
                &default,
            ),
            OsString::from("fallback.mdtree")
        );
        assert_eq!(
            resolve_workspace(
                None,
                None,
                None,
                Some("environment-fallback.mdtree".into()),
                &default,
            ),
            OsString::from("environment-fallback.mdtree")
        );
        assert_eq!(
            resolve_workspace(None, None, None, None, &default),
            default.as_os_str().to_owned()
        );
    }

    #[test]
    fn workspace_switching_requires_enablement_and_existing_roots() {
        let directory = tempdir().expect("temporary directory");
        assert!(workspace_switch_policy(false, vec![directory.path().into()]).is_err());
        assert!(workspace_switch_policy(true, Vec::new()).is_err());
        assert!(workspace_switch_policy(true, vec![directory.path().into()])
            .expect("workspace policy")
            .is_some());
    }
}
