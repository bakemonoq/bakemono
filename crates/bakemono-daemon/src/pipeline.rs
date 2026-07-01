use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use nostr_sdk::prelude::*;
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

use bakemono_core::Manifest;
use bakemono_scraper::{ScrapeRequest, Scraper};

use bakemono_engine::identity::Identity;
use crate::scrape::{gather_pairs, manifest_from_files};
use crate::thumbnail;
use bakemono_engine::seeder::SeederHandle;

pub type ProgressFn<'a> = &'a (dyn Fn(Progress) + Send + Sync);

pub struct JobContext<'a> {
    pub relays: &'a [String],
    pub identity: &'a Identity,
    pub seeder: Option<&'a SeederHandle>,
    pub cancel: &'a CancellationToken,
    pub progress: ProgressFn<'a>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "stage", rename_all = "snake_case")]
pub enum Progress {
    Scraping { creator: String, dest: String },
    ScrapePost { posts: usize, file: String },
    Scraped { files: usize, posts: usize },
    Pairs { count: usize },
    SeederReady,
    Manifest {
        index: usize,
        total: usize,
        file: String,
        hash: String,
        size: u64,
    },
    Seeded {
        file: String,
        magnet: String,
    },
    Thumbnailed {
        file: String,
        magnet: String,
    },
    Skipped {
        file: String,
        reason: String,
    },
    Publishing {
        relays: Vec<String>,
        count: usize,
    },
    Published {
        event_ids: Vec<String>,
    },
    Cancelled,
    Done {
        manifests: usize,
    },
    Failed {
        error: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunSummary {
    pub manifests: Vec<ManifestSummary>,
    pub event_ids: Vec<String>,
    pub relays: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ManifestSummary {
    pub d_tag: String,
    pub file_hash: String,
    pub magnet: String,
    pub size: u64,
    pub mime: String,
    pub creator: String,
    pub filename: Option<String>,
}

pub async fn run_scrape(
    request: ScrapeRequest,
    max_posts: Option<usize>,
    ctx: &JobContext<'_>,
) -> Result<RunSummary> {
    (ctx.progress)(Progress::Scraping {
        creator: request.creator.clone(),
        dest: request.dest.display().to_string(),
    });
    tracing::info!(creator = %request.creator, dest = %request.dest.display(), ?max_posts, "scrape start");

    let scraper = scraper_from_env();
    // a post-limit hit cancels only the download; a user Stop cancels the parent and everything
    let scrape_stop = ctx.cancel.child_token();
    let mut media: Vec<PathBuf> = Vec::new();
    let mut posts: Vec<String> = Vec::new();

    {
        let progress = ctx.progress;
        let media = &mut media;
        let posts = &mut posts;
        let on_file = |path: PathBuf| {
            let post = post_id_of(&path);
            if posts.iter().any(|p| *p == post) {
                media.push(path);
                return;
            }
            if let Some(max) = max_posts {
                if posts.len() >= max {
                    scrape_stop.cancel();
                    return;
                }
            }
            posts.push(post);
            progress(Progress::ScrapePost {
                posts: posts.len(),
                file: file_label(&path),
            });
            media.push(path);
        };
        scraper
            .scrape_streaming(&request, scrape_stop.clone(), on_file)
            .await
            .context("gallery-dl scrape")?;
    }

    (ctx.progress)(Progress::Scraped {
        files: media.len(),
        posts: posts.len(),
    });
    tracing::info!(files = media.len(), posts = posts.len(), "scrape done");

    if media.is_empty() {
        bail!(
            "gallery-dl downloaded nothing for '{}'. check the creator name and that the cookies file is a valid Patreon login (paid posts need cookies)",
            request.creator
        );
    }

    build_seed_publish(pairs_from_media(&media), ctx).await
}

pub async fn run_ingest(dir: &Path, ctx: &JobContext<'_>) -> Result<RunSummary> {
    tracing::info!(dir = %dir.display(), "ingest start");
    build_seed_publish(gather_pairs(dir)?, ctx).await
}

// re-seed everything in the scrape dir, so the swarm has the bytes again after a restart
pub async fn reseed(seeder: &SeederHandle, dir: &Path) -> usize {
    let pairs = match gather_pairs(dir) {
        Ok(pairs) => pairs,
        Err(e) => {
            tracing::warn!("reseed skipped, cannot read {}: {e:#}", dir.display());
            return 0;
        }
    };
    let mut count = 0;
    for (media, _sidecar) in &pairs {
        match seeder.seed(media).await {
            Ok(_) => count += 1,
            Err(e) => tracing::warn!("reseed failed for {}: {e:#}", media.display()),
        }
        // keep the preview's magnet alive across restarts by re-seeding it alongside its file
        let thumb = thumbnail::thumb_path(media);
        if thumb.is_file() && seeder.seed(&thumb).await.is_ok() {
            count += 1;
        }
    }
    tracing::info!(count, dir = %dir.display(), "reseeded from disk");
    count
}

async fn build_seed_publish(
    pairs: Vec<(PathBuf, PathBuf)>,
    ctx: &JobContext<'_>,
) -> Result<RunSummary> {
    let progress = ctx.progress;
    progress(Progress::Pairs { count: pairs.len() });
    if pairs.is_empty() {
        bail!("no media+sidecar pairs found");
    }
    if ctx.seeder.is_some() {
        progress(Progress::SeederReady);
    }

    let total = pairs.len();
    let mut manifests = Vec::new();
    let mut summaries = Vec::new();
    let mut cancelled = false;
    for (index, (media, sidecar)) in pairs.iter().enumerate() {
        if ctx.cancel.is_cancelled() {
            cancelled = true;
            tracing::info!("job cancelled before file {}", index + 1);
            break;
        }
        let mut manifest = match manifest_from_files(media, sidecar) {
            Ok(manifest) => manifest,
            Err(e) => {
                progress(Progress::Skipped {
                    file: file_label(media),
                    reason: format!("{e:#}"),
                });
                continue;
            }
        };
        progress(Progress::Manifest {
            index: index + 1,
            total,
            file: file_label(media),
            hash: manifest.file_hash.clone(),
            size: manifest.size,
        });
        if let Some(seeder) = ctx.seeder {
            manifest.magnet = seeder.seed(media).await.context("seeding file")?.magnet;
            progress(Progress::Seeded {
                file: file_label(media),
                magnet: manifest.magnet.clone(),
            });
            match seed_thumbnail(media, &manifest.mime, seeder).await {
                Ok((hash, magnet)) => {
                    progress(Progress::Thumbnailed {
                        file: file_label(media),
                        magnet: magnet.clone(),
                    });
                    manifest.thumb_x = Some(hash);
                    manifest.thumb_magnet = Some(magnet);
                }
                Err(e) => {
                    tracing::warn!("thumbnail skipped for {}: {e:#}", media.display());
                    progress(Progress::Skipped {
                        file: format!("{} (thumbnail)", file_label(media)),
                        reason: format!("{e:#}"),
                    });
                }
            }
        }
        summaries.push(summary_of(&manifest));
        manifests.push(manifest);
    }

    if manifests.is_empty() {
        if cancelled {
            progress(Progress::Cancelled);
            return Ok(RunSummary {
                manifests: summaries,
                event_ids: Vec::new(),
                relays: ctx.relays.to_vec(),
            });
        }
        bail!("no manifests built");
    }

    progress(Progress::Publishing {
        relays: ctx.relays.to_vec(),
        count: manifests.len(),
    });
    let ids = publish(ctx.relays, ctx.identity.keys(), &manifests).await?;
    let event_ids: Vec<String> = ids.iter().map(|id| id.to_hex()).collect();
    progress(Progress::Published {
        event_ids: event_ids.clone(),
    });
    tracing::info!(count = event_ids.len(), "published");

    if cancelled {
        progress(Progress::Cancelled);
    } else {
        progress(Progress::Done {
            manifests: manifests.len(),
        });
    }

    Ok(RunSummary {
        manifests: summaries,
        event_ids,
        relays: ctx.relays.to_vec(),
    })
}

// make a downscaled frame, seed it, return (sha256, magnet) for the manifest. the caller treats an
// error as a skipped preview (the file still ships), but surfaces the reason so a missing ffmpeg shows
async fn seed_thumbnail(
    media: &Path,
    mime: &str,
    seeder: &SeederHandle,
) -> Result<(String, String)> {
    let thumb = thumbnail::generate(media, mime)
        .await
        .context("generating thumbnail")?;
    let bytes = std::fs::read(&thumb).with_context(|| format!("reading {}", thumb.display()))?;
    let hash = hash_bytes(&bytes);
    let info = seeder.seed(&thumb).await.context("seeding thumbnail")?;
    Ok((hash, info.magnet))
}

fn hash_bytes(bytes: &[u8]) -> String {
    use sha2::Digest;
    hex::encode(sha2::Sha256::digest(bytes))
}

async fn publish(relays: &[String], keys: &Keys, manifests: &[Manifest]) -> Result<Vec<EventId>> {
    let client = Client::new(keys.clone());
    for relay in relays {
        client.add_relay(relay).await?;
    }
    client.connect().await;
    let mut ids = Vec::with_capacity(manifests.len());
    for manifest in manifests {
        let event = manifest.to_event(keys)?;
        client.send_event(&event).await?;
        ids.push(event.id);
    }
    client.disconnect().await;
    Ok(ids)
}

fn scraper_from_env() -> Scraper {
    match std::env::var_os("BAKEMONO_GALLERY_DL") {
        Some(path) => Scraper::with_binary(path),
        None => Scraper::new(),
    }
}

fn pairs_from_media(media: &[PathBuf]) -> Vec<(PathBuf, PathBuf)> {
    let mut pairs = Vec::new();
    for path in media {
        let mut sidecar = path.clone().into_os_string();
        sidecar.push(".json");
        let sidecar = PathBuf::from(sidecar);
        if sidecar.is_file() {
            pairs.push((path.clone(), sidecar));
        }
    }
    pairs.sort();
    pairs.dedup();
    pairs
}

// gallery-dl names Patreon files `<postid>_<title>_<n>.ext`; the leading digits are the post id
fn post_id_of(path: &Path) -> String {
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or_default();
    let digits: String = name.chars().take_while(|c| c.is_ascii_digit()).collect();
    if digits.is_empty() {
        name.to_string()
    } else {
        digits
    }
}

fn summary_of(manifest: &Manifest) -> ManifestSummary {
    ManifestSummary {
        d_tag: manifest.d_tag(),
        file_hash: manifest.file_hash.clone(),
        magnet: manifest.magnet.clone(),
        size: manifest.size,
        mime: manifest.mime.clone(),
        creator: manifest.creator.clone(),
        filename: manifest.filename.clone(),
    }
}

fn file_label(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string())
}
