use std::io::Read;
use std::path::PathBuf;

use anyhow::{Context, Result};
use sha1::{Digest, Sha1};

// one file inside a post bundle. `rel` is its name within the torrent and the sort key, so the same
// content set yields byte-identical torrents on every machine; keep it content-addressed by the caller
pub struct BundleFile {
    pub rel: String,
    pub path: PathBuf,
}

pub struct BundleTorrent {
    pub info_hash: String,
    pub torrent: Vec<u8>,
    // rel names in torrent order; a file's position here is the file_index the gateway serves
    pub order: Vec<String>,
}

// builds a deterministic v1 multi-file torrent: files are sorted by `rel`, pieces are hashed over the
// concatenated stream, so any two contributors with the same bytes produce the same infohash and swarm
pub fn build_bundle(name: &str, mut files: Vec<BundleFile>, piece_length: u32) -> Result<BundleTorrent> {
    files.sort_by(|a, b| a.rel.cmp(&b.rel));
    files.dedup_by(|a, b| a.rel == b.rel);

    let (pieces, file_dicts, order) = hash_files(&files, piece_length)?;
    let info = info_dict(name.as_bytes(), piece_length, &pieces, &file_dicts);
    let info_hash = hex::encode(Sha1::digest(&info));

    let mut torrent = Vec::new();
    torrent.extend_from_slice(b"d4:info");
    torrent.extend_from_slice(&info);
    torrent.push(b'e');

    Ok(BundleTorrent {
        info_hash,
        torrent,
        order,
    })
}

// stream every file in order, cutting the concatenation into piece_length chunks and sha1-ing each; the
// final piece is short. also emits each file's bencoded {length, path} entry and the ordered rel list
fn hash_files(files: &[BundleFile], piece_length: u32) -> Result<(Vec<u8>, Vec<Vec<u8>>, Vec<String>)> {
    let mut pieces = Vec::new();
    let mut file_dicts = Vec::new();
    let mut order = Vec::new();
    let mut piece = Vec::with_capacity(piece_length as usize);
    let mut buf = vec![0u8; 64 * 1024];

    for f in files {
        let mut fd = std::fs::File::open(&f.path)
            .with_context(|| format!("opening {}", f.path.display()))?;
        let mut len: u64 = 0;
        loop {
            let n = fd.read(&mut buf).with_context(|| format!("reading {}", f.path.display()))?;
            if n == 0 {
                break;
            }
            len += n as u64;
            let mut off = 0;
            while off < n {
                let take = (piece_length as usize - piece.len()).min(n - off);
                piece.extend_from_slice(&buf[off..off + take]);
                off += take;
                if piece.len() == piece_length as usize {
                    pieces.extend_from_slice(&Sha1::digest(&piece));
                    piece.clear();
                }
            }
        }
        file_dicts.push(file_dict(len, f.rel.as_bytes()));
        order.push(f.rel.clone());
    }
    if !piece.is_empty() {
        pieces.extend_from_slice(&Sha1::digest(&piece));
    }
    Ok((pieces, file_dicts, order))
}

// d 6:length i<len>e 4:path l <rel> e e  (single flat path component per file)
fn file_dict(length: u64, rel: &[u8]) -> Vec<u8> {
    let mut d = Vec::new();
    d.extend_from_slice(b"d6:lengthi");
    d.extend_from_slice(length.to_string().as_bytes());
    d.extend_from_slice(b"e4:pathl");
    bencode_bytes(&mut d, rel);
    d.extend_from_slice(b"ee");
    d
}

