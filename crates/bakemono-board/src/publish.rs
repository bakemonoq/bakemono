use std::path::PathBuf;

use anyhow::{Context, Result};
use sqlx::postgres::PgPool;

use bakemono_core::manifest::{BoardKey, FileEntry, Head, Post, RevokedEntry, Root, Shard, ShardRef};

use crate::db;
use crate::kubo::Kubo;

// rebuild the whole manifest and publish a new head, unless nothing changed. shard diffing is
// free: identical content serializes to identical bytes, which kubo maps to the identical CID
pub async fn publish_if_changed(pool: &PgPool, kubo: &Kubo) -> Result<Option<Head>> {
    let key = board_key()?;
    let mut root = Root::default();
    for (platform, creator_id, creator) in db::all_creators(pool).await? {
        let shard = build_shard(pool, &platform, &creator_id, &creator).await?;
        if shard.posts.is_empty() {
            continue;
        }
        let posts = shard.posts.len() as u64;
        let bytes: u64 = shard.posts.iter().flat_map(|p| &p.files).map(|f| f.size).sum();
        let cid = kubo.add(shard.to_json()?, &shard.key()).await?;
        root.shards.insert(shard.key(), ShardRef { cid, posts, bytes });
    }
    for (cid, sha256, reason, revoked_at) in db::revoked_entries(pool).await? {
        root.revoked.push(RevokedEntry {
            cid: Some(cid),
            sha256,
            reason,
            revoked_at,
            ..Default::default()
        });
    }

    let root_cid = kubo.add(root.to_json()?, "manifest root").await?;
    let last = db::last_head(pool).await?;
    if last.as_ref().is_some_and(|l| l.root_cid == root_cid) {
        return Ok(None);
    }

    let version = last.as_ref().map(|l| l.version + 1).unwrap_or(1);
    let prev = last.map(|l| l.head_cid);
    let published_at = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();
    let head = Head::build(version as u64, root_cid.clone(), prev, published_at, &key)
        .context("signing head")?;
    let head_json = head.to_json()?;
    let head_cid = kubo.add(head_json.clone(), "manifest head").await?;
    db::record_head(pool, version, &head_cid, &root_cid, &String::from_utf8(head_json)?).await?;
    tracing::info!(
        version,
        head_cid,
        "manifest published; DNSLink wants TXT dnslink=/ipfs/{head_cid}"
    );
    Ok(Some(head))
}

async fn build_shard(
    pool: &PgPool,
    platform: &str,
    creator_id: &str,
    creator: &str,
) -> Result<Shard> {
    let mut shard = Shard {
        platform: platform.to_string(),
        creator_id: creator_id.to_string(),
        creator: creator.to_string(),
        posts: Vec::new(),
    };
    for row in db::shard_rows(pool, platform, creator_id).await? {
        let entry = FileEntry {
            cid: row.cid,
            sha256: row.sha256,
            size: row.size as u64,
            mime: row.mime,
            filename: row.filename,
            thumb: row.thumb_cid,
        };
        match shard.posts.last_mut().filter(|p| p.post_id == row.post_id) {
            Some(post) => post.files.push(entry),
            None => shard.posts.push(Post {
                post_id: row.post_id,
                title: row.title,
                body: row.body,
                posted_at: row.posted_at,
                tier: row.tier,
                files: vec![entry],
            }),
        }
    }
    Ok(shard)
}

// env wins (docker); otherwise a key file, generated on first use. the pubkey is the board's
// identity, so losing this file ends the board's ability to publish - back it up offline
fn board_key() -> Result<BoardKey> {
    if let Ok(hex) = std::env::var("BAKEMONO_BOARD_KEY") {
        if !hex.trim().is_empty() {
            return BoardKey::from_hex(hex.trim()).context("parsing BAKEMONO_BOARD_KEY");
        }
    }
    let path = key_path();
    if path.is_file() {
        let hex = std::fs::read_to_string(&path)?;
        return BoardKey::from_hex(hex.trim())
            .with_context(|| format!("parsing {}", path.display()));
    }
    let key = BoardKey::generate();
    std::fs::write(&path, key.secret_hex())
        .with_context(|| format!("writing {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
    }
    tracing::warn!(
        pubkey = key.public_hex(),
        "generated a new board key at {}; BACK IT UP - it cannot be recovered",
        path.display()
    );
    Ok(key)
}

fn key_path() -> PathBuf {
    std::env::var("BAKEMONO_BOARD_KEY_FILE")
        .ok()
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("board.key"))
}
