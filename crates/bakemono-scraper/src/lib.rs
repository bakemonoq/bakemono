use std::collections::HashMap;
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::sync::{Arc, Mutex};

use thiserror::Error;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio_util::sync::CancellationToken;

#[derive(Debug, Clone)]
pub struct ScrapeRequest {
    pub creator: String,
    pub dest: PathBuf,
    pub cookies: Option<Cookies>,
    pub limit: Option<u32>,
    pub quiet: bool,
    // gallery-dl skips items recorded here without touching the files, so a re-scrape stays
    // incremental even after staged media is pruned
    pub archive: Option<PathBuf>,
    // upstream proxy for the whole run (Fanbox needs a residential one to clear Cloudflare)
    pub proxy: Option<String>,
    // raw `-o key=value` gallery-dl config overrides, e.g. a per-component proxy split
    pub options: Vec<String>,
}

#[derive(Debug, Clone)]
pub enum Cookies {
    File(PathBuf),
    Browser(String),
}

#[derive(Debug, Clone)]
pub struct ScrapeOutcome {
    pub creator: String,
    pub dest: PathBuf,
    pub files: Vec<ScrapedFile>,
}

#[derive(Debug, Clone)]
pub struct ScrapedFile {
    pub path: PathBuf,
    pub size: u64,
}

#[derive(Debug, Error)]
pub enum Error {
    #[error("could not run gallery-dl `{binary}`: {source}")]
    Spawn {
        binary: String,
        #[source]
        source: std::io::Error,
    },
    #[error("gallery-dl failed ({status}): {stderr}")]
    Failed { status: String, stderr: String },
    #[error("filesystem error at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

pub struct Scraper {
    binary: OsString,
}

impl ScrapeRequest {
    pub fn new(creator: impl Into<String>, dest: impl Into<PathBuf>) -> Self {
        Self {
            creator: creator.into(),
            dest: dest.into(),
            cookies: None,
            limit: None,
            quiet: false,
            archive: None,
            proxy: None,
            options: Vec::new(),
        }
    }
}

impl Default for Scraper {
    fn default() -> Self {
        Self {
            binary: OsString::from("gallery-dl"),
        }
    }
}

impl Scraper {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_binary(binary: impl Into<OsString>) -> Self {
        Self {
            binary: binary.into(),
        }
    }

    pub fn version(&self) -> Result<String, Error> {
        let output = self.run(&["--version".to_string()])?;
        Ok(String::from_utf8_lossy(&output).trim().to_string())
    }

    // resolve the first feed item without downloading: a cookie that authenticates yields at least
    // one media URL on stdout, a dead one prints nothing (gallery-dl exits 0 either way, so the
    // signal is stdout content, not the exit code). Ok(true) = live with reachable content
    pub async fn probe(&self, url: &str, cookies: Option<&Path>, proxy: Option<&str>) -> Result<bool, Error> {
        let mut args = vec![
            "--get-urls".to_string(),
            "--range".to_string(),
            "1-1".to_string(),
        ];
        if let Some(proxy) = proxy {
            args.push("--proxy".to_string());
            args.push(proxy.to_string());
        }
        if let Some(path) = cookies {
            args.push("--cookies".to_string());
            args.push(path.to_string_lossy().into_owned());
        }
        args.push(url.to_string());
        let mut cmd = tokio::process::Command::new(&self.binary);
        cmd.args(&args).stdout(Stdio::piped()).stderr(Stdio::null());
        #[cfg(windows)]
        cmd.creation_flags(0x0800_0000);
        let output = cmd.output().await.map_err(|source| Error::Spawn {
            binary: self.binary.to_string_lossy().into_owned(),
            source,
        })?;
        Ok(!output.stdout.iter().all(u8::is_ascii_whitespace))
    }

    pub fn scrape(&self, request: &ScrapeRequest) -> Result<ScrapeOutcome, Error> {
        std::fs::create_dir_all(&request.dest).map_err(|source| Error::Io {
            path: request.dest.clone(),
            source,
        })?;
        self.run(&build_args(request))?;
        Ok(ScrapeOutcome {
            creator: creator_name(&request.creator),
            dest: request.dest.clone(),
            files: collect_files(&request.dest)?,
        })
    }

