use std::ffi::OsString;
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

        let mut child = tokio::process::Command::new(&self.binary)
            .args(&args)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|source| Error::Spawn {
                binary: self.binary.to_string_lossy().into_owned(),
                source,
            })?;

        let stdout = child.stdout.take().expect("child stdout");
        let stderr = child.stderr.take().expect("child stderr");
        let stderr_tail = Arc::new(Mutex::new(Vec::<String>::new()));
        let stderr_task = tokio::spawn(drain_stderr(stderr, stderr_tail.clone()));
        let mut lines = BufReader::new(stdout).lines();

        loop {
            tokio::select! {
                _ = cancel.cancelled() => {
                    let _ = child.start_kill();
                    tracing::info!("scrape cancelled, killing gallery-dl");
                    break;
                }
                next = lines.next_line() => match next {
                    Ok(Some(line)) => {
                        // gallery-dl prints downloaded paths plainly and already-present ones as "# path"
                        let line = line.trim();
                        let line = line.strip_prefix("# ").unwrap_or(line);
                        let path = PathBuf::from(line);
                        if path.is_file() {
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
        // a kill we asked for is not a failure; anything else with a bad status is
        if !cancel.is_cancelled() && !status.success() {
            let tail = stderr_tail.lock().expect("stderr tail").join("\n");
            return Err(Error::Failed {
                status: status_label(&status),
                stderr: tail,
            });
        }
        Ok(ScrapeOutcome {
            creator: creator_name(&request.creator),
            dest: request.dest.clone(),
            files: collect_files(&request.dest)?,
        })
    }

    fn run(&self, args: &[String]) -> Result<Vec<u8>, Error> {
        let output = Command::new(&self.binary)
            .args(args)
            .output()
            .map_err(|source| Error::Spawn {
                binary: self.binary.to_string_lossy().into_owned(),
                source,
            })?;
        if output.status.success() {
            Ok(output.stdout)
        } else {
            Err(Error::Failed {
                status: status_label(&output.status),
                stderr: String::from_utf8_lossy(&output.stderr).trim().to_string(),
            })
        }
    }
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
}
