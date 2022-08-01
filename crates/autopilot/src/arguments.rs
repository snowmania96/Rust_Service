use shared::arguments::duration_from_seconds;
use std::{net::SocketAddr, time::Duration};
use tracing::level_filters::LevelFilter;
use url::Url;

#[derive(clap::Parser)]
pub struct Arguments {
    #[clap(long, env, default_value = "warn,autopilot=debug,shared=debug")]
    pub log_filter: String,

    #[clap(long, env, default_value = "error", parse(try_from_str))]
    pub log_stderr_threshold: LevelFilter,

    #[clap(long, env, default_value = "0.0.0.0:9589")]
    pub metrics_address: SocketAddr,

    /// Url of the Postgres database. By default connects to locally running postgres.
    #[clap(long, env, default_value = "postgresql://")]
    pub db_url: Url,

    /// The Ethereum node URL to connect to.
    #[clap(long, env, default_value = "http://localhost:8545")]
    pub node_url: Url,

    /// Timeout in seconds for all http requests.
    #[clap(
        long,
        default_value = "10",
        parse(try_from_str = duration_from_seconds),
    )]
    pub http_timeout: Duration,
}

impl std::fmt::Display for Arguments {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        writeln!(f, "log_filter: {}", self.log_filter)?;
        writeln!(f, "log_stderr_threshold: {}", self.log_stderr_threshold)?;
        writeln!(f, "metrics_address: {}", self.metrics_address)?;
        writeln!(f, "db_url: SECRET")?;
        writeln!(f, "node_url: {}", self.node_url)?;
        writeln!(f, "http_timeout: {:?}", self.http_timeout)?;
        Ok(())
    }
}
