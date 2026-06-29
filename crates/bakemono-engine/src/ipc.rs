use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};

use crate::content::ContentSource;
use crate::daemon::Daemon;

pub fn socket_path() -> PathBuf {
    super::data_dir().join("daemon.sock")
}

// the wire protocol is one request per connection: client sends one JSON line
// {cmd, job?}, the daemon streams {event:"progress",data:...} lines then a terminal
// {ok:true,result:...} or {ok:false,error:...}, then closes. transport-agnostic.

async fn write_line<W: AsyncWrite + Unpin>(writer: &mut W, msg: &Value) -> std::io::Result<()> {
    let mut bytes = serde_json::to_vec(msg).unwrap_or_default();
    bytes.push(b'\n');
    writer.write_all(&bytes).await?;
    writer.flush().await
}

async fn serve_request<C, R, W>(
    reader: R,
    mut writer: W,
    daemon: Arc<Daemon<C>>,
    shutdown: tokio_util::sync::CancellationToken,
) -> Result<()>
where
    C: ContentSource,
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin + Send + 'static,
{
    let mut lines = BufReader::new(reader).lines();
    let Some(line) = lines.next_line().await? else {
        return Ok(());
    };
    let request: Value = serde_json::from_str(&line).context("parsing request")?;
    let cmd = request.get("cmd").and_then(Value::as_str).unwrap_or_default();

    match cmd {
        "status" => {
            let status = daemon.status().await;
            write_line(&mut writer, &json!({"ok": true, "result": status})).await?;
        }
        "stats" => {
            let stats = daemon.stats();
            write_line(&mut writer, &json!({"ok": true, "result": stats})).await?;
        }
        "cancel" => {
            daemon.cancel();
            write_line(&mut writer, &json!({"ok": true})).await?;
        }
        "shutdown" => {
            write_line(&mut writer, &json!({"ok": true})).await?;
            shutdown.cancel();
        }
        "run" => {
            let job = request.get("job").cloned().unwrap_or(Value::Null);
            run_streaming(job, writer, daemon).await;
        }
        other => {
            write_line(&mut writer, &json!({"ok": false, "error": format!("unknown cmd {other}")}))
                .await?;
        }
    }
    Ok(())
}

// stream progress while the job runs: the sync progress callback feeds a channel that a
// writer task drains to the connection, so events flow as they happen
async fn run_streaming<C, W>(job: Value, writer: W, daemon: Arc<Daemon<C>>)
where
    C: ContentSource,
    W: AsyncWrite + Unpin + Send + 'static,
{
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Value>();
    let drain = tokio::spawn(async move {
        let mut writer = writer;
        while let Some(msg) = rx.recv().await {
            if write_line(&mut writer, &msg).await.is_err() {
                break;
            }
        }
    });

    let events = tx.clone();
    let on_progress = move |data: Value| {
        let _ = events.send(json!({"event": "progress", "data": data}));
    };
    let terminal = match daemon.run_job(job, &on_progress).await {
        Ok(result) => json!({"ok": true, "result": result}),
        Err(e) => json!({"ok": false, "error": format!("{e:#}")}),
    };
    let _ = tx.send(terminal);
    drop(tx);
    let _ = drain.await;
}

#[cfg(unix)]
pub async fn is_running() -> bool {
    tokio::net::UnixStream::connect(socket_path()).await.is_ok()
}

#[cfg(not(unix))]
pub async fn is_running() -> bool {
    false
}

#[cfg(unix)]
pub async fn serve<C: ContentSource>(daemon: Arc<Daemon<C>>) -> Result<()> {
    use tokio_util::sync::CancellationToken;

    let path = socket_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    // single instance: if something already answers on the socket, another daemon owns it
    if tokio::net::UnixStream::connect(&path).await.is_ok() {
        bail!("a daemon is already running at {}", path.display());
    }
    let _ = std::fs::remove_file(&path); // clear a stale socket from a crashed run
    let listener = tokio::net::UnixListener::bind(&path)
        .with_context(|| format!("binding {}", path.display()))?;
    tracing::info!(socket = %path.display(), "ipc listening");

    let shutdown = CancellationToken::new();
    loop {
        tokio::select! {
            _ = shutdown.cancelled() => break,
            accepted = listener.accept() => {
                let (stream, _) = accepted?;
                let daemon = daemon.clone();
                let shutdown = shutdown.clone();
                tokio::spawn(async move {
                    let (reader, writer) = stream.into_split();
                    if let Err(e) = serve_request(reader, writer, daemon, shutdown).await {
                        tracing::debug!("ipc connection ended: {e:#}");
                    }
                });
            }
        }
    }
    let _ = std::fs::remove_file(&path);
    daemon.shutdown().await;
    Ok(())
}

#[cfg(unix)]
pub async fn call(request: Value, mut on_event: impl FnMut(Value)) -> Result<Value> {
    let path = socket_path();
    let stream = tokio::net::UnixStream::connect(&path)
        .await
        .with_context(|| format!("connecting to {}", path.display()))?;
    let (reader, mut writer) = stream.into_split();
    write_line(&mut writer, &request).await?;
    let mut lines = BufReader::new(reader).lines();
    while let Some(line) = lines.next_line().await? {
        let msg: Value = serde_json::from_str(&line)?;
        if msg.get("event").and_then(Value::as_str) == Some("progress") {
            on_event(msg.get("data").cloned().unwrap_or(Value::Null));
        } else if let Some(ok) = msg.get("ok").and_then(Value::as_bool) {
            return if ok {
                Ok(msg.get("result").cloned().unwrap_or(Value::Null))
            } else {
                bail!(
                    "{}",
                    msg.get("error").and_then(Value::as_str).unwrap_or("daemon error")
                );
            };
        }
    }
    bail!("daemon closed the connection without a response")
}

// TODO: Windows named-pipe transport; the protocol above is transport-agnostic so only these two wrappers change
#[cfg(not(unix))]
pub async fn serve<C: ContentSource>(_daemon: Arc<Daemon<C>>) -> Result<()> {
    bail!("daemon IPC is not yet implemented on this platform")
}

#[cfg(not(unix))]
pub async fn call(_request: Value, _on_event: impl FnMut(Value)) -> Result<Value> {
    bail!("daemon IPC is not yet implemented on this platform")
}