    // streams each downloaded media path as gallery-dl prints it; killable mid-run via the token
    pub async fn scrape_streaming<F>(
        &self,
        request: &ScrapeRequest,
        cancel: CancellationToken,
        mut on_file: F,
    ) -> Result<ScrapeOutcome, Error>
    where
        F: FnMut(PathBuf),
    {
        std::fs::create_dir_all(&request.dest).map_err(|source| Error::Io {
            path: request.dest.clone(),
            source,
        })?;
        let args = build_args(request);
        tracing::info!(binary = %self.binary.to_string_lossy(), ?args, "running gallery-dl");

        let mut cmd = tokio::process::Command::new(&self.binary);
        cmd.args(&args).stdout(Stdio::piped()).stderr(Stdio::piped());
        // own process group so cancel can kill gallery-dl's PyInstaller worker, not just the bootstrap
        #[cfg(unix)]
        cmd.process_group(0);
        #[cfg(windows)]
        cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
        let mut child = cmd.spawn().map_err(|source| Error::Spawn {
            binary: self.binary.to_string_lossy().into_owned(),
            source,
        })?;

        let stdout = child.stdout.take().expect("child stdout");
        let stderr = child.stderr.take().expect("child stderr");
        let stderr_tail = Arc::new(Mutex::new(Vec::<String>::new()));
        let stderr_task = tokio::spawn(drain_stderr(stderr, stderr_tail.clone()));
        let mut lines = BufReader::new(stdout).lines();
        let mut downloaded = 0usize;
        let mut dir_cache: HashMap<PathBuf, PathBuf> = HashMap::new();

        loop {
            tokio::select! {
                _ = cancel.cancelled() => {
                    kill_group(&mut child);
                    tracing::info!("scrape cancelled, killing gallery-dl");
                    break;
                }
                next = lines.next_line() => match next {
                    Ok(Some(line)) => {
                        // gallery-dl prints downloaded paths plainly and already-present ones as "# path"
                        let line = line.trim();
                        let line = line.strip_prefix("# ").unwrap_or(line);
                        if let Some(path) = resolve_printed(Path::new(line), &request.dest, &mut dir_cache) {
                            downloaded += 1;
                            on_file(path);
                        }
                    }
                    _ => break,
                },
            }
        }

        let status = child.wait().await.map_err(|source| Error::Spawn {
            binary: self.binary.to_string_lossy().into_owned(),
            source,
        })?;
        let _ = stderr_task.await;
        // a kill we asked for is not a failure. gallery-dl also exits non-zero when single items fail
        // (a dead embedded CDN link, a gated post); for an archival run that is expected, so only abort
        // when the whole run produced nothing
        if !cancel.is_cancelled() && !status.success() {
            let tail = stderr_tail.lock().expect("stderr tail").join("\n");
            if downloaded == 0 {
                return Err(Error::Failed {
                    status: status_label(&status),
                    stderr: tail,
                });
            }
            tracing::warn!(
                status = %status_label(&status),
                kept = downloaded,
                "gallery-dl reported errors on some items, keeping what downloaded:\n{tail}"
            );
        }
        Ok(ScrapeOutcome {
            creator: creator_name(&request.creator),
            dest: request.dest.clone(),
            files: collect_files(&request.dest)?,
        })
    }

    fn run(&self, args: &[String]) -> Result<Vec<u8>, Error> {
        let output = self.output_with_retry(args)?;
        if output.status.success() {
            Ok(output.stdout)
        } else {
            Err(Error::Failed {
                status: status_label(&output.status),
                stderr: String::from_utf8_lossy(&output.stderr).trim().to_string(),
            })
        }
    }