// info dict with keys in bencode-required byte order: files, name, piece length, pieces
fn info_dict(name: &[u8], piece_length: u32, pieces: &[u8], file_dicts: &[Vec<u8>]) -> Vec<u8> {
    let mut d = Vec::new();
    d.push(b'd');
    d.extend_from_slice(b"5:filesl");
    for fd in file_dicts {
        d.extend_from_slice(fd);
    }
    d.push(b'e');
    d.extend_from_slice(b"4:name");
    bencode_bytes(&mut d, name);
    d.extend_from_slice(b"12:piece lengthi");
    d.extend_from_slice(piece_length.to_string().as_bytes());
    d.push(b'e');
    d.extend_from_slice(b"6:pieces");
    bencode_bytes(&mut d, pieces);
    d.push(b'e');
    d
}

fn bencode_bytes(out: &mut Vec<u8>, b: &[u8]) {
    out.extend_from_slice(b.len().to_string().as_bytes());
    out.push(b':');
    out.extend_from_slice(b);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(dir: &std::path::Path, name: &str, bytes: &[u8]) -> BundleFile {
        let path = dir.join(name);
        std::fs::write(&path, bytes).unwrap();
        BundleFile {
            rel: name.to_string(),
            path,
        }
    }

    #[test]
    fn infohash_is_order_independent() {
        let dir = std::env::temp_dir().join(format!("bundle-det-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mk = || {
            vec![
                write(&dir, "aaa", &vec![1u8; 3_000_000]),
                write(&dir, "bbb", &vec![2u8; 1_500_000]),
                write(&dir, "ccc", &vec![3u8; 500_000]),
            ]
        };
        let a = build_bundle("post", mk(), 1 << 20).unwrap();
        let mut shuffled = mk();
        shuffled.reverse();
        let b = build_bundle("post", shuffled, 1 << 20).unwrap();
        assert_eq!(a.info_hash, b.info_hash, "same content must yield same infohash");
        assert_eq!(a.order, vec!["aaa", "bbb", "ccc"]);
        std::fs::remove_dir_all(&dir).ok();
    }

    // the pieces must validate against the actual file bytes, or a seeder reports 0 valid pieces and can
    // serve metadata but never the files
    #[tokio::test]
    async fn bundle_seeds_as_complete() {
        let root = std::env::temp_dir().join(format!("bundle-seed-{}", std::process::id()));
        let bdir = root.join("tb");
        std::fs::create_dir_all(&bdir).unwrap();
        let files = vec![
            write(&bdir, "aaa", &vec![1u8; 3_000_000]),
            write(&bdir, "bbb", &vec![2u8; 1_500_000]),
        ];
        let built = build_bundle("tb", files, 1 << 20).unwrap();
        let session = librqbit::Session::new(root.clone()).await.unwrap();
        let handle = session
            .add_torrent(
                librqbit::AddTorrent::from_bytes(built.torrent),
                Some(librqbit::AddTorrentOptions {
                    output_folder: Some(bdir.to_string_lossy().into_owned()),
                    overwrite: true,
                    ..Default::default()
                }),
            )
            .await
            .unwrap()
            .into_handle()
            .unwrap();
        handle.wait_until_initialized().await.unwrap();
        let stats = handle.stats();
        println!("STATS {stats:?}");
        std::fs::remove_dir_all(&root).ok();
        assert_eq!(stats.progress_bytes, stats.total_bytes, "bundle must validate complete");
    }

    // our bencode + piece hashing must match what librqbit computes from the same bytes, or the torrent
    // is malformed and no client would seed it
    #[test]
    fn librqbit_agrees_on_infohash() {
        let dir = std::env::temp_dir().join(format!("bundle-lq-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let files = vec![
            write(&dir, "0", &vec![7u8; 2_100_000]),
            write(&dir, "1", &vec![9u8; 900_000]),
        ];
        let built = build_bundle("post", files, 1 << 20).unwrap();
        let parsed =
            librqbit::torrent_from_bytes::<librqbit::ByteBufOwned>(&built.torrent).unwrap();
        assert_eq!(parsed.info_hash.as_string(), built.info_hash);
        assert_eq!(parsed.info.iter_file_lengths().unwrap().count(), 2);
        std::fs::remove_dir_all(&dir).ok();
    }
}
