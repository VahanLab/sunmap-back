#!/usr/bin/env bash
# Importe une nouvelle zone OSM (extrait PBF) et régénère l'archive de tuiles.
#
#   scripts/import-zone.sh <zone.osm.pbf | URL Geofabrik> [--upload] [--purge]
#
#   scripts/import-zone.sh https://download.geofabrik.de/europe/france/ile-de-france-latest.osm.pbf
#
# Enchaîne tout le pipeline documenté dans docs/import-zone.md :
#   1. téléchargement du PBF si on donne une URL (sinon fichier local) ;
#   2. osm-extract.sh — filtrage osmium (bâtiments, végétation, établissements,
#      mobilier urbain) et export GeoJSONSeq ;
#   3. bin/import — remplissage PostGIS (règles tags → hauteur canoniques) ;
#   4. build-pmtiles.py — tiles/sunmap.pmtiles, l'archive vectorielle unique
#      (couches buildings/woods/trees), sur l'emprise TOTALE de la base : les
#      zones déjà importées sont re-tuilées avec, l'archive est globale.
#
# --upload : pousse ensuite l'archive sur Cloudflare R2 (variables R2_* de
#            l'environnement ou de helios-server/.env).
# --purge  : vide les tables buildings/trees/woods après génération — PostGIS
#            ne garde alors que les lieux et contributions. À ne faire QUE si
#            le serveur tourne avec VECTOR_TILES=tiles/sunmap.pmtiles : sans
#            archive, plus de géométrie du tout. (La régénération suivante
#            repassera par un import.)
#
# DATABASE_URL cible la base (défaut : postgres://localhost/sunmap, comme le
# serveur). Relançable sans risque : l'import est un upsert.
set -euo pipefail
cd "$(dirname "$0")/.."

SRC=${1:?Usage: import-zone.sh <zone.osm.pbf | URL Geofabrik> [--upload] [--purge]}
shift
UPLOAD=0
PURGE=0
for arg in "$@"; do
  case "$arg" in
    --upload) UPLOAD=1 ;;
    --purge) PURGE=1 ;;
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

echo "=== 3/4 import PostGIS (buildings, trees, woods, places)"
cargo run --release --bin import -- "$GEOJSONL"

echo "=== 4/4 archive vectorielle → tiles/sunmap.pmtiles"
VENV=.venv-tiles
if [[ ! -x "$VENV/bin/python" ]]; then
  echo "création du venv $VENV…"
  python3 -m venv "$VENV"
  "$VENV/bin/pip" install -q "psycopg[binary]" pmtiles mapbox-vector-tile boto3
fi
"$VENV/bin/python" scripts/build-pmtiles.py --selftest
ARGS=(--out-dir tiles)
[[ $UPLOAD == 1 ]] && ARGS+=(--upload)
"$VENV/bin/python" scripts/build-pmtiles.py "${ARGS[@]}"

if [[ $PURGE == 1 ]]; then
  echo "=== purge des tables géométriques (PostGIS ne garde que les lieux)"
  psql "${DATABASE_URL:-sunmap}" -c "TRUNCATE buildings, trees, woods"
fi

echo "=== terminé"
echo "Serveur : VECTOR_TILES=tiles/sunmap.pmtiles (cf. helios-server/.env)."
