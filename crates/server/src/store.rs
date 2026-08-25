//! The server's own on-disk state.
//!
//! Everything here is deliberately *not* in SpacetimeDB: the operator
//! credential, agent tokens, the GitHub App private key and application
//! environment. SpacetimeDB 2.0.2 has no enforced row-level security, so any
//! table an agent can read, every agent can read — one host's database
//! password would be visible to the whole fleet. Secrets stay on this disk
//! and travel only over the server's authenticated HTTP channel.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use argon2::Argon2;
use base64::Engine;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::sync::RwLock;

use crate::github::AppConfig;

#[derive(Default, Serialize, Deserialize)]
pub struct Persisted {
    /// Argon2 hash of the operator password. None means the instance has not
    /// been claimed yet and first-run setup is still open.
    #[serde(default)]
    pub operator_hash: Option<String>,

    #[serde(default)]
    pub github: Option<AppConfig>,

    /// SHA-256 of each agent token -> the host name it was issued for. Only
    /// the hash is kept: a leaked state file must not let anyone impersonate
    /// an agent.
    #[serde(default)]
    pub agent_tokens: HashMap<String, String>,

    /// Enrolment codes minted but not yet redeemed, so the install command can
    /// be shown again before an agent uses it.
    #[serde(default)]
    pub pending_enrollments: HashMap<String, String>,

    /// Per-application environment, keyed by application name.
    #[serde(default)]
    pub app_env: HashMap<String, Vec<EnvVar>>,

    /// Named operator accounts, keyed by username. Replaces the single
    /// `operator_hash` password; a legacy file is migrated on open.
    #[serde(default)]
    pub users: HashMap<String, UserAccount>,

    /// API tokens for scripting the operator actions, keyed by token hash.
    #[serde(default)]
    pub api_tokens: HashMap<String, ApiTokenMeta>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct UserAccount {
    pub hash: String,
    pub admin: bool,
    #[serde(default)]
    pub created_unix: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ApiTokenMeta {
    pub name: String,
    #[serde(default)]
    pub created_unix: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct EnvVar {
    pub key: String,
    pub value: String,
    /// Secret values are never shown back to the operator, only their names.
    #[serde(default)]
    pub secret: bool,
}

impl EnvVar {
    /// What the operator sees. Once a secret is stored it is write-only.
    pub fn display_value(&self) -> &str {
        if self.secret {
            "••••••••"
        } else {
            &self.value
        }
    }
}

/// Parse `KEY=value` lines into variables.
///
/// Blank lines and `#` comments are skipped so an operator can paste a `.env`
/// file straight in. Values keep any `=` after the first one, and surrounding
/// quotes are stripped because `.env` files usually carry them.
pub fn parse_env(text: &str, secret: bool) -> Result<Vec<EnvVar>, String> {
    let mut vars = Vec::new();
    for (n, raw) in text.lines().enumerate() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            return Err(format!("line {}: expected KEY=value, got {line}", n + 1));
        };

        let key = key.trim();
        if key.is_empty() {
            return Err(format!("line {}: missing a name before '='", n + 1));
        }
        // Environment names a shell cannot express are a trap, not a feature.
        if !key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
            return Err(format!(
                "line {}: {key} is not a usable name — letters, digits and underscores only",
                n + 1
            ));
        }

        let value = value.trim();
        let value = value
            .strip_prefix('"')
            .and_then(|v| v.strip_suffix('"'))
            .or_else(|| value.strip_prefix('\'').and_then(|v| v.strip_suffix('\'')))
            .unwrap_or(value);

        vars.push(EnvVar {
            key: key.to_string(),
            value: value.to_string(),
            secret,
        });
    }
    Ok(vars)
}

#[derive(Clone)]
pub struct Store {
    path: PathBuf,
    inner: Arc<RwLock<Persisted>>,
}

impl Store {
    pub async fn open(dir: &Path) -> Result<Self> {
        tokio::fs::create_dir_all(dir)
            .await
            .with_context(|| format!("creating data directory {}", dir.display()))?;
        let path = dir.join("digihost.json");

        let mut loaded: Persisted = match tokio::fs::read_to_string(&path).await {
            Ok(raw) => serde_json::from_str(&raw)
                .with_context(|| format!("parsing {}", path.display()))?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Persisted::default(),
            Err(e) => return Err(e).context("reading server state"),
        };

        // Instances claimed before named accounts existed carry one shared
        // password. It becomes the `admin` account, password unchanged, so an
        // update never locks anyone out.
        let migrated = if loaded.users.is_empty() {
            if let Some(hash) = loaded.operator_hash.take() {
                loaded.users.insert(
                    "admin".to_string(),
                    UserAccount { hash, admin: true, created_unix: now_unix() },
                );
                true
            } else {
                false
            }
        } else {
            false
        };

        let store = Self {
            path,
            inner: Arc::new(RwLock::new(loaded)),
        };
        if migrated {
            let snapshot = clone_persisted(&*store.inner.read().await);
            store.flush(&snapshot).await?;
        }
        Ok(store)
    }

