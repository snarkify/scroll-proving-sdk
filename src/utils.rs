use tracing_subscriber::filter::{EnvFilter, LevelFilter};

const SDK_VERSION: &str = "v0.0.1";

pub fn get_version(circuit_version: &str) -> String {
    "v4.4.56-bce3383-6f7b46a-e5ddf67".to_string()
}

pub fn init_tracing() {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::builder()
                .with_default_directive(LevelFilter::INFO.into())
                .from_env_lossy(),
        )
        .with_ansi(false)
        .with_level(true)
        .with_target(true)
        .try_init()
        .expect("Failed to initialize tracing subscriber");
}
