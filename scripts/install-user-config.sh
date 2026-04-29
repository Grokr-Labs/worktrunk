#!/usr/bin/env bash
# Idempotently symlink ~/.config/worktrunk/{config,approvals}.toml into the
# canonical fleet-user-config inside this worktrunk checkout, so every fleet
# host shares one source of truth.
#
# Usage: scripts/install-user-config.sh
#
# Re-runs are safe: existing correct symlinks are left alone; existing files or
# wrong symlinks are replaced. A backup is taken on first replacement so a
# host's prior local config is never lost silently.

set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
src_dir="$repo_root/dev/fleet-user-config"
dst_dir="${XDG_CONFIG_HOME:-$HOME/.config}/worktrunk"
backup_dir="$dst_dir/.pre-fleet-config.$(date -u +%Y%m%dT%H%M%SZ)"

mkdir -p "$dst_dir"

took_backup=0
for name in config.toml approvals.toml; do
	src="$src_dir/$name"
	dst="$dst_dir/$name"

	if [[ ! -f "$src" ]]; then
		echo "ERROR: $src does not exist; aborting." >&2
		exit 1
	fi

	# Already linked to the right target — nothing to do.
	if [[ -L "$dst" ]] && [[ "$(readlink "$dst")" == "$src" ]]; then
		echo "ok       $name (already linked)"
		continue
	fi

	# Anything else at $dst gets backed up before being replaced.
	if [[ -e "$dst" || -L "$dst" ]]; then
		if [[ "$took_backup" -eq 0 ]]; then
			mkdir -p "$backup_dir"
			took_backup=1
			echo "backup   created $backup_dir/"
		fi
		mv "$dst" "$backup_dir/$name"
		echo "backup   moved $name -> $backup_dir/$name"
	fi

	ln -s "$src" "$dst"
	echo "linked   $dst -> $src"
done

echo
echo "Done. Verify with: wt config show"
