//! Management-API bearer token resolution.
//!
//! HTTPS always, auth always (including loopback). Precedence: env (an operator
//! override, persisted so every reader agrees) → `<config-dir>` file → generate
//! 32-byte hex and persist. Files are `KEY=<hex>` at 0600.
//!
//! [`take_env_credentials`] empties those variables out of this process before
//! anything can inherit them: hooks, games and the plugin runner are our
//! children, and none of them has business with the admin API.
//!
//! Two tokens:
//! - **`mgmt-token`** (`PUNKTFUNK_MGMT_TOKEN`) — full admin.
//! - **`plugin-token`** (`PUNKTFUNK_PLUGIN_TOKEN`) — `mgmt::auth::plugin_may_access`.
//!   The SDK `connect()` prefers this file so a plugin cannot rewrite
//!   `hooks.json` or admit devices.

use anyhow::{Context, Result};
use rand::RngCore;
use std::collections::BTreeMap;
use std::fs;
use std::path::Path;
use std::sync::OnceLock;

/// What the environment carried at startup, for [`load_or_generate_impl`] to persist.
static ENV_TOKENS: OnceLock<[Option<String>; 2]> = OnceLock::new();

/// The variables `main` takes out of the environment before anything can inherit them. The
/// console password is here because no child of ours may see it either.
pub const CREDENTIAL_ENV_VARS: [&str; 3] = [ENV_VAR, PLUGIN_ENV_VAR, "PUNKTFUNK_UI_PASSWORD"];

/// Remember what the environment carried, once, before `main` empties it.
///
/// The values are not lost by that removal — every reader goes through [`load_or_generate`],
/// which persists an operator's pinned token to the file the other readers use.
pub fn adopt_env_tokens(mgmt: Option<String>, plugin: Option<String>) {
    let _ = ENV_TOKENS.set([mgmt, plugin]);
}

fn env_token(env_var: &str) -> Option<String> {
    let tokens = ENV_TOKENS.get()?;
    let slot = if env_var == PLUGIN_ENV_VAR { 1 } else { 0 };
    tokens[slot].clone()
}

const ENV_VAR: &str = "PUNKTFUNK_MGMT_TOKEN";
const FILE: &str = "mgmt-token";
const PLUGIN_ENV_VAR: &str = "PUNKTFUNK_PLUGIN_TOKEN";
const PLUGIN_FILE: &str = "plugin-token";
/// `{ "<plugin id>": "<token>" }` — see [`load_or_generate_per_plugin`].
const PER_PLUGIN_FILE: &str = "plugin-tokens.json";

/// Admin token: env > file > generate+persist. Hex so `KEY=VALUE` is safe
/// to source from a shell or systemd `EnvironmentFile`.
pub fn load_or_generate() -> Result<String> {
    load_or_generate_impl(ENV_VAR, FILE)
}

/// One token per installed plugin, by plugin id, in `plugin-run/plugin-tokens.json`.
///
/// The shared [`load_or_generate_plugin`] token authenticates the RUNNER; these authenticate a
/// PLUGIN, which is what lets the management API refuse a plugin writing another's registration.
/// Keyed by what is installed now: every install path calls this before it restarts the runner.
pub fn load_or_generate_per_plugin() -> Result<BTreeMap<String, String>> {
    let ids: Vec<String> = crate::plugins::manifest::installed().into_keys().collect();
    load_or_generate_per_plugin_in(&pf_paths::config_dir(), &ids)
}

/// The per-plugin tokens on disk under `config_dir`, or `None` while the file is missing or
/// mid-rewrite. Never mints: the host reads this when another process minted.
pub(crate) fn read_per_plugin(config_dir: &Path) -> Option<BTreeMap<String, String>> {
    let path = config_dir
        .join(crate::plugins::RUNNER_DATA_DIR)
        .join(PER_PLUGIN_FILE);
    serde_json::from_str(&fs::read_to_string(path).ok()?).ok()
}

fn load_or_generate_per_plugin_in(
    config_dir: &Path,
    ids: &[String],
) -> Result<BTreeMap<String, String>> {
    let dir = config_dir.join(crate::plugins::RUNNER_DATA_DIR);
    let path = dir.join(PER_PLUGIN_FILE);
    let planted = crate::planted::quarantine_planted_secret(&path);
    pf_paths::create_secret_dir(&dir).with_context(|| format!("create {}", dir.display()))?;
    let current = (!planted).then(|| fs::read_to_string(&path).ok()).flatten();
    let legacy = config_dir.join(PER_PLUGIN_FILE);
    let migrated =
        current.is_none() && !crate::planted::quarantine_planted_secret(&legacy) && legacy.exists();
    let source = current.or_else(|| migrated.then(|| fs::read_to_string(&legacy).ok()).flatten());
    let mut tokens: BTreeMap<String, String> = source
        .as_deref()
        .and_then(|text| serde_json::from_str(text).ok())
        .unwrap_or_default();
    let before = tokens.clone();
    tokens.retain(|id, _| ids.contains(id));
    for id in ids {
        tokens.entry(id.clone()).or_insert_with(|| {
            let mut buf = [0u8; 32];
            rand::rng().fill_bytes(&mut buf);
            hex::encode(buf)
        });
    }
    if tokens != before || migrated || !path.exists() {
        let body = serde_json::to_string_pretty(&tokens)?;
        pf_paths::write_secret_file(&path, body.as_bytes())
            .with_context(|| format!("write {}", path.display()))?;
        tracing::info!(
            path = %path.display(),
            plugins = tokens.len(),
            "minted per-plugin API tokens (owner-only)"
        );
    }
    // Unconditional: `create_secret_dir` above resets the directory ACL on Windows even when
    // the file is unchanged. A failure costs the runner, never `serve`.
    if let Err(e) = crate::plugins::converge_runner_credential(&path) {
        tracing::warn!(path = %path.display(), error = %format!("{e:#}"), "runner token grant did not apply");
    }
    Ok(tokens)
}

