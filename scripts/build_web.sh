#!/usr/bin/env sh
# Builds the web console into web/dist, which `chibidb serve` hosts from
# `server.http_addr` (see `server.web_root`).
#
# Kept out of the cargo build on purpose: making `cargo build` require Node
# would mean a machine without it could not build the database at all.
#
# Requires Node and network access for the first `npm ci`.
set -eu

root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
cd "$root/web"

if [ ! -d node_modules ]; then
    echo "installing web dependencies (needs network)..."
    npm install
fi

npm run build
echo
echo "built $root/web/dist — start the server with:"
echo "  cargo run -q -- serve <dir>    # with server.http_addr set in config.toml"
