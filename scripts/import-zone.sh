#!/usr/bin/env bash
# Importe une nouvelle zone OSM (extrait PBF) : lieux en base, géométrie dans
# l'archive vectorielle. PostgreSQL ne voit plus passer aucune géométrie.
#
#   scripts/import-zone.sh <zone.osm.pbf | URL Geofabrik> [--upload]
#
#   scripts/import-zone.sh https://download.geofabrik.de/europe/france-latest.osm.pbf --upload
#
# Enchaîne tout le pipeline documenté dans docs/import-zone.md :
#   1. téléchargement du PBF si on donne une URL (sinon fichier local) ;
#   2. osm-extract.sh — filtrage osmium (bâtiments, végétation, établissements,
#      mobilier urbain) et export GeoJSONSeq ;
#   3. bin/tilegen — extrait → tiles/sunmap.pmtiles, l'archive vectorielle
#      unique (règles tags → hauteur canoniques de osm.rs, médiane locale
#      comprise), SANS base de données ;
#   4. bin/import — lieux (établissements + mobilier urbain) vers PostgreSQL,
#      la seule chose qui y reste (DATABASE_URL, défaut
#      postgres://localhost/sunmap).
#
# --upload : pousse ensuite l'archive sur Cloudflare R2 (scripts/r2-upload.py,
#            variables R2_* de l'environnement ou de helios-server/.env).
#
# ⚠ L'archive générée ne couvre que CET extrait : contrairement à l'époque
# PostGIS, il n'y a plus de base cumulative — pour couvrir plusieurs zones,
# partir d'un extrait qui les contient toutes (ex. france-latest).
# Relançable sans risque : l'import des lieux est un upsert.
set -euo pipefail
cd "$(dirname "$0")/.."

SRC=${1:?Usage: import-zone.sh <zone.osm.pbf | URL Geofabrik> [--upload]}
shift
UPLOAD=0
for arg in "$@"; do
  case "$arg" in
    --upload) UPLOAD=1 ;;
    *) echo "option inconnue : $arg" >&2; exit 1 ;;
  esac
done

mkdir -p pbf tiles

case "$SRC" in
  http://*|https://*)
    PBF="pbf/$(basename "$SRC")"
    echo "=== 1/4 téléchargement → $PBF"
    curl -fL --retry 3 -o "$PBF" "$SRC"
    ;;
  *)
    [[ -f "$SRC" ]] || { echo "fichier introuvable : $SRC" >&2; exit 1; }
    PBF="$SRC"
    echo "=== 1/4 PBF local : $PBF"
    ;;
esac

BASE=$(basename "$PBF")
GEOJSONL="pbf/${BASE%.osm.pbf}.geojsonl"
echo "=== 2/4 extraction osmium → $GEOJSONL"
scripts/osm-extract.sh "$PBF" "$GEOJSONL"

echo "=== 3/4 archive vectorielle → tiles/sunmap.pmtiles"
cargo run --release --bin tilegen -- "$GEOJSONL" tiles/sunmap.pmtiles

echo "=== 4/4 lieux (établissements + mobilier) → PostgreSQL"
cargo run --release --bin import -- "$GEOJSONL"

if [[ $UPLOAD == 1 ]]; then
  echo "=== upload R2"
  VENV=.venv-tiles
  if [[ ! -x "$VENV/bin/python" ]]; then
    python3 -m venv "$VENV"
    "$VENV/bin/pip" install -q boto3
  fi
  "$VENV/bin/python" scripts/r2-upload.py tiles/sunmap.pmtiles
fi

echo "=== terminé"
echo "Serveur : VECTOR_TILES=tiles/sunmap.pmtiles (cf. helios-server/.env)."
