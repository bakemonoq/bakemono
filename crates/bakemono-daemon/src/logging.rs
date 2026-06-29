use tracing_subscriber::prelude::*;

// stdout plus a rolling daily file under the data dir; returns a guard that must stay alive
pub fn init(name: &str) -> tracing_appender::non_blocking::WorkerGuard {
    let dir = super::data_dir().join("logs");
    std::fs::create_dir_all(&dir).ok();
    let (file_writer, guard) =
        tracing_appender::non_blocking(tracing_appender::rolling::daily(&dir, format!("{name}.log")));
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    tracing_subscriber::registry()
        .with(filter)
        .with(tracing_subscriber::fmt::layer().with_writer(std::io::stdout))
        .with(
            tracing_subscriber::fmt::layer()
                .with_ansi(false)
                .with_writer(file_writer),
        )
        .init();
    guard
}
