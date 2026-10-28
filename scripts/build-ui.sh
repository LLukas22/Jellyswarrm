#!/usr/bin/env bash
set -euo pipefail

cd "$(dirname "${BASH_SOURCE[0]}")/.."

npm --prefix ui ci
npm --prefix ui run build:production
test -s ui/dist/index.html

# Stage the complete bundle before replacing the previously embedded UI.
static=crates/jellyswarrm-proxy/static
staging=$(mktemp -d "${static}.XXXXXX")
trap 'rm -rf "$staging"' EXIT
cp -R ui/dist/. "$staging/"
version=$(node -p "require('./ui/package.json').version.replace(/^v/, '')")
commit=$(git -C ui rev-parse HEAD)
printf 'UI_VERSION=%s\nUI_COMMIT=%s\n' "$version" "$commit" > "$staging/ui-version.env"
rm -rf "$static"
mv "$staging" "$static"
