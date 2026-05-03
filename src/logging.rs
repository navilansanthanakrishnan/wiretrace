use anyhow::Result;
use tracing_subscriber::EnvFilter;

pub fn init(log_filter: &str) -> Result<()> {
    let env_filter = EnvFilter::try_new(log_filter)
        .or_else(|_| EnvFilter::try_new("info"))
        .expect("static env filter is valid");

    tracing_subscriber::fmt()
        .with_env_filter(env_filter)
        .with_target(false)
        .compact()
        .init();

    Ok(())
}
