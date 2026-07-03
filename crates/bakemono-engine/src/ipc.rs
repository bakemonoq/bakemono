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
        // status/stats are polled by the gui on a timer, so keep them off the default info log
        "status" => {
            tracing::debug!("ipc status");
            let status = daemon.status().await;
            write_line(&mut writer, &json!({"ok": true, "result": status})).await?;
        }
        "stats" => {
            tracing::debug!("ipc stats");
            let stats = daemon.stats();
            write_line(&mut writer, &json!({"ok": true, "result": stats})).await?;
        }
        "cancel" => {
            tracing::info!("ipc cancel");
            daemon.cancel();
            write_line(&mut writer, &json!({"ok": true})).await?;
        }
        "shutdown" => {
            tracing::info!("ipc shutdown");
            // cancel first: a fire-and-forget client (the gui on exit) may already be gone,
            // so writing the ok can fail - the daemon must shut down regardless
            shutdown.cancel();
            let _ = write_line(&mut writer, &json!({"ok": true})).await;
        }
        "run" => {
            let job = request.get("job").cloned().unwrap_or(Value::Null);
            let kind = job.get("kind").and_then(Value::as_str).unwrap_or("unknown");
            tracing::info!(%kind, "ipc run");
            run_streaming(job, writer, daemon).await;
        }
        other => {
            tracing::warn!(cmd = %other, "ipc unknown command");
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
                let (reader, writer) = stream.into_split();
                spawn_connection(reader, writer, daemon.clone(), shutdown.clone());
            }
        }
    }
    let _ = std::fs::remove_file(&path);
    // bound the graceful shutdown: librqbit's session teardown can hang, and the daemon force-exits
    // after this returns anyway
    let _ = tokio::time::timeout(std::time::Duration::from_secs(3), daemon.shutdown()).await;
    Ok(())
}

#[cfg(unix)]
pub async fn call(request: Value, mut on_event: impl FnMut(Value)) -> Result<Value> {
    let path = socket_path();
    let stream = tokio::net::UnixStream::connect(&path)
        .await
        .with_context(|| format!("connecting to {}", path.display()))?;
    let (reader, writer) = stream.into_split();
    converse(reader, writer, &request, &mut on_event).await
}

// named pipes share one machine-global namespace, so derive the name from the per-user
// data dir to keep separate users/installs from colliding on one box
#[cfg(windows)]
fn pipe_name() -> String {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    super::data_dir().hash(&mut hasher);
    format!(r"\\.\pipe\bakemono-daemon-{:016x}", hasher.finish())
}

#[cfg(windows)]
pub async fn is_running() -> bool {
    use tokio::net::windows::named_pipe::ClientOptions;
    match ClientOptions::new().open(pipe_name()) {
        Ok(_) => true,
        // ERROR_PIPE_BUSY: a server owns the name but every instance is mid-handshake
        Err(e) => e.raw_os_error() == Some(231),
    }
}

#[cfg(windows)]
pub async fn serve<C: ContentSource>(daemon: Arc<Daemon<C>>) -> Result<()> {
    use tokio::net::windows::named_pipe::ServerOptions;
    use tokio_util::sync::CancellationToken;

    let name = pipe_name();
    if is_running().await {
        bail!("a daemon is already running at {name}");
    }
    // first_pipe_instance rejects a second server binding the same name
    let mut server = ServerOptions::new()
        .first_pipe_instance(true)
        .create(&name)
        .with_context(|| format!("creating pipe {name}"))?;
    tracing::info!(pipe = %name, "ipc listening");

    let shutdown = CancellationToken::new();
    loop {
        tokio::select! {
            _ = shutdown.cancelled() => break,
            res = server.connect() => res?,
        }
        // stand up the next listener before handing off the connected one; the connect future
        // that borrowed `server` is dropped when the select! above ends, so the move is clean
        let connected = server;
        server = ServerOptions::new().create(&name)?;
        let (reader, writer) = tokio::io::split(connected);
        spawn_connection(reader, writer, daemon.clone(), shutdown.clone());
    }
    let _ = tokio::time::timeout(std::time::Duration::from_secs(3), daemon.shutdown()).await;
    Ok(())
}

#[cfg(windows)]
pub async fn call(request: Value, mut on_event: impl FnMut(Value)) -> Result<Value> {
    use tokio::net::windows::named_pipe::ClientOptions;
    let name = pipe_name();
    let mut waited = 0;
    let client = loop {
        match ClientOptions::new().open(&name) {
            Ok(client) => break client,
            // ERROR_PIPE_BUSY: server is between instances, retry briefly
            Err(e) if e.raw_os_error() == Some(231) && waited < 50 => {
                waited += 1;
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
            Err(e) => {
                return Err(anyhow::Error::new(e))
                    .with_context(|| format!("connecting to {name}"));
            }
        }
    };
    let (reader, writer) = tokio::io::split(client);
    converse(reader, writer, &request, &mut on_event).await
}

#[cfg(not(any(unix, windows)))]
pub async fn is_running() -> bool {
    false
}

#[cfg(not(any(unix, windows)))]
pub async fn serve<C: ContentSource>(_daemon: Arc<Daemon<C>>) -> Result<()> {
    bail!("daemon IPC is not implemented on this platform")
}

#[cfg(not(any(unix, windows)))]
pub async fn call(_request: Value, _on_event: impl FnMut(Value)) -> Result<Value> {
    bail!("daemon IPC is not implemented on this platform")
}

#[cfg(any(unix, windows))]
fn spawn_connection<C, R, W>(
    reader: R,
    writer: W,
    daemon: Arc<Daemon<C>>,
    shutdown: tokio_util::sync::CancellationToken,
) where
    C: ContentSource,
    R: AsyncRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin + Send + 'static,
{
    tokio::spawn(async move {
        if let Err(e) = serve_request(reader, writer, daemon, shutdown).await {
            tracing::debug!("ipc connection ended: {e:#}");
        }
    });
}

// client side of the wire protocol, transport-agnostic: send the request, relay progress
// events to the caller, return the terminal ok/error
#[cfg(any(unix, windows))]
async fn converse<R, W>(
    reader: R,
    mut writer: W,
    request: &Value,
    on_event: &mut impl FnMut(Value),
) -> Result<Value>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    write_line(&mut writer, request).await?;
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
