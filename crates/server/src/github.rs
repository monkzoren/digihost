//! GitHub as a deployment source.
//!
//! This module is the only place in DigiHost that holds a credential capable
//! of reading source. Agents never see one: they ask the server for a
//! deployment's source and get a tarball back.
//!
//! GitHub App auth is two-legged. The App's private key signs a short JWT
//! that identifies the *App*; that JWT buys an installation access token
//! which identifies the App *on one account* and expires in an hour. Only the
//! second token can read code, so it is the one we cache and use.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, bail, Context, Result};
use jsonwebtoken::{Algorithm, EncodingKey, Header};
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

/// GitHub rejects requests without one, and a specific name makes instances
/// identifiable in a customer's audit log.
const USER_AGENT: &str = concat!("DigiHost/", env!("CARGO_PKG_VERSION"));
const API: &str = "https://api.github.com";

/// The App JWT may live at most 10 minutes; nine keeps clear of clock skew.
const APP_JWT_TTL_SECS: u64 = 540;
/// Installation tokens last an hour; refresh early so a long deployment never
/// has one expire mid-fetch.
const TOKEN_REFRESH_MARGIN_SECS: u64 = 300;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AppConfig {
    /// Numeric App ID from the App's settings page.
    pub app_id: String,
    /// PEM-encoded RSA private key generated for the App.
    pub private_key_pem: String,
    /// Shared secret for verifying webhook deliveries.
    #[serde(default)]
    pub webhook_secret: Option<String>,
}

#[derive(Serialize)]
struct AppClaims {
    iat: u64,
    exp: u64,
    iss: String,
}

#[derive(Deserialize)]
struct InstallationToken {
    token: String,
    expires_at: String,
}

#[derive(Deserialize)]
struct Installation {
    id: u64,
    account: Option<Account>,
}

#[derive(Deserialize)]
struct Account {
    login: String,
}

#[derive(Clone)]
struct CachedToken {
    token: String,
    /// Unix seconds.
    expires_at: u64,
}

pub struct Commit {
    pub sha: String,
    pub message: String,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct Repo {
    pub full_name: String,
    #[serde(default)]
    pub private: bool,
    #[serde(default)]
    pub default_branch: String,
    #[serde(default)]
    pub description: Option<String>,
}

/// Fetches source for deployments. Cheap to clone; the token cache is shared.
#[derive(Clone)]
pub struct GitHub {
    http: reqwest::Client,
    app: Option<AppConfig>,
    /// owner -> installation token, keyed by owner because that is what a
    /// repository reference gives us; the installation id is looked up once.
    tokens: Arc<Mutex<HashMap<String, CachedToken>>>,
    installations: Arc<Mutex<HashMap<String, u64>>>,
}

impl GitHub {
    pub fn new(app: Option<AppConfig>) -> Result<Self> {
        let http = reqwest::Client::builder()
            .user_agent(USER_AGENT)
            .timeout(Duration::from_secs(120))
            .build()
            .context("building HTTP client")?;
        Ok(Self {
            http,
            app,
            tokens: Arc::new(Mutex::new(HashMap::new())),
            installations: Arc::new(Mutex::new(HashMap::new())),
        })
    }

    pub fn app_configured(&self) -> bool {
        self.app.is_some()
    }

    /// Check a private key parses before it is stored. Otherwise a typo'd PEM
    /// is accepted quietly and only surfaces on the first private deployment,
    /// long after the operator has moved on.
    pub fn validate_key(pem: &str) -> Result<()> {
        EncodingKey::from_rsa_pem(pem.as_bytes()).map(|_| ()).context(
            "that does not parse as a PEM-encoded RSA private key — download the key from \
             the App's settings page and paste it whole, BEGIN and END lines included",
        )
    }

    /// Sign a JWT asserting "I am this App". Valid for minutes and useless
    /// for reading code on its own.
    fn app_jwt(&self) -> Result<String> {
        let app = self
            .app
            .as_ref()
            .ok_or_else(|| anyhow!("no GitHub App configured on this instance"))?;

        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .context("system clock is before the unix epoch")?
            .as_secs();

        let claims = AppClaims {
            // Backdated slightly: GitHub rejects a JWT whose iat is in its
            // future, and a second of clock drift is common.
            iat: now.saturating_sub(60),
            exp: now + APP_JWT_TTL_SECS,
            iss: app.app_id.clone(),
        };

        let key = EncodingKey::from_rsa_pem(app.private_key_pem.as_bytes())
            .context("reading the GitHub App private key")?;

        jsonwebtoken::encode(&Header::new(Algorithm::RS256), &claims, &key)
            .context("signing the GitHub App JWT")
    }

