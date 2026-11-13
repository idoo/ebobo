#!/usr/bin/env bash
# Publishes a release: scripts/release.sh v0.2.0
# Builds both downloads from a clean snapshot of the tag (no stray working-tree files, no ROMs),
# then creates the GitHub release with stable asset names so these links always work:
#   https://github.com/idoo/ebobo/releases/latest/download/ebobo-mac.zip
#   https://github.com/idoo/ebobo/releases/latest/download/ebobo-steamdeck.tar.gz
#   https://github.com/idoo/ebobo/releases/latest/download/install-steamdeck.sh
set -euo pipefail
cd "$(dirname "$0")/.."

tag="${1:?usage: scripts/release.sh vX.Y.Z}"
version="${tag#v}"
cargo_version="$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -n 1)"
[ "$version" = "$cargo_version" ] || { echo "Cargo.toml version is $cargo_version, tag says $version" >&2; exit 1; }
# The build uses a `git archive` of the tag, so uncommitted edits never reach a release; still
# refuse by default so nobody tags a commit that is missing work. ALLOW_DIRTY=1 overrides.
if [ -z "${ALLOW_DIRTY:-}" ] && [ -n "$(git status --porcelain --untracked-files=no)" ]; then
    echo "commit your changes first (or ALLOW_DIRTY=1 to release HEAD anyway)" >&2; exit 1
fi

git tag -a "$tag" -m "ebobo $version" 2>/dev/null || echo "tag $tag exists, reusing"
git -c credential.helper='!gh auth git-credential' push -q origin main "$tag"

src="$(mktemp -d)"
trap 'rm -rf "$src"' EXIT
# -m: extracted files get the current time. `git archive` stamps them with the commit time,
# and cargo skips rebuilding sources that look older than its cached artifacts.
git archive "$tag" | tar -x -m -C "$src"
# Reuse the warm build cache outside the snapshot (evaluated here, before the cd).
cache="${CARGO_TARGET_DIR:-$PWD/target-pkg}"
(cd "$src" && CARGO_TARGET_DIR="$cache" scripts/package-mac.sh)
(cd "$src" && scripts/build-linux.sh)

# Gate: nothing game-derived in the downloads.
if (cd "$src/dist" && unzip -l ebobo-mac.zip; tar -tzf ebobo-steamdeck.tar.gz) | grep -i -E '\.(nes|chr|zip)$' | grep -v 'ebobo-mac.zip'; then
    echo "a ROM or tile file ended up in a download, refusing" >&2
    exit 1
fi

# Checksums the in-app updater verifies downloads against (`shasum -a 256` format, bare names).
(cd "$src/dist" && shasum -a 256 ebobo-mac.zip ebobo-steamdeck.tar.gz install-steamdeck.sh > SHA256SUMS)

notes="$(mktemp)"
cat > "$notes" <<EOF
**Mac** (macOS 11+): download \`ebobo-mac.zip\`, unzip it and drag ebobo.app to Applications.

**Steam Deck**: in Desktop Mode open Konsole and run
\`\`\`
curl -sL https://github.com/idoo/ebobo/releases/latest/download/install-steamdeck.sh | bash
\`\`\`

ebobo updates itself after that. No games included, bring your own ROMs.
EOF
gh release create "$tag" --repo idoo/ebobo --title "ebobo $version" --notes-file "$notes" \
    "$src/dist/ebobo-mac.zip" "$src/dist/ebobo-steamdeck.tar.gz" "$src/dist/install-steamdeck.sh" "$src/dist/SHA256SUMS"
echo "released $tag"
