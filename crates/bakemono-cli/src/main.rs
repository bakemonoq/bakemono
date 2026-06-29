use anyhow::{bail, Context, Result};
use serde_json::{json, Value};

use bakemono_engine::ipc;

const USAGE: &str = "usage:
  bakemono scrape <creator> [--limit N] [--cookies FILE] [--browser NAME]
  bakemono ingest <dir>
  bakemono status
  bakemono stats
  bakemono cancel
  bakemono stop";

#[tokio::main]
async fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let cmd = args.first().map(String::as_str).unwrap_or("");
    if matches!(cmd, "" | "-h" | "--help") {
        eprintln!("{USAGE}");
        return Ok(());
    }
    if !ipc::is_running().await {
        bail!("the bakemono daemon is not running\nstart it with `bakemono-daemon` (or your service manager), then retry");
    }
    if let Some(latest) = bakemono_engine::version::cached_newer(env!("CARGO_PKG_VERSION")) {
        eprintln!("a newer bakemono release is available ({latest}): {}", bakemono_engine::version::RELEASES_URL);
    }
    match cmd {
        "scrape" => scrape(&args[1..]).await,
        "ingest" => {
            let dir = args.get(1).context("ingest needs a directory")?;
            run(json!({"cmd": "run", "job": {"kind": "ingest", "dir": dir}})).await
        }
        "status" => show(json!({"cmd": "status"})).await,
        "stats" => show(json!({"cmd": "stats"})).await,
        "cancel" => show(json!({"cmd": "cancel"})).await,
        "stop" => show(json!({"cmd": "shutdown"})).await,
        other => bail!("unknown command `{other}`\n{USAGE}"),
    }
}

async fn scrape(args: &[String]) -> Result<()> {
    let mut creator = None;
    let mut limit: Option<u32> = None;
    let mut cookies: Option<String> = None;
    let mut browser: Option<String> = None;
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--limit" => limit = Some(it.next().context("--limit expects a number")?.parse()?),
            "--cookies" => cookies = Some(it.next().context("--cookies expects a path")?.clone()),
            "--browser" => browser = Some(it.next().context("--browser expects a name")?.clone()),
            flag if flag.starts_with('-') => bail!("unknown flag {flag}"),
            positional if creator.is_none() => creator = Some(positional.to_string()),
            other => bail!("unexpected argument {other}"),
        }
    }
    let creator = creator.context("scrape needs a creator")?;
    run(json!({
        "cmd": "run",
        "job": {"kind": "scrape", "creator": creator, "limit": limit, "cookies": cookies, "browser": browser}
    }))
    .await
}

// run a job, streaming its progress to stdout
async fn run(request: Value) -> Result<()> {
    let result = ipc::call(request, |event| println!("  {}", render(&event))).await?;
    if let Some(ids) = result.get("event_ids").and_then(Value::as_array) {
        println!("done: {} event(s) published", ids.len());
    }
    Ok(())
}

// one-shot command, pretty-print whatever the daemon returns
async fn show(request: Value) -> Result<()> {
    let result = ipc::call(request, |_| {}).await?;
    if !result.is_null() {
        println!("{}", serde_json::to_string_pretty(&result)?);
    }
    Ok(())
}

fn render(event: &Value) -> String {
    let stage = event.get("stage").and_then(Value::as_str).unwrap_or("?");
    let s = |key: &str| match event.get(key) {
        Some(Value::String(v)) => v.clone(),
        Some(other) => other.to_string(),
        None => String::new(),
    };
    match stage {
        "scraping" => format!("scraping {}", s("creator")),
        "scrape_post" => format!("post #{} {}", s("posts"), s("file")),
        "scraped" => format!("scraped {} file(s) across {} post(s)", s("files"), s("posts")),
        "manifest" => format!("[{}/{}] {}", s("index"), s("total"), s("file")),
        "seeded" => format!("seeded {}", s("file")),
        "skipped" => format!("skip {}: {}", s("file"), s("reason")),
        "publishing" => format!("publishing {} event(s)", s("count")),
        "published" => "published".to_string(),
        "cancelled" => "cancelled".to_string(),
        "done" => format!("done, {} manifest(s)", s("manifests")),
        "failed" => format!("failed: {}", s("error")),
        other => other.to_string(),
    }
}