    // a just-written binary can report ETXTBSY while another thread is mid fork+exec; retry briefly
    fn output_with_retry(&self, args: &[String]) -> Result<std::process::Output, Error> {
        let mut attempts = 0;
        loop {
            let mut cmd = Command::new(&self.binary);
            cmd.args(args);
            #[cfg(windows)]
            {
                use std::os::windows::process::CommandExt;
                cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
            }
            match cmd.output() {
                Err(e) if e.raw_os_error() == Some(26) && attempts < 50 => {
                    attempts += 1;
                    std::thread::sleep(std::time::Duration::from_millis(20));
                }
                other => {
                    return other.map_err(|source| Error::Spawn {
                        binary: self.binary.to_string_lossy().into_owned(),
                        source,
                    });
                }
            }
        }
    }
}

// gallery-dl's PyInstaller onefile bootstrap forks a worker that outlives a plain kill, so signal the
// whole process group (the child is its own group leader); start_kill still reaps the bootstrap
fn kill_group(child: &mut tokio::process::Child) {
    #[cfg(unix)]
    if let Some(pid) = child.id() {
        let _ = std::process::Command::new("kill")
            .args(["-KILL", &format!("-{pid}")])
            .status();
    }
    // on Windows start_kill reaps only the bootstrap; the worker survives holding the stdout/stderr
    // pipes, so the caller blocks forever awaiting stderr EOF. taskkill /T tears down the whole tree
    #[cfg(windows)]
    if let Some(pid) = child.id() {
        use std::os::windows::process::CommandExt;
        let _ = std::process::Command::new("taskkill")
            .args(["/F", "/T", "/PID", &pid.to_string()])
            .creation_flags(0x0800_0000) // CREATE_NO_WINDOW
            .status();
    }
    let _ = child.start_kill();
}

// gallery-dl echoes each file's path to stdout, but on Windows it encodes the path in the ANSI
// codepage, so a component the codepage cannot represent (e.g. a creator named with `☾`) prints as
// `?` and no longer matches the real file on disk. map the printed path back to the actual file by
// walking dest, matching each component exactly or, failing that, to the sole `?`-masked candidate.
// dir_cache memoizes resolved parent dirs so every file under one creator resolves without re-reading
fn resolve_printed(
    printed: &Path,
    dest: &Path,
    dir_cache: &mut HashMap<PathBuf, PathBuf>,
) -> Option<PathBuf> {
    if printed.is_file() {
        return Some(printed.to_path_buf());
    }
    let rel = printed.strip_prefix(dest).ok()?;
    let name = rel.file_name()?;
    let parent_rel = rel.parent().unwrap_or_else(|| Path::new(""));
    let real_dir = match dir_cache.get(parent_rel) {
        Some(dir) => dir.clone(),
        None => {
            let dir = resolve_dir(dest, parent_rel)?;
            dir_cache.insert(parent_rel.to_path_buf(), dir.clone());
            dir
        }
    };
    let exact = real_dir.join(name);
    if exact.is_file() {
        return Some(exact);
    }
    match_entry(&real_dir, name).filter(|p| p.is_file())
}

fn resolve_dir(dest: &Path, rel: &Path) -> Option<PathBuf> {
    let mut real = dest.to_path_buf();
    for comp in rel.components() {
        let comp = comp.as_os_str();
        let exact = real.join(comp);
        real = if exact.is_dir() {
            exact
        } else {
            match_entry(&real, comp).filter(|p| p.is_dir())?
        };
    }
    Some(real)
}

// the single entry in `dir` whose real name masks to `target` once unrepresentable chars become `?`
fn match_entry(dir: &Path, target: &OsStr) -> Option<PathBuf> {
    let target = target.to_string_lossy();
    let mut found = None;
    for entry in std::fs::read_dir(dir).ok()?.flatten() {
        if masks_to(&entry.file_name().to_string_lossy(), &target) {
            if found.is_some() {
                return None;
            }
            found = Some(entry.path());
        }
    }
    found
}

// `?` is illegal in a real Windows filename, so a `?` in the printed name always marks a replaced,
// unrepresentable char; every other position must match the real name one-for-one
fn masks_to(real: &str, masked: &str) -> bool {
    let real: Vec<char> = real.chars().collect();
    let masked: Vec<char> = masked.chars().collect();
    real.len() == masked.len()
        && real
            .iter()
            .zip(&masked)
            .all(|(r, m)| r == m || (*m == '?' && !r.is_ascii()))
}

fn build_args(request: &ScrapeRequest) -> Vec<String> {
    let mut args = vec![
        "--destination".to_string(),
        request.dest.to_string_lossy().into_owned(),
        "--write-metadata".to_string(),
        "--no-part".to_string(),
    ];
    if request.quiet {
        args.push("--quiet".to_string());
    }
    if let Some(limit) = request.limit {
        args.push("--range".to_string());
        args.push(format!("1-{limit}"));
    }
    if let Some(archive) = &request.archive {
        args.push("--download-archive".to_string());
        args.push(archive.to_string_lossy().into_owned());
    }
    if let Some(proxy) = &request.proxy {
        args.push("--proxy".to_string());
        args.push(proxy.clone());
    }
    for opt in &request.options {
        args.push("-o".to_string());
        args.push(opt.clone());
    }
    match &request.cookies {
        Some(Cookies::File(path)) => {
            args.push("--cookies".to_string());
            args.push(path.to_string_lossy().into_owned());
        }
        Some(Cookies::Browser(browser)) => {
            args.push("--cookies-from-browser".to_string());
            args.push(browser.clone());
        }
        None => {}
    }
    args.push(creator_url(&request.creator));
    args
}

fn creator_url(creator: &str) -> String {
    if creator.starts_with("http://") || creator.starts_with("https://") {
        creator.to_string()
    } else {
        format!(
            "https://www.patreon.com/{}",
            creator.trim_start_matches('/')
        )
    }
}

fn creator_name(creator: &str) -> String {
    let base = creator.split(['?', '#']).next().unwrap_or(creator);
    base.trim_end_matches('/')
        .rsplit('/')
        .next()
        .unwrap_or(base)
        .to_string()
}

fn collect_files(root: &Path) -> Result<Vec<ScrapedFile>, Error> {
    let mut files = Vec::new();
    collect_into(root, &mut files)?;
    files.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(files)
}

fn collect_into(dir: &Path, files: &mut Vec<ScrapedFile>) -> Result<(), Error> {
    let io = |source| Error::Io {
        path: dir.to_path_buf(),
        source,
    };
    for entry in std::fs::read_dir(dir).map_err(io)? {
        let entry = entry.map_err(io)?;
        let meta = entry.metadata().map_err(io)?;
        if meta.is_dir() {
            collect_into(&entry.path(), files)?;
        } else if meta.is_file() {
            files.push(ScrapedFile {
                path: entry.path(),
                size: meta.len(),
            });
        }
    }
    Ok(())
}

async fn drain_stderr(stderr: tokio::process::ChildStderr, tail: Arc<Mutex<Vec<String>>>) {
    let mut lines = BufReader::new(stderr).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        tracing::debug!(target: "gallery_dl", "{line}");
        let mut tail = tail.lock().expect("stderr tail");
        tail.push(line.to_string());
        let overflow = tail.len().saturating_sub(20);
        if overflow > 0 {
            tail.drain(0..overflow);
        }
    }
}