    async fn flush(&self, data: &Persisted) -> Result<()> {
        let encoded = serde_json::to_string_pretty(data)?;
        // Write-then-rename: a crash mid-write must not leave a truncated file
        // that would lock every operator and agent out of the instance.
        let tmp = self.path.with_extension("json.tmp");
        tokio::fs::write(&tmp, encoded)
            .await
            .context("writing server state")?;
        tokio::fs::rename(&tmp, &self.path)
            .await
            .context("replacing server state")?;
        Ok(())
    }

    // ---------------------------------------------------------------- users

    pub async fn is_claimed(&self) -> bool {
        !self.inner.read().await.users.is_empty()
    }

    /// Verify a login. Returns the account's admin flag on success.
    pub async fn verify_login(&self, name: &str, password: &str) -> Option<bool> {
        let account = self.inner.read().await.users.get(&normalise(name)).cloned()?;
        verify_hash(&account.hash, password).then_some(account.admin)
    }

    /// A user's admin flag — None when the account no longer exists, which is
    /// how sessions of deleted users die.
    pub async fn user_admin(&self, name: &str) -> Option<bool> {
        self.inner
            .read()
            .await
            .users
            .get(&normalise(name))
            .map(|u| u.admin)
    }

    pub async fn add_user(&self, name: &str, password: &str, admin: bool) -> Result<String> {
        let name = normalise(name);
        if !name_ok(&name) {
            anyhow::bail!(
                "usernames are 1-32 lowercase letters, digits, '-', '_' or '.' — got {name:?}"
            );
        }
        let hash = hash_password(password)?;

        let mut guard = self.inner.write().await;
        if guard.users.contains_key(&name) {
            anyhow::bail!("user {name} already exists");
        }
        guard.users.insert(
            name.clone(),
            UserAccount { hash, admin, created_unix: now_unix() },
        );
        let snapshot = clone_persisted(&guard);
        drop(guard);
        self.flush(&snapshot).await?;
        Ok(name)
    }

    pub async fn set_user_password(&self, name: &str, password: &str) -> Result<()> {
        let name = normalise(name);
        let hash = hash_password(password)?;

        let mut guard = self.inner.write().await;
        let account = guard
            .users
            .get_mut(&name)
            .ok_or_else(|| anyhow::anyhow!("no user named {name}"))?;
        account.hash = hash;
        let snapshot = clone_persisted(&guard);
        drop(guard);
        self.flush(&snapshot).await
    }

    /// Remove an account — but never the last administrator, which would lock
    /// the instance permanently.
    pub async fn remove_user(&self, name: &str) -> Result<()> {
        let name = normalise(name);
        let mut guard = self.inner.write().await;
        let target = guard
            .users
            .get(&name)
            .ok_or_else(|| anyhow::anyhow!("no user named {name}"))?;
        if target.admin {
            let admins = guard.users.values().filter(|u| u.admin).count();
            if admins <= 1 {
                anyhow::bail!("cannot remove the last administrator");
            }
        }
        guard.users.remove(&name);
        let snapshot = clone_persisted(&guard);
        drop(guard);
        self.flush(&snapshot).await
    }

    /// (name, admin, created) sorted by name.
    pub async fn users(&self) -> Vec<(String, bool, u64)> {
        let mut out: Vec<(String, bool, u64)> = self
            .inner
            .read()
            .await
            .users
            .iter()
            .map(|(n, u)| (n.clone(), u.admin, u.created_unix))
            .collect();
        out.sort_by(|a, b| a.0.cmp(&b.0));
        out
    }

    // ----------------------------------------------------------- API tokens

    /// Mint a named API token. The token itself is returned exactly once;
    /// only its hash is stored.
    pub async fn mint_api_token(&self, name: &str) -> Result<String> {
        let name = name.trim().to_string();
        if name.is_empty() {
            anyhow::bail!("give the token a name so it can be recognised later");
        }
        let mut guard = self.inner.write().await;
        if guard.api_tokens.values().any(|t| t.name == name) {
            anyhow::bail!("a token named {name} already exists");
        }
        let token = generate_token()?;
        guard.api_tokens.insert(
            hash_token(&token),
            ApiTokenMeta { name, created_unix: now_unix() },
        );
        let snapshot = clone_persisted(&guard);
        drop(guard);
        self.flush(&snapshot).await?;
        Ok(token)
    }

