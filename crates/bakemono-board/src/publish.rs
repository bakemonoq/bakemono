use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use anyhow::{Context, Result};
use sqlx::postgres::PgPool;

use bakemono_core::manifest::{
    shard_key, BoardKey, FileEntry, Head, Post, RevokedEntry, Root, Shard, ShardRef,
};

use crate::db;
use crate::kubo::Kubo;

// rebuild the manifest and publish a new head, unless nothing changed. only shards whose content digest
// moved since the last publish are re-added to kubo; the rest reuse their stored CID, so the work tracks
// what changed rather than the whole catalog - the add + pin round-trips are what the box can't afford
// per-creator at scale
pub async fn publish_if_changed(pool: &PgPool, kubo: &Kubo) -> Result<Option<Head>> {
    let key = board_key()?;
    let mut root = Root::default();
    let stored: HashMap<(String, String), (String, String, u64, u64)> = db::stored_shards(pool)
        .await?
        .into_iter()
        .map(|(pl, ci, digest, cid, posts, bytes)| ((pl, ci), (digest, cid, posts as u64, bytes as u64)))
        .collect();
    let mut live: HashSet<(String, String)> = HashSet::new();
    for row in db::shard_digests(pool).await? {
        let k = (row.platform.clone(), row.creator_id.clone());
        live.insert(k.clone());
        let shard_ref = match stored.get(&k) {
            Some((digest, cid, posts, bytes)) if *digest == row.digest => {
                ShardRef { cid: cid.clone(), posts: *posts, bytes: *bytes }
            }
            _ => {
                let shard = build_shard(pool, &row.platform, &row.creator_id, &row.creator).await?;
                if shard.posts.is_empty() {
                    continue;
                }
                let cid = kubo.add(shard.to_json()?, &shard.key()).await?;
                kubo.pin_archive(&cid, &format!("shard {}", shard.key())).await?;
                db::upsert_shard(
                    pool, &row.platform, &row.creator_id, &row.digest, &cid, row.posts, row.bytes,
                )
                .await?;
                ShardRef { cid, posts: row.posts as u64, bytes: row.bytes as u64 }
            }
        };
        root.shards.insert(shard_key(&row.platform, &row.creator_id), shard_ref);
    }
    // creators whose visible content is now fully gone (every file revoked) leave the manifest and its
    // shard table, so a later publish never resurrects a stale CID for them
    for k in stored.keys() {
        if !live.contains(k) {
            db::delete_shard(pool, &k.0, &k.1).await?;
        }
    }
    let mut denied_cids = Vec::new();
    for (cid, sha256, reason, revoked_at) in db::revoked_entries(pool).await? {
        denied_cids.push(cid.clone());
        root.revoked.push(RevokedEntry {
            cid: Some(cid),
            sha256,
            reason,
            revoked_at,
            ..Default::default()
        });
    }
    // publish the machine-enforceable form of the takedowns: a nopfs `.deny` blob every fleet gateway
    // fetches, so a revoked CID is blocked immediately instead of lingering until GC. its CID rides in
    // the signed root, and the co-located Kubo gets it on disk now for instant local enforcement
    if !denied_cids.is_empty() {
        let deny = nopfs_denylist(&denied_cids);
        write_local_denylist(&deny);
        let deny_cid = kubo.add(deny.into_bytes(), "denylist").await?;
        kubo.pin_archive(&deny_cid, "denylist").await?;
        root.denylist = Some(deny_cid);
    }

    let root_cid = kubo.add(root.to_json()?, "manifest root").await?;
    let last = db::last_head(pool).await?;
    if last.as_ref().is_some_and(|l| l.root_cid == root_cid) {
        return Ok(None);
    }

    let version = last.as_ref().map(|l| l.version + 1).unwrap_or(1);
    let prev = last.map(|l| l.head_cid);
    // the previous publish time bounds "new since last publish" for the IndexNow ping; grab it before the
    // fresh head lands, or 0 on the first publish so nothing is missed
    let since = db::last_head_published_epoch(pool).await.ok().flatten().unwrap_or(0);
    let published_at = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();
    let head = Head::build(version as u64, root_cid.clone(), prev, published_at, &key)
        .context("signing head")?;
    let head_json = head.to_json()?;
    let head_cid = kubo.add(head_json.clone(), "manifest head").await?;
    kubo.pin_archive(&root_cid, &format!("root v{version}")).await?;
    kubo.pin_archive(&head_cid, &format!("head v{version}")).await?;
    db::record_head(pool, version, &head_cid, &root_cid, &String::from_utf8(head_json)?).await?;
    tokio::spawn(crate::seo::ping_indexnow(pool.clone(), since));
    tracing::info!(
        version,
        head_cid,
        "manifest published; DNSLink wants TXT dnslink=/ipfs/{head_cid}"
    );
    Ok(Some(head))
}

