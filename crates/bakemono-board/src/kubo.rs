use anyhow::{bail, Context, Result};
use bakemono_core::manifest::{ADD_CHUNKER, ADD_CID_VERSION, ADD_HASH, ADD_RAW_LEAVES};

pub struct Kubo {
    api: String,
    // ipfs-cluster REST API; set = the fleet pinset is authoritative, unset = single-node board
    cluster: Option<String>,
    http: reqwest::Client,
}

impl Kubo {
    pub fn from_env() -> Self {
        Self {
            api: env_or("BAKEMONO_KUBO_API", "http://127.0.0.1:5001"),
            cluster: std::env::var("BAKEMONO_CLUSTER_API").ok().filter(|s| !s.is_empty()),
            // connect timeout only: a stalled kubo must not hang a handler forever, but `add`
            // streams large bodies so no overall request timeout
            http: reqwest::Client::builder()
                .connect_timeout(std::time::Duration::from_secs(10))
                .build()
                .unwrap_or_default(),
        }
    }

    // register a CID in the cluster pinset so the fleet and followers replicate it. the board's
    // own kubo already holds it (add pins locally), so this is a no-op without a cluster
    pub async fn pin_archive(&self, cid: &str, label: &str) -> Result<()> {
        let Some(cluster) = &self.cluster else {
            return Ok(());
        };
        let resp = self
            .http
            .post(format!("{cluster}/pins/{cid}"))
            .query(&[("name", label)])
            .send()
            .await
            .with_context(|| format!("cluster api unreachable at {cluster}"))?;
        if !resp.status().is_success() {
            bail!("cluster pin {cid} failed: {} {}", resp.status(), resp.text().await.unwrap_or_default());
        }
        Ok(())
    }

    // drop from the pinset (fleet + followers unpin on sync) and from the local node
    pub async fn unpin_archive(&self, cid: &str) -> Result<()> {
        if let Some(cluster) = &self.cluster {
            let resp = self
                .http
                .delete(format!("{cluster}/pins/{cid}"))
                .send()
                .await
                .with_context(|| format!("cluster api unreachable at {cluster}"))?;
            if !resp.status().is_success() && resp.status() != reqwest::StatusCode::NOT_FOUND {
                bail!("cluster unpin {cid} failed: {}", resp.status());
            }
        }
        self.unpin(cid).await
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

    // RPC cat, not the gateway: restore must fetch from the network even where the
    // public-facing gateway runs NoFetch
    pub async fn cat(&self, cid: &str) -> Result<Vec<u8>> {
        let resp = self
            .http
            .post(format!("{}/api/v0/cat?arg={cid}", self.api))
            .send()
            .await
            .with_context(|| format!("kubo api unreachable at {}", self.api))?;
        if !resp.status().is_success() {
            bail!("kubo cat {cid} failed: {} {}", resp.status(), resp.text().await.unwrap_or_default());
        }
        Ok(resp.bytes().await?.to_vec())
    }

    // recursive pin; blocks until the node holds every block, fetching from peers as needed
    pub async fn pin(&self, cid: &str) -> Result<()> {
        let resp = self
            .http
            .post(format!("{}/api/v0/pin/add?arg={cid}", self.api))
            .send()
            .await
            .with_context(|| format!("kubo api unreachable at {}", self.api))?;
        if !resp.status().is_success() {
            bail!("kubo pin {cid} failed: {} {}", resp.status(), resp.text().await.unwrap_or_default());
        }
        Ok(())
    }

    // unpin only marks; bytes leave disk when kubo's GC runs (--enable-gc or `ipfs repo gc`)
    pub async fn unpin(&self, cid: &str) -> Result<()> {
        let resp = self
            .http
            .post(format!("{}/api/v0/pin/rm?arg={cid}", self.api))
            .send()
            .await
            .with_context(|| format!("kubo api unreachable at {}", self.api))?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            if !body.contains("not pinned") {
                bail!("kubo unpin {cid} failed: {status} {body}");
            }
        }
        Ok(())
    }

}

fn env_or(key: &str, default: &str) -> String {
    std::env::var(key)
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| default.to_string())
}
