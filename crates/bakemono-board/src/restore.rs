use anyhow::{Context, Result};
use sha2::{Digest, Sha256};
use sqlx::postgres::PgPool;

use bakemono_core::manifest::{Root, Shard};
use bakemono_core::verify_head_json;

use crate::db;
use crate::kubo::Kubo;
use crate::scrape::PostMeta;

// disaster recovery: given the latest head CID (from a keeper or DNSLink), rebuild postgres
// and re-pin the whole archive. trust anchor is the board's own restored key
pub async fn restore(pool: &PgPool, kubo: &Kubo, head_cid: &str) -> Result<()> {
    let trusted = crate::publish::board_pubkey()?;
    let raw = kubo.cat(head_cid).await.context("fetching head")?;
    let raw = String::from_utf8(raw).context("head is not UTF-8")?;
    let head = verify_head_json(&raw, &trusted, None).context("verifying head")?;
    tracing::info!(version = head.version, root = head.root, "head verified, restoring");

    let root: Root = serde_json::from_slice(&kubo.cat(&head.root).await.context("fetching root")?)
        .context("parsing root")?;

    for (key, shard_ref) in &root.shards {
        let shard: Shard = serde_json::from_slice(
            &kubo.cat(&shard_ref.cid).await.with_context(|| format!("fetching shard {key}"))?,
        )
        .with_context(|| format!("parsing shard {key}"))?;
        restore_shard(pool, kubo, &shard).await?;
        kubo.pin(&shard_ref.cid).await?;
        kubo.pin_archive(&shard_ref.cid, &format!("shard {key}")).await?;
        tracing::info!(shard = key, posts = shard.posts.len(), "shard restored");
    }

    for entry in &root.revoked {
        if let Some(cid) = &entry.cid {
            db::deny_restored(pool, cid, &entry.reason, entry.sha256.as_deref(), &entry.revoked_at)
                .await?;
        }
    }

    kubo.pin(&head.root).await?;
    kubo.pin(head_cid).await?;
    kubo.pin_archive(&head.root, &format!("root v{}", head.version)).await?;
    kubo.pin_archive(head_cid, &format!("head v{}", head.version)).await?;
    db::record_head(pool, head.version as i64, head_cid, &head.root, &raw).await?;
    tracing::info!(version = head.version, "restore complete");
    Ok(())
}

async fn restore_shard(pool: &PgPool, kubo: &Kubo, shard: &Shard) -> Result<()> {
    db::upsert_creator(pool, &shard.platform, &shard.creator_id, &shard.creator).await?;
    for post in &shard.posts {
        for (index, file) in post.files.iter().enumerate() {
            let meta = PostMeta {
                platform: shard.platform.clone(),
                creator: shard.creator.clone(),
                creator_id: shard.creator_id.clone(),
                post_id: post.post_id.clone(),
                file_index: index as i32,
                title: post.title.clone(),
                body: post.body.clone(),
                posted_at: post.posted_at.clone(),
                tier: post.tier.clone().unwrap_or_else(|| "unknown".into()),
                creator_url: None,
            };
            db::insert_file(
                pool,
                &file.cid,
                &file.sha256,
                file.size as i64,
                &file.mime,
                file.filename.as_deref(),
                file.thumb.as_deref(),
            )
            .await?;
            db::upsert_post(pool, &meta).await?;
            db::upsert_post_file(pool, &meta, &file.cid).await?;
            kubo.pin(&file.cid).await?;
            kubo.pin_archive(&file.cid, &meta.post_key()).await?;
            // the manifest carries no byte facts about thumbs, so take them from the bytes
            if let Some(thumb) = &file.thumb {
                let bytes = kubo.cat(thumb).await.with_context(|| format!("fetching thumb {thumb}"))?;
                let sha = hex::encode(Sha256::digest(&bytes));
                db::insert_file(pool, thumb, &sha, bytes.len() as i64, "image/jpeg", None, None).await?;
                kubo.pin(thumb).await?;
                kubo.pin_archive(thumb, &format!("thumb {}", meta.post_key())).await?;
            }
        }
    }
    Ok(())
}
