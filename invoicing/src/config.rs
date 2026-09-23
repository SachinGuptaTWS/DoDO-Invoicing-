use std::{env, net::SocketAddr, time::Duration};

use anyhow::{bail, Context};

/// Runtime configuration. Deliberately not `Debug`: it holds the admin token.
pub struct Config {
    pub database_url: String,
    pub bind_addr: SocketAddr,
    pub admin_token: String,
    pub psp_base_url: String,
    /// Upper bound on how long `POST /pay` waits for the PSP before answering
    /// 202 and handing the attempt to the reconciler.
    pub psp_timeout: Duration,
    /// How long the PSP may have "no record" of an attempt before we conclude
    /// our charge request never reached it. Must exceed `psp_timeout`, or we
    /// could fail an attempt whose request is still in flight.
    pub psp_not_found_grace: Duration,
    pub webhook_timeout: Duration,
    pub worker_poll_interval: Duration,
}

impl Config {
    pub fn from_env() -> anyhow::Result<Self> {
        let config = Self {
            database_url: required("DATABASE_URL")?,
            bind_addr: optional("BIND_ADDR", "0.0.0.0:8080")
                .parse()
                .context("BIND_ADDR must be host:port")?,
            admin_token: required("ADMIN_TOKEN")?,
            psp_base_url: required("PSP_BASE_URL")?,
            psp_timeout: millis("PSP_TIMEOUT_MS", 3_000)?,
            psp_not_found_grace: millis("PSP_NOT_FOUND_GRACE_MS", 30_000)?,
            webhook_timeout: millis("WEBHOOK_TIMEOUT_MS", 10_000)?,
            worker_poll_interval: millis("WORKER_POLL_INTERVAL_MS", 500)?,
        };
        config.validate()?;
        Ok(config)
    }

    pub fn validate(&self) -> anyhow::Result<()> {
        if self.admin_token.len() < 16 {
            bail!("ADMIN_TOKEN must be at least 16 characters");
        }
        if self.psp_not_found_grace <= self.psp_timeout {
            bail!("PSP_NOT_FOUND_GRACE_MS must be greater than PSP_TIMEOUT_MS");
        }
        Ok(())
    }
}

fn required(name: &str) -> anyhow::Result<String> {
    env::var(name).with_context(|| format!("{name} must be set"))
}

fn optional(name: &str, default: &str) -> String {
    env::var(name).unwrap_or_else(|_| default.to_owned())
}

fn millis(name: &str, default: u64) -> anyhow::Result<Duration> {
    match env::var(name) {
        Ok(raw) => raw
            .parse()
            .map(Duration::from_millis)
            .with_context(|| format!("{name} must be an integer number of milliseconds")),
        Err(_) => Ok(Duration::from_millis(default)),
    }
}