    /// The name behind a presented API token, if it is valid.
    pub async fn api_token_name(&self, token: &str) -> Option<String> {
        self.inner
            .read()
            .await
            .api_tokens
            .get(&hash_token(token))
            .map(|t| t.name.clone())
    }

    pub async fn revoke_api_token(&self, name: &str) -> Result<()> {
        let mut guard = self.inner.write().await;
        let before = guard.api_tokens.len();
        guard.api_tokens.retain(|_, t| t.name != name);
        if guard.api_tokens.len() == before {
            anyhow::bail!("no token named {name}");
        }
        let snapshot = clone_persisted(&guard);
        drop(guard);
        self.flush(&snapshot).await
    }

    /// (name, created) sorted newest first.
    pub async fn api_tokens(&self) -> Vec<(String, u64)> {
        let mut out: Vec<(String, u64)> = self
            .inner
            .read()
            .await
            .api_tokens
            .values()
            .map(|t| (t.name.clone(), t.created_unix))
            .collect();
        out.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        out
    }

    // --------------------------------------------------------------- GitHub

    pub async fn github(&self) -> Option<AppConfig> {
        self.inner.read().await.github.clone()
    }

    pub async fn set_github(&self, config: Option<AppConfig>) -> Result<()> {
        let mut guard = self.inner.write().await;
        guard.github = config;
        let snapshot = clone_persisted(&guard);
        drop(guard);
        self.flush(&snapshot).await
    }

    // --------------------------------------------------------------- agents

    /// Record a minted enrolment code so the install command can be shown
    /// again until an agent redeems it.
    pub async fn remember_enrollment(&self, code: &str, region: &str) -> Result<()> {
        let mut guard = self.inner.write().await;
        guard
            .pending_enrollments
            .insert(code.to_string(), region.to_string());
        let snapshot = clone_persisted(&guard);
        drop(guard);
        self.flush(&snapshot).await
    }

    /// Redeem an enrolment code for a long-lived agent token. Returns None if
    /// the code was never minted here or is already spent — single-use on the
    /// HTTP side just as it is in the control plane.
    pub async fn redeem_enrollment(&self, code: &str, host_name: &str) -> Result<Option<String>> {
        let mut guard = self.inner.write().await;
        if guard.pending_enrollments.remove(code).is_none() {
            return Ok(None);
        }

        let token = generate_token()?;
        guard
            .agent_tokens
            .insert(hash_token(&token), host_name.to_string());

        let snapshot = clone_persisted(&guard);
        drop(guard);
        self.flush(&snapshot).await?;
        Ok(Some(token))
    }

    /// The host an agent token speaks for, or None if it is not a valid token.
    pub async fn agent_for(&self, token: &str) -> Option<String> {
        self.inner
            .read()
            .await
            .agent_tokens
            .get(&hash_token(token))
            .cloned()
    }

    // ------------------------------------------------------------------ env

    pub async fn env_for(&self, app: &str) -> Vec<EnvVar> {
        self.inner
            .read()
            .await
            .app_env
            .get(app)
            .cloned()
            .unwrap_or_default()
    }

    /// Merge variables into an application's environment. Later definitions of
    /// the same name win, so re-submitting a key updates rather than
    /// duplicates it.
    pub async fn set_env(&self, app: &str, incoming: Vec<EnvVar>) -> Result<()> {
        let mut guard = self.inner.write().await;
        let existing = guard.app_env.entry(app.to_string()).or_default();
        for var in incoming {
            match existing.iter_mut().find(|e| e.key == var.key) {
                Some(slot) => *slot = var,
                None => existing.push(var),
            }
        }
        existing.sort_by(|a, b| a.key.cmp(&b.key));

        let snapshot = clone_persisted(&guard);
        drop(guard);
        self.flush(&snapshot).await
    }

    /// Forget an application's entire environment — for when the application
    /// itself is removed.
    pub async fn clear_env(&self, app: &str) -> Result<()> {
        let mut guard = self.inner.write().await;
        guard.app_env.remove(app);
        let snapshot = clone_persisted(&guard);
        drop(guard);
        self.flush(&snapshot).await
    }

    pub async fn remove_env(&self, app: &str, key: &str) -> Result<()> {
        let mut guard = self.inner.write().await;
        if let Some(existing) = guard.app_env.get_mut(app) {
            existing.retain(|e| e.key != key);
        }
        let snapshot = clone_persisted(&guard);
        drop(guard);
        self.flush(&snapshot).await
    }
}

