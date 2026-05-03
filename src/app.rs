use std::fs;
use std::path::PathBuf;

use anyhow::{Context, Result};
use directories::ProjectDirs;

#[derive(Debug, Clone)]
pub struct AppPaths {
    pub root: PathBuf,
    pub certs_dir: PathBuf,
    pub logs_dir: PathBuf,
}

impl AppPaths {
    pub fn resolve() -> Result<Self> {
        let project_dirs = ProjectDirs::from("ai", "navilan", "agent-mcp-b")
            .context("failed to resolve platform config directory")?;

        let root = project_dirs.config_dir().to_path_buf();
        let certs_dir = root.join("certs");
        let logs_dir = root.join("logs");

        Ok(Self {
            root,
            certs_dir,
            logs_dir,
        })
    }

    pub fn ensure(&self) -> Result<()> {
        fs::create_dir_all(&self.certs_dir).context("failed to create cert directory")?;
        fs::create_dir_all(&self.logs_dir).context("failed to create log directory")?;
        Ok(())
    }
}