    async fn list_installations(&self) -> Result<Vec<Installation>> {
        let jwt = self.app_jwt()?;
        let resp = self
            .http
            .get(format!("{API}/app/installations"))
            .bearer_auth(&jwt)
            .header("Accept", "application/vnd.github+json")
            .header("X-GitHub-Api-Version", "2022-11-28")
            .send()
            .await
            .context("listing GitHub App installations")?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            bail!("GitHub rejected the App credentials ({status}): {body}");
        }
        resp.json().await.context("parsing installations")
    }

    /// Which installation covers this owner. Cached: it only changes when
    /// somebody installs or uninstalls the App.
    async fn installation_id(&self, owner: &str) -> Result<u64> {
        let key = owner.to_ascii_lowercase();
        if let Some(id) = self.installations.lock().await.get(&key) {
            return Ok(*id);
        }

        let installs = self.list_installations().await?;
        let found = installs
            .iter()
            .find(|i| {
                i.account
                    .as_ref()
                    .is_some_and(|a| a.login.eq_ignore_ascii_case(owner))
            })
            .map(|i| i.id)
            .ok_or_else(|| {
                anyhow!(
                    "the GitHub App is not installed on {owner} — install it there, \
                     then retry"
                )
            })?;

        self.installations.lock().await.insert(key, found);
        Ok(found)
    }

    /// An installation token for this owner, minted or reused from cache.
    async fn installation_token(&self, owner: &str) -> Result<String> {
        let key = owner.to_ascii_lowercase();
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);

        if let Some(cached) = self.tokens.lock().await.get(&key) {
            if cached.expires_at > now + TOKEN_REFRESH_MARGIN_SECS {
                return Ok(cached.token.clone());
            }
        }

        let id = self.installation_id(owner).await?;
        let jwt = self.app_jwt()?;
        let resp = self
            .http
            .post(format!("{API}/app/installations/{id}/access_tokens"))
            .bearer_auth(&jwt)
            .header("Accept", "application/vnd.github+json")
            .header("X-GitHub-Api-Version", "2022-11-28")
            .send()
            .await
            .context("requesting a GitHub installation token")?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            bail!("could not mint an installation token ({status}): {body}");
        }

        let minted: InstallationToken = resp.json().await.context("parsing installation token")?;
        let expires_at = parse_rfc3339_secs(&minted.expires_at).unwrap_or(now + 3600);

        self.tokens.lock().await.insert(
            key,
            CachedToken {
                token: minted.token.clone(),
                expires_at,
            },
        );
        Ok(minted.token)
    }

    async fn get(&self, url: String, private_owner: Option<&str>) -> Result<reqwest::Response> {
        let mut req = self
            .http
            .get(url)
            .header("Accept", "application/vnd.github+json")
            .header("X-GitHub-Api-Version", "2022-11-28");
        if let Some(owner) = private_owner {
            let token = self.installation_token(owner).await?;
            req = req.bearer_auth(token);
        }
        req.send().await.context("calling GitHub")
    }

    /// Repositories this instance's App can see, sorted by name. Only
    /// available with a GitHub App — there is no way to enumerate "public
    /// repositories" in general.
    pub async fn repositories(&self) -> Result<Vec<Repo>> {
        if self.app.is_none() {
            bail!("connect a GitHub App to browse repositories");
        }

        let mut repos = Vec::new();
        for install in self.list_installations().await? {
            let Some(account) = install.account.as_ref() else {
                continue;
            };
            let token = match self.installation_token(&account.login).await {
                Ok(t) => t,
                Err(e) => {
                    // One inaccessible installation should not blank the list.
                    tracing::warn!("skipping installation {}: {e:#}", account.login);
                    continue;
                }
            };

            #[derive(Deserialize)]
            struct Listing {
                repositories: Vec<Repo>,
            }

            let resp = self
                .http
                .get(format!("{API}/installation/repositories?per_page=100"))
                .bearer_auth(&token)
                .header("Accept", "application/vnd.github+json")
                .header("X-GitHub-Api-Version", "2022-11-28")
                .send()
                .await
                .context("listing repositories")?;
            if !resp.status().is_success() {
                tracing::warn!("repository listing failed for {}", account.login);
                continue;
            }
            let listing: Listing = resp.json().await.context("parsing repositories")?;
            repos.extend(listing.repositories);
        }

        repos.sort_by(|a, b| a.full_name.to_lowercase().cmp(&b.full_name.to_lowercase()));
        repos.dedup_by(|a, b| a.full_name == b.full_name);
        Ok(repos)
    }

    /// The names of the files at a repository's root, for build detection.
    pub async fn root_files(&self, repo: &str, reference: &str, private: bool) -> Result<Vec<String>> {
        let (owner, name) = split_repo(repo)?;
        let resp = self
            .get(
                format!("{API}/repos/{owner}/{name}/contents/?ref={reference}"),
                private.then_some(owner.as_str()),
            )
            .await?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            bail!("could not read {owner}/{name} at {reference} ({status}): {body}");
        }

        #[derive(Deserialize)]
        struct Entry {
            name: String,
        }
        let entries: Vec<Entry> = resp.json().await.context("parsing repository contents")?;
        Ok(entries.into_iter().map(|e| e.name).collect())
    }

    /// Resolve a branch, tag or commit to the commit it names, so a deployment
    /// records what was actually deployed rather than a moving reference.
    pub async fn resolve_ref(&self, repo: &str, reference: &str, private: bool) -> Result<Commit> {
        let (owner, name) = split_repo(repo)?;
        let resp = self
            .get(
                format!("{API}/repos/{owner}/{name}/commits/{reference}"),
                private.then_some(owner.as_str()),
            )
            .await?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            bail!("could not resolve {reference} in {owner}/{name} ({status}): {body}");
        }

        #[derive(Deserialize)]
        struct CommitResponse {
            sha: String,
            commit: CommitDetail,
        }
        #[derive(Deserialize)]
        struct CommitDetail {
            message: String,
        }

        let parsed: CommitResponse = resp.json().await.context("parsing commit")?;
        Ok(Commit {
            sha: parsed.sha,
            // Commit bodies can be long; the subject is what belongs in a list.
            message: parsed
                .commit
                .message
                .lines()
                .next()
                .unwrap_or_default()
                .to_string(),
        })
    }

    /// The repository tree at one commit, as a gzipped tarball. Returned as a
    /// stream so a large repository never sits in the server's memory on its
    /// way to an agent.
    pub async fn tarball(&self, repo: &str, commit: &str, private: bool) -> Result<reqwest::Response> {
        let (owner, name) = split_repo(repo)?;
        let resp = self
            .get(
                format!("{API}/repos/{owner}/{name}/tarball/{commit}"),
                private.then_some(owner.as_str()),
            )
            .await?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            // 404 on a private repo usually means "no access", not "no such
            // repo" — GitHub hides existence from unauthorised callers.
            if status.as_u16() == 404 && !private {
                bail!(
                    "GitHub returned 404 for {owner}/{name} at {commit}. If this repository \
                     is private, register the application as private so it goes through the \
                     GitHub App."
                );
            }
            bail!("GitHub returned {status} for {owner}/{name} at {commit}: {body}");
        }

        Ok(resp)
    }
}

