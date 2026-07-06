# Security policy

## Reporting a vulnerability

Please report security issues privately, not through public GitHub issues. Email **bakemonochan@proton.me** with:

- what the issue is and where (component, file, or endpoint),
- how to reproduce it,
- what an attacker could do with it.

Expect an acknowledgement within a few days. Once a fix is out, credit is offered unless you would rather stay anonymous.

## Scope worth extra attention

The board runs untrusted-facing code, so these areas are the ones most worth probing:

- **`/contribute`** accepts a platform and a session cookie from anonymous visitors and drives server-side `gallery-dl`. Cookie handling, abuse and resource exhaustion, and process invocation all live here.
- **Media serving** at `/ipfs/{cid}` is the local Kubo gateway, fronted by a reverse proxy, not a board route. It relies on `Gateway.NoFetch=true` (serve only already-held blocks) and the nopfs denylist to refuse taken-down CIDs; a misconfigured gateway or a stale denylist is the risk here.
- **Cookie sealing** (`crypto.rs`): contributor tokens are sealed with per-cookie AES-256-GCM wrapped by RSA-4096; the private key stays offline. Anything that would expose plaintext at rest or in logs matters.
- **`/mod`** endpoints mutate the manifest and pinset and are gated by a single Basic-auth token.

## Out of scope

- Content that a third party pinned independently on the public IPFS network. The manifest records intent and takedowns apply inside the operator's trust boundary; nodes outside it are beyond anyone's control.
- Denial of service that requires operator-level access (a valid `/mod` token, shell on the box)
