#!/bin/sh
# Regenerate the WIT bindings under internal/. Run after changing ../../wit.
#
# componentize-go generates one tree per world. The three worlds import the
# same interfaces and export the same lifecycle, so the imports are kept once
# under internal/imports, the hand-written lifecycle under
# internal/export_plexigraf_plugin_lifecycle, and each world keeps only its export
# glue under internal/<world>.
set -eu
cd "$(dirname "$0")"
mod=github.com/powersj/plexigraf/sdk/go/internal
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

for world in scraper processor exporter; do
    go tool componentize-go -d ../../wit -w "plexigraf:plugin/$world" --ignore-toml-files \
        bindings --format --pkg-name "$mod/bind_$world" -o "$tmp/$world" >/dev/null
done

rm -rf internal/imports internal/scraper/wit_exports internal/processor/wit_exports internal/exporter/wit_exports
mkdir -p internal
cp -R "$tmp/scraper/imports" internal/imports
rm -rf internal/imports/wit_world
for world in scraper processor exporter; do
    mkdir -p "internal/$world"
    cp -R "$tmp/$world/exports/wit_exports" "internal/$world/wit_exports"
done

find internal -name '*.go' -path '*wit_*' -exec sed -i.bak \
    -e "s#$mod/bind_[a-z]*/imports/#$mod/imports/#g" \
    -e "s#$mod/bind_[a-z]*/exports/export_plexigraf_plugin_lifecycle#$mod/export_plexigraf_plugin_lifecycle#g" \
    -e "s#$mod/bind_\([a-z]*\)/exports/export_wit_world#$mod/\1/export_wit_world#g" {} +
find internal -name '*.bak' -delete
