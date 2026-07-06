# Defaults

Operational defaults shipped with the board.

- `kubo-init.sh` - config applied to the ipfs/kubo container on every start: bitswap server on, broadcast reduction off, roots-only reproviding (via `Provide.Strategy` on kubo >= 0.38, falling back to `Reprovider.Strategy`), and `Gateway.NoFetch` on the board host. Mounted at `/container-init.d/` by both compose files; a bare-metal keeper applies the same calls by hand, see `docs/KEEPERS.md`
