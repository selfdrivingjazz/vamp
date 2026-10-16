#!/bin/sh
# VAMP CLI installer, served by vamp-server at /install.sh.
#   curl -fsSL @BASE@/install.sh | sh
# Installs vamp + vamp-media into ${VAMP_INSTALL_DIR:-~/.local/bin} (checksum-verified),
# drops the agent skill into ~/.claude/skills/vamp/ when ~/.claude exists, then runs
# `vamp join @BASE@` (skip with VAMP_NO_JOIN=1). Safe to re-run: it upgrades in place.
set -eu

base='@BASE@'
bin_dir="${VAMP_INSTALL_DIR:-$HOME/.local/bin}"

case "$(uname -s)" in
Linux) os=unknown-linux-gnu ;;
Darwin) os=apple-darwin ;;
*) echo "vamp install: unsupported OS $(uname -s)" >&2; exit 1 ;;
esac
case "$(uname -m)" in
x86_64 | amd64) arch=x86_64 ;;
arm64 | aarch64) arch=aarch64 ;;
*) echo "vamp install: unsupported CPU $(uname -m)" >&2; exit 1 ;;
esac
target="$arch-$os"

if command -v sha256sum >/dev/null 2>&1; then
	sha() { sha256sum "$1" | cut -d' ' -f1; }
else
	sha() { shasum -a 256 "$1" | cut -d' ' -f1; }
fi

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT INT TERM

curl -fsSL "$base/dist/$target/SHA256SUMS" -o "$tmp/SHA256SUMS" || {
	echo "vamp install: no build for $target on $base (run dist/build.sh on the authority)" >&2
	exit 1
}
mkdir -p "$bin_dir"
for name in vamp vamp-media; do
	want="$(awk -v n="$name" '$2 == n { print $1 }' "$tmp/SHA256SUMS")"
	[ -n "$want" ] || { echo "vamp install: $name missing from SHA256SUMS" >&2; exit 1; }
	curl -fsSL "$base/dist/$target/$name" -o "$tmp/$name"
	got="$(sha "$tmp/$name")"
	[ "$got" = "$want" ] || { echo "vamp install: checksum mismatch for $name" >&2; exit 1; }
	chmod 755 "$tmp/$name"
	# Rename over the old binary so a running vamp keeps its inode.
	mv -f "$tmp/$name" "$bin_dir/$name"
	echo "installed $bin_dir/$name"
done

if [ -d "$HOME/.claude" ]; then
	mkdir -p "$HOME/.claude/skills/vamp"
	curl -fsSL "$base/dist/SKILL.md" -o "$tmp/SKILL.md"
	mv -f "$tmp/SKILL.md" "$HOME/.claude/skills/vamp/SKILL.md"
	echo "installed $HOME/.claude/skills/vamp/SKILL.md"
fi

case ":$PATH:" in
*":$bin_dir:"*) ;;
*) echo "note: add $bin_dir to PATH (e.g. export PATH=\"$bin_dir:\$PATH\")" ;;
esac

if [ "${VAMP_NO_JOIN:-}" = 1 ]; then
	exit 0
fi
# Re-runs keep an existing replica of this server instead of failing to re-create it.
if "$bin_dir/vamp" contexts list 2>/dev/null | grep -Fq "\"url\":\"$base\""; then
	echo "already joined $base"
	exit 0
fi
# The server admits this machine by its Tailscale identity (`vamp-server allow --user/--tag`).
"$bin_dir/vamp" join "$base" || echo "vamp install: join failed; rerun: vamp join $base" >&2
