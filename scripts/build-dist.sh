#!/usr/bin/env bash
# Builds snout_net for one Postgres major, in two steps so a container build can cache the first:
#
#   bash scripts/build-dist.sh tools 17            # the pgdg server headers, libclang, libcurl's headers
#   bash scripts/build-dist.sh build 17 /out       # <out>/lib/snout_net.so, stripped
#                                                  # <out>/extension/snout_net.control and its scripts
#                                                  # <out>/debug/snout_net.so.debug, its symbols
#
# The one recipe for a build that ships: a database image runs both steps in a build stage with
# this folder as a named build context, and installs what lands in <out>. It expects Debian bookworm
# with the Rust toolchain Cargo.toml's rust-version names. At run time the library needs libcurl 4
# (7.85 or later), which the server's distribution provides.
set -euo pipefail

step="${1:?usage: scripts/build-dist.sh tools <major> | build <major> <out dir>}"
major="${2:?the Postgres major}"
src="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

case "$major" in
	17) ;;
	*) echo "snout_net is built and tested for Postgres 17, not $major" >&2; exit 2 ;;
esac

pg_config="/usr/lib/postgresql/${major}/bin/pg_config"

if [ "$step" = tools ]; then
	# A different rustc builds a different binary from the same source, and nothing downstream
	# would notice.
	want="$(tr -d '\r' <"$src/Cargo.toml" | sed -n 's/^rust-version *= *"\(.*\)"/\1/p')"
	have="$(rustc --version | awk '{print $2}')"
	if [ "$want" != "$have" ]; then
		echo "Cargo.toml pins rust $want; this image has rustc $have" >&2
		exit 1
	fi

	export DEBIAN_FRONTEND=noninteractive
	apt-get update
	apt-get install -y --no-install-recommends build-essential clang libclang-dev pkg-config ca-certificates curl gnupg libcurl4-openssl-dev
	install -d /usr/share/postgresql-common/pgdg
	curl -fsSL https://www.postgresql.org/media/keys/ACCC4CF8.asc -o /usr/share/postgresql-common/pgdg/apt.postgresql.org.asc
	echo "deb [signed-by=/usr/share/postgresql-common/pgdg/apt.postgresql.org.asc] https://apt.postgresql.org/pub/repos/apt bookworm-pgdg main" \
		>/etc/apt/sources.list.d/pgdg.list
	apt-get update
	apt-get install -y --no-install-recommends "postgresql-server-dev-${major}"
	rm -rf /var/lib/apt/lists/*
	exit 0
fi

[ "$step" = build ] || { echo "unknown step: $step" >&2; exit 2; }
out="${3:?the out dir}"

# A copy to build in, so the build context stays read-only.
#
# A plain `cargo build`, not `cargo pgrx install`: the SQL objects are the hand-written install
# script in sql/, so there is no schema to generate, and pgrx's schema generation is what keeps
# most of a pgrx library's exported symbols alive. pgrx finds the server's headers through
# PGRX_PG_CONFIG_PATH, so cargo-pgrx is not needed at all.
work="$(mktemp -d)"
cp -r "$src/Cargo.toml" "$src/Cargo.lock" "$src/src" "$work/"
export PGRX_PG_CONFIG_PATH="$pg_config"
(cd "$work" && cargo build --release --locked --lib --no-default-features --features "pg${major}")

mkdir -p "$out/lib" "$out/extension" "$out/debug"
so="${CARGO_TARGET_DIR:-$work/target}/release/libsnout_net.so"
objcopy --only-keep-debug "$so" "$out/debug/snout_net.so.debug"
strip --strip-unneeded -o "$out/lib/snout_net.so" "$so"
cp "$src/snout_net.control" "$src"/sql/snout_net--*.sql "$out/extension/"
ls -l "$out/lib" "$out/extension"
