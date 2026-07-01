use std::net::{ToSocketAddrs, UdpSocket};
use std::time::Duration;

const PROTOCOL_ID: u64 = 0x41727101980;
const ACTION_CONNECT: u32 = 0;
const ACTION_SCRAPE: u32 = 2;
// we only echo-check the transaction id, so a fixed one is fine ("BAKM")
const TX: u32 = 0x42414b4d;

// BEP 15 UDP scrape: connect handshake, then scrape one infohash, returning its seeder (complete) count.
// blocking on purpose - the board drives this from spawn_blocking on a slow timer, so it never touches the
// async runtime and gets simple read timeouts for free. None means the tracker did not answer (not zero)
pub fn scrape_seeders(tracker: &str, infohash: &str, timeout: Duration) -> Option<u32> {
    let addr = tracker_addr(tracker)?;
    let ih = hex20(infohash)?;
    let sock = UdpSocket::bind(("0.0.0.0", 0)).ok()?;
    sock.set_read_timeout(Some(timeout)).ok()?;
    sock.set_write_timeout(Some(timeout)).ok()?;
    sock.connect(&addr[..]).ok()?;

    sock.send(&connect_request()).ok()?;
    let mut buf = [0u8; 512];
    let n = sock.recv(&mut buf).ok()?;
    let conn = parse_connect(&buf[..n])?;

    sock.send(&scrape_request(conn, &ih)).ok()?;
    let n = sock.recv(&mut buf).ok()?;
    parse_scrape(&buf[..n])
}

fn connect_request() -> [u8; 16] {
    let mut r = [0u8; 16];
    r[0..8].copy_from_slice(&PROTOCOL_ID.to_be_bytes());
    r[8..12].copy_from_slice(&ACTION_CONNECT.to_be_bytes());
    r[12..16].copy_from_slice(&TX.to_be_bytes());
    r
}

fn parse_connect(resp: &[u8]) -> Option<u64> {
    if resp.len() < 16 || be32(resp, 0)? != ACTION_CONNECT || be32(resp, 4)? != TX {
        return None;
    }
    Some(u64::from_be_bytes(resp[8..16].try_into().ok()?))
}

fn scrape_request(connection_id: u64, infohash: &[u8; 20]) -> [u8; 36] {
    let mut r = [0u8; 36];
    r[0..8].copy_from_slice(&connection_id.to_be_bytes());
    r[8..12].copy_from_slice(&ACTION_SCRAPE.to_be_bytes());
    r[12..16].copy_from_slice(&TX.to_be_bytes());
    r[16..36].copy_from_slice(infohash);
    r
}

// scrape response: action, transaction_id, then per hash (seeders, completed, leechers). we asked for one
fn parse_scrape(resp: &[u8]) -> Option<u32> {
    if resp.len() < 20 || be32(resp, 0)? != ACTION_SCRAPE || be32(resp, 4)? != TX {
        return None;
    }
    be32(resp, 8)
}

fn be32(buf: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_be_bytes(buf.get(at..at + 4)?.try_into().ok()?))
}

fn tracker_addr(tracker: &str) -> Option<Vec<std::net::SocketAddr>> {
    let host_port = tracker.strip_prefix("udp://")?.split('/').next()?;
    let addrs: Vec<_> = host_port.to_socket_addrs().ok()?.collect();
    (!addrs.is_empty()).then_some(addrs)
}

fn hex20(s: &str) -> Option<[u8; 20]> {
    if s.len() != 40 {
        return None;
    }
    let b = s.as_bytes();
    let mut out = [0u8; 20];
    for (i, slot) in out.iter_mut().enumerate() {
        *slot = (hexval(b[2 * i])? << 4) | hexval(b[2 * i + 1])?;
    }
    Some(out)
}

fn hexval(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_connect_handshake() {
        let mut resp = Vec::new();
        resp.extend_from_slice(&ACTION_CONNECT.to_be_bytes());
        resp.extend_from_slice(&TX.to_be_bytes());
        resp.extend_from_slice(&0x0102030405060708u64.to_be_bytes());
        assert_eq!(parse_connect(&resp), Some(0x0102030405060708));
        // wrong transaction id is rejected
        resp[4] ^= 0xff;
        assert_eq!(parse_connect(&resp), None);
    }

    #[test]
    fn reads_seeder_count_from_scrape() {
        let mut resp = Vec::new();
        resp.extend_from_slice(&ACTION_SCRAPE.to_be_bytes());
        resp.extend_from_slice(&TX.to_be_bytes());
        resp.extend_from_slice(&7u32.to_be_bytes()); // seeders
        resp.extend_from_slice(&3u32.to_be_bytes()); // completed
        resp.extend_from_slice(&2u32.to_be_bytes()); // leechers
        assert_eq!(parse_scrape(&resp), Some(7));
        // an error/short packet yields nothing rather than a bogus count
        assert_eq!(parse_scrape(&resp[..8]), None);
    }

    #[test]
    fn decodes_infohash_hex() {
        assert_eq!(hex20("00ff10").is_none(), true);
        let h = hex20("0123456789abcdef0123456789abcdef01234567").unwrap();
        assert_eq!(h[0], 0x01);
        assert_eq!(h[19], 0x67);
    }
}
