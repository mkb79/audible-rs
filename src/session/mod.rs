//! Sessions, broker and agent (archived architecture §9): one component
//! with two lifetimes. The shared `/v1` HTTP router lives in [`rpc`]; the
//! ephemeral per-plugin-call lifetime is [`crate::plugins::broker`], the
//! long-lived [`agent`] holds a session map keyed by account with scoped
//! app tokens (M5). Shared socket/runtime-dir helpers live here.

use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result};

use crate::config::ctx::Ctx;

#[cfg(unix)]
pub mod agent;
pub mod audit;
#[cfg(unix)]
pub mod client;
pub mod rpc;
pub mod tokens;

/// Base directory for agent/broker sockets: `[session].socket_dir`, else
/// `$XDG_RUNTIME_DIR`, else the platform data dir.
pub fn runtime_dir(ctx: &Ctx) -> PathBuf {
    if let Some(dir) = &ctx.config().session.socket_dir {
        return crate::naming::expand_tilde(dir);
    }
    if let Some(runtime) = std::env::var_os("XDG_RUNTIME_DIR") {
        let dir = PathBuf::from(runtime);
        if dir.is_dir() {
            return dir;
        }
    }
    crate::config::paths::data_dir()
}

/// Creates a directory with 0700 permissions (owner-only), idempotently.
pub fn create_private_dir(dir: &Path) -> Result<()> {
    std::fs::create_dir_all(dir).with_context(|| format!("could not create {}", dir.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}