fn clone_persisted(data: &Persisted) -> Persisted {
    Persisted {
        operator_hash: data.operator_hash.clone(),
        github: data.github.clone(),
        agent_tokens: data.agent_tokens.clone(),
        pending_enrollments: data.pending_enrollments.clone(),
        app_env: data.app_env.clone(),
        users: data.users.clone(),
        api_tokens: data.api_tokens.clone(),
    }
}

fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn normalise(name: &str) -> String {
    name.trim().to_ascii_lowercase()
}

fn name_ok(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 32
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '-' | '_' | '.'))
}

fn hash_password(password: &str) -> Result<String> {
    // argon2's own salt generator sits behind an RNG feature that moved
    // between releases; drawing the salt ourselves keeps this stable.
    let mut salt_bytes = [0u8; 16];
    getrandom::fill(&mut salt_bytes)
        .map_err(|e| anyhow::anyhow!("reading system randomness: {e}"))?;
    let salt = SaltString::encode_b64(&salt_bytes)
        .map_err(|e| anyhow::anyhow!("encoding password salt: {e}"))?;
    Ok(Argon2::default()
        .hash_password(password.as_bytes(), &salt)
        .map_err(|e| anyhow::anyhow!("hashing password: {e}"))?
        .to_string())
}

fn verify_hash(stored: &str, password: &str) -> bool {
    let Ok(parsed) = PasswordHash::new(stored) else {
        return false;
    };
    Argon2::default()
        .verify_password(password.as_bytes(), &parsed)
        .is_ok()
}

/// 256 bits of OS randomness, URL-safe so it drops into a shell command.
pub fn generate_token() -> Result<String> {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).map_err(|e| anyhow::anyhow!("reading system randomness: {e}"))?;
    Ok(base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes))
}

