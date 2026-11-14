#!/usr/bin/env bash
# Deploys your own ebobo room server (the Cloudflare Worker in worker/) to your Cloudflare
# account and prints its URL. The free Workers plan is enough. Needs Node.js (npx).
#
# Usage: scripts/deploy-server.sh
set -euo pipefail
cd "$(dirname "$0")/../worker"

if ! command -v npx >/dev/null 2>&1; then
    echo "Install Node.js first (https://nodejs.org), then run this again." >&2
    exit 1
fi
npm ci --no-audit --no-fund --silent
wrangler=(npx wrangler)   # the version pinned in worker/package.json

who="$("${wrangler[@]}" whoami 2>&1 || true)"
case "$who" in
    *"not authenticated"*|*"Not logged in"*|*"not logged in"*) "${wrangler[@]}" login ;;
esac

if ! out="$("${wrangler[@]}" deploy 2>&1)"; then
    printf '%s\n' "$out" >&2
    exit 1
fi
printf '%s\n' "$out"

url="$(printf '%s\n' "$out" | sed -n -E 's|.*(https://[A-Za-z0-9.-]+\.workers\.dev).*|\1|p' | sed -n 1p)"
if [ -z "$url" ]; then
    echo "Deployed, but no workers.dev URL in the output. Turn on the workers.dev route in the Cloudflare dashboard or add a custom domain." >&2
    exit 1
fi

# A fresh workers.dev name can take a few seconds to resolve.
for _ in 1 2 3 4 5 6 7 8 9 10; do
    if [ "$(curl -fsS "$url/" 2>/dev/null || true)" = "ebobo-signal ok" ]; then
        echo
        echo "Your server: $url"
        echo "In ebobo on every machine that plays together: Settings > Advanced > Server, enter that URL."
        echo "Or start ebobo with: --server $url"
        exit 0
    fi
    sleep 3
done
echo "Deployed to $url, but it does not answer yet. Try opening it in a browser in a minute." >&2
exit 1