// write the current denylist to disk for the co-located Kubo on boot, so nopfs is correct after a
// restart or a fresh denylists volume without waiting for the next publish
pub async fn sync_local_denylist(pool: &PgPool) -> Result<()> {
    let cids: Vec<String> =
        db::revoked_entries(pool).await?.into_iter().map(|(cid, ..)| cid).collect();
    if !cids.is_empty() {
        write_local_denylist(&nopfs_denylist(&cids));
    }
    Ok(())
}

// nopfs `.deny` (v1): a header, then one `/ipfs/<cid>` rule per revoked CID. a gateway with this loaded
// returns 410 for those paths regardless of whether the block is still on disk
fn nopfs_denylist(cids: &[String]) -> String {
    let mut out = String::from("version: 1\nname: bakemono\n---\n");
    for cid in cids {
        out.push_str("/ipfs/");
        out.push_str(cid);
        out.push('\n');
    }
    out
}

// drop the denylist where the co-located Kubo's nopfs reads it ($IPFS_PATH/denylists), so the board
// host enforces takedowns the instant they publish, without waiting on the fleet sync
fn write_local_denylist(content: &str) {
    let Some(dir) = std::env::var("BAKEMONO_DENYLIST_DIR").ok().filter(|s| !s.trim().is_empty()) else {
        return;
    };
    let path = std::path::Path::new(&dir).join("bakemono.deny");
    if let Err(e) = std::fs::write(&path, content) {
        tracing::warn!("writing local denylist {}: {e:#}", path.display());
    }
}

// the whole new-stack takedown: denylist (gateway stops serving), unpin (GC frees bytes,
// followers drop it on pinset sync once cluster lands), republish (peers see it in revoked)
pub async fn revoke_cid(pool: &PgPool, kubo: &Kubo, cid: &str, reason: &str) -> Result<()> {
    // the preview of revoked content goes with it
    if let Some(thumb) = db::thumb_of(pool, cid).await? {
        db::deny_cid(pool, &thumb, reason).await?;
        if let Err(e) = kubo.unpin_archive(&thumb).await {
            tracing::warn!("unpin thumb {thumb}: {e:#}");
        }
    }
    db::deny_cid(pool, cid, reason).await?;
    if let Err(e) = kubo.unpin_archive(cid).await {
        tracing::warn!("unpin {cid}: {e:#}");
    }
    publish_if_changed(pool, kubo).await?;
    Ok(())
}

// post-level takedown: every file (and preview) of the post is denied and unpinned; the post
// disappears from the next manifest because it has no visible files left
pub async fn revoke_post(
    pool: &PgPool,
    kubo: &Kubo,
    platform: &str,
    creator_id: &str,
    post_id: &str,
    reason: &str,
) -> Result<()> {
    let cids = db::cids_for_post(pool, platform, creator_id, post_id).await?;
    revoke_cids(pool, kubo, &cids, reason).await
}

pub async fn revoke_creator(
    pool: &PgPool,
    kubo: &Kubo,
    platform: &str,
    creator_id: &str,
    reason: &str,
) -> Result<()> {
    let cids = db::cids_for_creator(pool, platform, creator_id).await?;
    revoke_cids(pool, kubo, &cids, reason).await
}

async fn revoke_cids(pool: &PgPool, kubo: &Kubo, cids: &[String], reason: &str) -> Result<()> {
    for cid in cids {
        if let Some(thumb) = db::thumb_of(pool, cid).await? {
            db::deny_cid(pool, &thumb, reason).await?;
            if let Err(e) = kubo.unpin_archive(&thumb).await {
                tracing::warn!("unpin thumb {thumb}: {e:#}");
            }
        }
        db::deny_cid(pool, cid, reason).await?;
        if let Err(e) = kubo.unpin_archive(cid).await {
            tracing::warn!("unpin {cid}: {e:#}");
        }
    }
    publish_if_changed(pool, kubo).await?;
    Ok(())
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

pub fn board_pubkey() -> Result<String> {
    Ok(board_key()?.public_hex())
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