/// Tokens are high-entropy random values, so a plain SHA-256 is the right
/// lookup key — nothing to brute-force, no need for a slow KDF.
fn hash_token(token: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(token.as_bytes());
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(hasher.finalize())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch_dir(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!("digihost-store-{tag}-{}", generate_token().unwrap()))
    }

    #[test]
    fn tokens_are_distinct_and_long() {
        let a = generate_token().unwrap();
        let b = generate_token().unwrap();
        assert_ne!(a, b);
        assert!(a.len() >= 40, "token too short: {a}");
    }

    #[test]
    fn hashing_is_stable_and_not_the_token() {
        let token = generate_token().unwrap();
        assert_eq!(hash_token(&token), hash_token(&token));
        assert_ne!(hash_token(&token), token);
    }

    #[test]
    fn parses_dotenv_style_input() {
        let vars = parse_env(
            "# comment\nDATABASE_URL=postgres://x\n\nPORT = 8080\nQUOTED=\"a b\"\n",
            false,
        )
        .unwrap();
        assert_eq!(vars.len(), 3);
        assert_eq!(vars[0].key, "DATABASE_URL");
        assert_eq!(vars[0].value, "postgres://x");
        assert_eq!(vars[1].value, "8080");
        assert_eq!(vars[2].value, "a b", "surrounding quotes should be stripped");
    }

    #[test]
    fn keeps_equals_signs_inside_values() {
        let vars = parse_env("TOKEN=abc=def==", false).unwrap();
        assert_eq!(vars[0].value, "abc=def==", "only the first = separates");
    }

    #[test]
    fn rejects_unusable_names() {
        assert!(parse_env("no-equals-sign", false).is_err());
        assert!(parse_env("=value", false).is_err());
        assert!(parse_env("BAD NAME=x", false).is_err());
        assert!(parse_env("bad-name=x", false).is_err());
    }

    #[test]
    fn secret_values_are_never_displayed() {
        let secret = EnvVar { key: "K".into(), value: "hunter2".into(), secret: true };
        let plain = EnvVar { key: "K".into(), value: "public".into(), secret: false };
        assert!(!secret.display_value().contains("hunter2"));
        assert_eq!(plain.display_value(), "public");
    }

    #[tokio::test]
    async fn enrollment_codes_are_single_use() {
        let dir = scratch_dir("enroll");
        let store = Store::open(&dir).await.unwrap();

        store.remember_enrollment("code-1", "Helsinki").await.unwrap();
        let first = store.redeem_enrollment("code-1", "host-a").await.unwrap();
        assert!(first.is_some());

        let second = store.redeem_enrollment("code-1", "host-b").await.unwrap();
        assert!(second.is_none(), "a code must not be redeemable twice");

        let token = first.unwrap();
        assert_eq!(store.agent_for(&token).await.as_deref(), Some("host-a"));
        assert_eq!(store.agent_for("not-a-token").await, None);

        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    #[tokio::test]
    async fn users_round_trip_and_the_last_admin_is_protected() {
        let dir = scratch_dir("users");
        let store = Store::open(&dir).await.unwrap();

        assert!(!store.is_claimed().await);
        store.add_user("Morgan", "a fine long password", true).await.unwrap();
        assert!(store.is_claimed().await);

        // Names normalise to lowercase on the way in and on lookup.
        assert_eq!(store.verify_login("morgan", "a fine long password").await, Some(true));
        assert_eq!(store.verify_login("MORGAN", "a fine long password").await, Some(true));
        assert_eq!(store.verify_login("morgan", "wrong").await, None);

        assert!(store.add_user("morgan", "x", false).await.is_err(), "no duplicates");
        assert!(store.add_user("Bad Name!", "irrelevant password", false).await.is_err());

        store.add_user("crew", "another long password", false).await.unwrap();
        assert_eq!(store.verify_login("crew", "another long password").await, Some(false));

        store.set_user_password("crew", "rotated password now").await.unwrap();
        assert_eq!(store.verify_login("crew", "another long password").await, None);
        assert_eq!(store.verify_login("crew", "rotated password now").await, Some(false));

        assert!(
            store.remove_user("morgan").await.is_err(),
            "the last administrator must be unremovable"
        );
        store.remove_user("crew").await.unwrap();
        assert_eq!(store.users().await.len(), 1);

        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    #[tokio::test]
    async fn legacy_single_password_becomes_the_admin_account() {
        let dir = scratch_dir("migrate");
        {
            let store = Store::open(&dir).await.unwrap();
            // Simulate a pre-accounts install: hash written straight into the
            // legacy field, no users.
            let hash = hash_password("the original password").unwrap();
            let mut guard = store.inner.write().await;
            guard.users.clear();
            guard.operator_hash = Some(hash);
            let snapshot = clone_persisted(&guard);
            drop(guard);
            store.flush(&snapshot).await.unwrap();
        }

        let store = Store::open(&dir).await.unwrap();
        assert!(store.is_claimed().await);
        assert_eq!(
            store.verify_login("admin", "the original password").await,
            Some(true),
            "the old password must keep working, as admin"
        );

        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    #[tokio::test]
    async fn api_tokens_mint_resolve_and_revoke() {
        let dir = scratch_dir("apitok");
        let store = Store::open(&dir).await.unwrap();

        let token = store.mint_api_token("ci-deploys").await.unwrap();
        assert!(store.mint_api_token("ci-deploys").await.is_err(), "names are unique");
        assert_eq!(store.api_token_name(&token).await.as_deref(), Some("ci-deploys"));
        assert_eq!(store.api_token_name("forged").await, None);

        store.revoke_api_token("ci-deploys").await.unwrap();
        assert_eq!(store.api_token_name(&token).await, None, "revoked means gone");
        assert!(store.revoke_api_token("ci-deploys").await.is_err());

        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    #[tokio::test]
    async fn setting_env_updates_rather_than_duplicates() {
        let dir = scratch_dir("env");
        let store = Store::open(&dir).await.unwrap();

        store.set_env("billing", parse_env("A=1\nB=2", false).unwrap()).await.unwrap();
        store.set_env("billing", parse_env("A=changed", false).unwrap()).await.unwrap();

        let vars = store.env_for("billing").await;
        assert_eq!(vars.len(), 2, "re-setting a key must not duplicate it");
        assert_eq!(vars.iter().find(|v| v.key == "A").unwrap().value, "changed");

        store.remove_env("billing", "A").await.unwrap();
        assert_eq!(store.env_for("billing").await.len(), 1);
        assert!(store.env_for("other").await.is_empty());

        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    #[tokio::test]
    async fn state_survives_a_reopen() {
        let dir = scratch_dir("reopen");
        {
            let store = Store::open(&dir).await.unwrap();
            store.add_user("keeper", "a durable password!", true).await.unwrap();
            store.set_env("app", parse_env("K=v", true).unwrap()).await.unwrap();
        }
        let store = Store::open(&dir).await.unwrap();
        assert_eq!(store.verify_login("keeper", "a durable password!").await, Some(true));
        assert_eq!(store.env_for("app").await.len(), 1);

        let _ = tokio::fs::remove_dir_all(&dir).await;
    }
}
