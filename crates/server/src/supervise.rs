//! Owning the SpacetimeDB process.
//!
//! A customer installs DigiHost, not DigiHost-and-also-a-database. With
//! `--manage-spacetime` the server starts SpacetimeDB as a child, waits for
//! it to answer, and kills it on the way out so a restart never races a stale
//! instance still holding the port.

use std::process::Stdio;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use tokio::process::{Child, Command};

pub struct Spacetime {
    child: Child,
}

impl Spacetime {
    pub async fn start(uri: &str) -> Result<Self> {
        // Someone already owns the port — most likely a `spacetime start` the
        // operator ran by hand. Adopting it silently would mean killing a
        // process we did not start, so refuse instead.
        if reachable(uri).await {
            bail!(
                "something is already serving {uri}. Stop it, or drop --manage-spacetime \
                 and let DigiHost use it as-is."
            );
        }

        // Listen exactly where the configured URI says, not on the CLI's
        // default — a box that already runs things on 3000 (or that should
        // only expose a private interface) gets to choose.
        let listen = uri
            .trim_start_matches("http://")
            .trim_start_matches("https://")
            .trim_end_matches('/')
            .to_string();

        tracing::info!("starting SpacetimeDB on {listen}");
        let child = Command::new("spacetime")
            .args(["start", "--listen-addr", &listen])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .context(
                "starting SpacetimeDB — is the `spacetime` CLI on PATH? \
                 Install it from https://spacetimedb.com/install",
            )?;

        // Cold start builds indexes and opens the log; 30s is generous on
        // slow disks and still fails fast enough to be a useful error.
        for _ in 0..60 {
            if reachable(uri).await {
                tracing::info!("SpacetimeDB is up");
                return Ok(Self { child });
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }

        let mut failed = Self { child };
        failed.stop().await;
        bail!("SpacetimeDB did not become reachable at {uri} within 30s");
    }

    async fn stop(&mut self) {
        let _ = self.child.start_kill();
        let _ = self.child.wait().await;
    }
}

impl Drop for Spacetime {
    fn drop(&mut self) {
        // kill_on_drop delivers the signal; this line just makes the intent
        // visible in the log when the server exits.
        tracing::info!("stopping SpacetimeDB");
    }
}

async fn reachable(uri: &str) -> bool {
    let Ok(client) = reqwest::Client::builder()
        .timeout(Duration::from_secs(2))
        .build()
    else {
        return false;
    };
    // Any HTTP answer means something is listening; the status is irrelevant.
    client.get(uri).send().await.is_ok()
}