/// Plugin-lane token, same precedence as [`load_or_generate`].
///
/// On Windows, `plugins enable` grants LocalService read on this file and
/// `cert.pem` — never `mgmt-token`.
pub fn load_or_generate_plugin() -> Result<String> {
    load_or_generate_impl(PLUGIN_ENV_VAR, PLUGIN_FILE)
}

/// Persisted operator token in `dir`, or `None`. Never mints.
///
/// `ctl` must not generate: a client-minted file would become the host
/// credential. Ignores `PUNKTFUNK_MGMT_TOKEN` so a consumer does not publish
/// the token in `/proc/<pid>/environ`.
pub(crate) fn read_persisted(dir: &Path) -> Option<String> {
    let contents = fs::read_to_string(dir.join(FILE)).ok()?;
    parse_token(&contents, ENV_VAR)
}

fn load_or_generate_impl(env_var: &str, file: &str) -> Result<String> {
    let dir = pf_paths::config_dir();
    let path = dir.join(file);
    // An operator override is PERSISTED rather than kept in memory: the console, `ctl`, the tray
    // and the plugin runner all read the file, and a token only this process knew locked them out.
    if let Some(pinned) = env_token(env_var) {
        if parse_token(&fs::read_to_string(&path).unwrap_or_default(), env_var).as_deref()
            != Some(pinned.as_str())
        {
            // A seat's config directory doesn't exist before its host's first start.
            pf_paths::create_private_dir(&dir)
                .with_context(|| format!("create {}", dir.display()))?;
            write_token(&path, env_var, &pinned)?;
        }
        return Ok(pinned);
    }
    // Locking the dir does not disown a file already in it. Read the owner first: a token a
    // local user planted before the first elevated run is theirs, and adopting it would hand
    // them host admin. `create_private_dir` re-owns contents, so this must come before it.
    let planted = crate::planted::quarantine_planted_secret(&path);
    pf_paths::create_private_dir(&dir).with_context(|| format!("create {}", dir.display()))?;
    if !planted {
        if let Ok(contents) = fs::read_to_string(&path) {
            if let Some(tok) = parse_token(&contents, env_var) {
                return Ok(tok);
            }
        }
    }
    let mut buf = [0u8; 32];
    rand::rng().fill_bytes(&mut buf);
    let token = hex::encode(buf);
    write_token(&path, env_var, &token)?;
    tracing::info!(path = %path.display(), "generated and persisted API token (owner-only)");
    Ok(token)
}

/// First non-empty line: bare token or `<KEY>=<token>` (EnvironmentFile).
fn parse_token(contents: &str, env_var: &str) -> Option<String> {
    let line = contents.lines().find(|l| !l.trim().is_empty())?.trim();
    let tok = line
        .strip_prefix(env_var)
        .and_then(|rest| rest.strip_prefix('='))
        .unwrap_or(line)
        .trim();
    (!tok.is_empty()).then(|| tok.to_string())
}

/// Owner-only `KEY=token` via `pf_paths::write_secret_file` (0600 Unix;
/// SYSTEM/Administrators DACL on Windows). Same lockdown as the host key.
fn write_token(path: &Path, env_var: &str, token: &str) -> Result<()> {
    let line = format!("{env_var}={token}\n");
    pf_paths::write_secret_file(path, line.as_bytes())
        .with_context(|| format!("write {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_bare_and_keyvalue_forms() {
        assert_eq!(parse_token("abc123\n", ENV_VAR).as_deref(), Some("abc123"));
        assert_eq!(
            parse_token("PUNKTFUNK_MGMT_TOKEN=deadbeef\n", ENV_VAR).as_deref(),
            Some("deadbeef")
        );
        assert_eq!(
            parse_token("PUNKTFUNK_PLUGIN_TOKEN=deadbeef\n", PLUGIN_ENV_VAR).as_deref(),
            Some("deadbeef")
        );
        assert_eq!(parse_token("\n  \n", ENV_VAR), None);
        assert_eq!(parse_token("PUNKTFUNK_MGMT_TOKEN=\n", ENV_VAR), None);
    }

    #[test]
    fn generated_token_round_trips_through_the_file() {
        let dir = std::env::temp_dir().join(format!("pf-mgmt-token-test-{}", std::process::id()));
        let _ = fs::create_dir_all(&dir);
        let path = dir.join(FILE);
        write_token(&path, ENV_VAR, "cafef00d").unwrap();
        let read = fs::read_to_string(&path).unwrap();
        assert_eq!(parse_token(&read, ENV_VAR).as_deref(), Some("cafef00d"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }
        let _ = fs::remove_dir_all(&dir);
    }

    #[cfg(not(windows))]
    #[test]
    fn per_plugin_tokens_migrate_without_rotating() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join(PER_PLUGIN_FILE),
            r#"{"demo":"keep-me","gone":"discard-me"}"#,
        )
        .unwrap();
        let tokens =
            load_or_generate_per_plugin_in(dir.path(), &["demo".to_string(), "new".to_string()])
                .unwrap();
        assert_eq!(tokens.get("demo").map(String::as_str), Some("keep-me"));
        assert_eq!(tokens.len(), 2);
        assert!(!tokens.contains_key("gone"));
        let migrated = fs::read_to_string(
            dir.path()
                .join(crate::plugins::RUNNER_DATA_DIR)
                .join(PER_PLUGIN_FILE),
        )
        .unwrap();
        assert_eq!(
            serde_json::from_str::<BTreeMap<_, _>>(&migrated).unwrap(),
            tokens
        );
    }
}