pub fn split_repo(repo: &str) -> Result<(String, String)> {
    let mut parts = repo.trim().trim_end_matches(".git").split('/');
    match (parts.next(), parts.next(), parts.next()) {
        (Some(owner), Some(name), None) if !owner.is_empty() && !name.is_empty() => {
            Ok((owner.to_string(), name.to_string()))
        }
        _ => bail!("expected a GitHub repository as owner/name, got {repo}"),
    }
}

/// Minimal RFC3339 -> unix seconds for GitHub's `expires_at`, which is always
/// `YYYY-MM-DDTHH:MM:SSZ`. Failure just means refreshing the token sooner.
fn parse_rfc3339_secs(text: &str) -> Option<u64> {
    let bytes = text.as_bytes();
    if bytes.len() < 20 {
        return None;
    }
    let num = |a: usize, b: usize| text.get(a..b)?.parse::<u64>().ok();
    let (y, mo, d) = (num(0, 4)?, num(5, 7)?, num(8, 10)?);
    let (h, mi, s) = (num(11, 13)?, num(14, 16)?, num(17, 19)?);

    // Days since epoch via the civil-from-days algorithm.
    let y_adj = if mo <= 2 { y as i64 - 1 } else { y as i64 };
    let era = if y_adj >= 0 { y_adj } else { y_adj - 399 } / 400;
    let yoe = y_adj - era * 400;
    let mp = (mo as i64 + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;

    Some((days * 86_400 + (h * 3600 + mi * 60 + s) as i64) as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_owner_and_name() {
        assert!(matches!(split_repo("digiflex/digihost"), Ok((o, n)) if o == "digiflex" && n == "digihost"));
        assert!(matches!(split_repo("a/b.git"), Ok((_, n)) if n == "b"));
        assert!(split_repo("digiflex").is_err());
        assert!(split_repo("a/b/c").is_err());
        assert!(split_repo("/name").is_err());
    }

    #[test]
    fn parses_github_expiry() {
        assert_eq!(parse_rfc3339_secs("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(parse_rfc3339_secs("2000-01-01T00:00:00Z"), Some(946_684_800));
        assert_eq!(parse_rfc3339_secs("2024-02-29T12:00:00Z"), Some(1_709_208_000));
    }

    #[test]
    fn rejects_unparseable_expiry() {
        // Falling back is fine — it only means refreshing the token sooner.
        assert_eq!(parse_rfc3339_secs("soon"), None);
        assert_eq!(parse_rfc3339_secs(""), None);
    }

    #[test]
    fn key_validation_refuses_junk() {
        assert!(GitHub::validate_key("not a key").is_err());
        assert!(GitHub::validate_key("").is_err());
    }
}
