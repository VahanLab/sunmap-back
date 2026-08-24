#!/usr/bin/env bash
# Trie la sortie de `fastlane snapshot` (ios/SunMap/screenshots/<langue>/)
# vers l'arborescence attendue par `asc push:shots`
# (tools/asc/screenshots/<locale>/<DISPLAY_TYPE>/NN-nom.png).
#
# - iPhone 17 Pro Max → APP_IPHONE_67 (le 6,9" se dépose là, cf. README) ;
# - iPad Pro 13-inch → APP_IPAD_PRO_3GEN_129 ;
# - la locale App Store de l'italien est `it`, pas `it-IT` ;
# - le préfixe numérique des noms est conservé : c'est lui qui fixe l'ordre
#   d'affichage sur la fiche.
set -euo pipefail

SRC="$(cd "$(dirname "$0")/../../ios/SunMap/screenshots" && pwd)"
DST="$(cd "$(dirname "$0")" && pwd)/screenshots"

for lang_dir in "$SRC"/*/; do
  lang="$(basename "$lang_dir")"
  case "$lang" in
    fr-FR|en-US|de-DE|es-ES|it-IT) ;;
    *) continue ;;
  esac
  asc_locale="$lang"
  [ "$lang" = "it-IT" ] && asc_locale="it"

  iphone_dst="$DST/$asc_locale/APP_IPHONE_67"
  ipad_dst="$DST/$asc_locale/APP_IPAD_PRO_3GEN_129"
  rm -rf "$iphone_dst" "$ipad_dst"
  mkdir -p "$iphone_dst" "$ipad_dst"

  for png in "$lang_dir"*.png; do
    name="$(basename "$png")"
    case "$name" in
      iPhone*) cp "$png" "$iphone_dst/${name#iPhone * Max-}" ;;
      iPad*)   cp "$png" "$ipad_dst/${name#iPad *)-}" ;;
    esac
  done
  echo "$asc_locale : $(ls "$iphone_dst" | wc -l | tr -d ' ') iPhone, $(ls "$ipad_dst" | wc -l | tr -d ' ') iPad"
done
