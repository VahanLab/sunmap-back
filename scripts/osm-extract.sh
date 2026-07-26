#!/usr/bin/env bash
# Extrait d'un PBF OSM les seuls objets qui portent ombre ou qu'on affiche,
# et les convertit en GeoJSONSeq pour `cargo run --bin import`.
#
#   scripts/osm-extract.sh ile-de-france.osm.pbf extrait.geojsonl
#
# Extraits Geofabrik : https://download.geofabrik.de/europe/france.html
#   Île-de-France ~336 Mo, France entière ~5,0 Go.
#
# Deux passes, et l'ordre compte : `tags-filter` réduit d'abord le fichier de
# deux ordres de grandeur, ce qui rend l'assemblage des aires (coûteux en
# mémoire) praticable sur la France entière.
set -euo pipefail

IN=${1:?Usage: osm-extract.sh <entrée.osm.pbf> [sortie.geojsonl]}
OUT=${2:-extrait.geojsonl}
TMP="${OUT%.geojsonl}.filtre.osm.pbf"

command -v osmium >/dev/null || { echo "osmium introuvable : brew install osmium-tool"; exit 1; }

echo "1/2 — filtrage des tags…"
# `w/building` garde les ways bâtiment, `r/building` les relations
# multipolygone (les immeubles à cour, majoritaires à Paris — les oublier
# retirait 41 % des casters d'ombre). `building:part` couvre Simple 3D
# Buildings. Le `-t` conserve les objets référencés (nœuds des ways, membres
# des relations), sans quoi il n'y aurait aucune géométrie à assembler.
osmium tags-filter --overwrite -t -o "$TMP" "$IN" \
  w/building r/building w/building:part \
  n/natural=tree \
  nwr/amenity=bar,pub,restaurant,cafe,fast_food,biergarten

echo "2/2 — assemblage des aires et export GeoJSONSeq…"
# osmium assemble les multipolygones ici : les relations sortent en Polygon /
# MultiPolygon avec leurs anneaux intérieurs, ce qui garde les cours creuses.
# `-u type_id` donne les identifiants "w123"/"r456", reconvertis en
# "way/123"/"relation/456" côté Rust pour rester recoupables avec osm.org.
osmium export --overwrite -u type_id -f geojsonseq -o "$OUT" "$TMP"

rm -f "$TMP"
echo "→ $OUT ($(du -h "$OUT" | cut -f1), $(wc -l < "$OUT") features)"
