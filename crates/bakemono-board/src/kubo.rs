use anyhow::{bail, Context, Result};
use bakemono_core::manifest::{ADD_CHUNKER, ADD_CID_VERSION, ADD_HASH, ADD_RAW_LEAVES};

pub struct Kubo {
    api: String,
    gateway: String,
    http: reqwest::Client,
}

impl Kubo {
    pub fn from_env() -> Self {
        Self {
            api: env_or("BAKEMONO_KUBO_API", "http://127.0.0.1:5001"),
            gateway: env_or("BAKEMONO_KUBO_GATEWAY", "http://127.0.0.1:8080"),
            http: reqwest::Client::new(),
        }
    }

    pub async fn add(&self, bytes: Vec<u8>, label: &str) -> Result<String> {
        self.add_part(reqwest::multipart::Part::bytes(bytes), label).await
    }

    // streams from disk so multi-GB videos never sit in memory
    pub async fn add_path(&self, path: &std::path::Path) -> Result<String> {
        let file = tokio::fs::File::open(path)
            .await
            .with_context(|| format!("opening {}", path.display()))?;
        let body = reqwest::Body::wrap_stream(tokio_util::io::ReaderStream::new(file));
        self.add_part(reqwest::multipart::Part::stream(body), &path.to_string_lossy())
            .await
    }

    // pin=true: everything added is archive content; unpinned blocks are GC fodder
    async fn add_part(&self, part: reqwest::multipart::Part, label: &str) -> Result<String> {
        let url = format!(
            "{}/api/v0/add?cid-version={ADD_CID_VERSION}&raw-leaves={ADD_RAW_LEAVES}&hash={ADD_HASH}&chunker={ADD_CHUNKER}&pin=true",
            self.api
        );
        let form = reqwest::multipart::Form::new().part("file", part);
        let resp = self
            .http
            .post(&url)
            .multipart(form)
            .send()
            .await
            .with_context(|| format!("kubo api unreachable at {}", self.api))?;
        if !resp.status().is_success() {
            bail!("kubo add failed for {label}: {} {}", resp.status(), resp.text().await.unwrap_or_default());
        }
        let body: serde_json::Value = serde_json::from_str(&resp.text().await?)
            .context("kubo add returned non-JSON")?;
        body["Hash"]
            .as_str()
            .map(str::to_owned)
            .with_context(|| format!("kubo add returned no Hash for {label}"))
    }

    pub async fn fetch(&self, cid: &str, range: Option<&str>) -> Result<reqwest::Response> {
        let mut req = self.http.get(format!("{}/ipfs/{cid}", self.gateway));
        if let Some(range) = range {
            req = req.header(reqwest::header::RANGE, range);
        }
        req.send()
            .await
            .with_context(|| format!("kubo gateway unreachable at {}", self.gateway))
    }
}

fn env_or(key: &str, default: &str) -> String {
    std::env::var(key)
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| default.to_string())
}
