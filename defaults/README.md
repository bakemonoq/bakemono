# Defaults

Operational defaults shipped with the board.

- `kubo-init.sh` - config applied to the ipfs/kubo container on every start (bitswap server on, broadcast reduction off, roots-only reproviding). Mounted at `/container-init.d/` by both compose files; a bare-metal kubo wants the same three `ipfs config` calls, see `docs/KEEPERS.md`