fn status_label(status: &ExitStatus) -> String {
    match status.code() {
        Some(code) => format!("exit code {code}"),
        None => "terminated by signal".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_basic_patreon_command() {
        let args = build_args(&ScrapeRequest::new("boxofmittens", "/tmp/out"));
        assert!(args.contains(&"--destination".to_string()));
        assert!(args.contains(&"/tmp/out".to_string()));
        assert!(args.contains(&"--write-metadata".to_string()));
        assert!(args.contains(&"--no-part".to_string()));
        assert_eq!(args.last().unwrap(), "https://www.patreon.com/boxofmittens");
    }

    #[test]
    fn passes_full_url_through_untouched() {
        let args = build_args(&ScrapeRequest::new(
            "https://www.patreon.com/someone/posts",
            "/tmp/out",
        ));
        assert_eq!(
            args.last().unwrap(),
            "https://www.patreon.com/someone/posts"
        );
    }

    #[test]
    fn cookies_and_limit_add_their_flags() {
        let mut request = ScrapeRequest::new("x", "/tmp/out");
        request.cookies = Some(Cookies::File(PathBuf::from("/c/cookies.txt")));
        request.limit = Some(5);
        let joined = build_args(&request).join(" ");
        assert!(joined.contains("--cookies /c/cookies.txt"));
        assert!(joined.contains("--range 1-5"));
    }

    #[test]
    fn creator_name_handles_vanity_and_url() {
        assert_eq!(
            creator_name("https://www.patreon.com/boxofmittens"),
            "boxofmittens"
        );
        assert_eq!(creator_name("boxofmittens"), "boxofmittens");
    }

    #[test]
    fn masks_to_matches_replaced_non_ascii() {
        // gallery-dl prints "BONI ☾" as "BONI ?" when the codepage cannot encode the moon
        assert!(masks_to("BONI ☾", "BONI ?"));
        assert!(!masks_to("BONI ☾", "BONA ?"));
        assert!(!masks_to("BONI ☾x", "BONI ?"));
        // an ascii `?` in the real name is impossible on Windows, so a `?` only masks non-ascii
        assert!(!masks_to("BONI a", "BONI ?"));
        assert!(masks_to("plain", "plain"));
    }

    #[test]
    fn resolve_printed_recovers_masked_directory() {
        let base = std::env::temp_dir().join(format!("bakemono-resolve-{}", std::process::id()));
        let real_dir = base.join("patreon").join("BONI ☾");
        std::fs::create_dir_all(&real_dir).unwrap();
        let real_file = real_dir.join("162532880_wip_01.jpg");
        std::fs::write(&real_file, b"x").unwrap();

        let printed = base.join("patreon").join("BONI ?").join("162532880_wip_01.jpg");
        let mut cache = HashMap::new();
        let resolved = resolve_printed(&printed, &base, &mut cache).unwrap();

        std::fs::remove_dir_all(&base).ok();
        assert_eq!(resolved, real_file);
    }
}
