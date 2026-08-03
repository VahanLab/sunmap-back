#!/usr/bin/env bash
# Importe une nouvelle zone OSM (extrait PBF) : lieux en base, géométrie dans
# l'archive vectorielle. PostgreSQL ne voit plus passer aucune géométrie.
#
#   scripts/import-zone.sh <zone.osm.pbf | URL Geofabrik> [--upload] [--replace]
#
#   scripts/import-zone.sh https://download.geofabrik.de/europe/france-latest.osm.pbf --upload
#
# Enchaîne tout le pipeline documenté dans docs/import-zone.md :
#   1. téléchargement du PBF si on donne une URL (sinon fichier local) ;
#   2. osm-extract.sh — filtrage osmium (bâtiments, végétation, établissements,
#      mobilier urbain) et export GeoJSONSeq ;
#   3. bin/tilegen — extrait → tiles/sunmap.pmtiles, l'archive vectorielle
#      unique (règles tags → hauteur canoniques de osm.rs, médiane locale
#      comprise), SANS base de données. La zone est FUSIONNÉE dans l'archive
#      existante si elle est là : ajouter une région n'efface pas les
#      précédentes (--replace pour repartir de zéro) ;
#   4. bin/import — lieux (établissements + mobilier urbain) vers PostgreSQL,
#      la seule chose qui y reste (DATABASE_URL, défaut
#      postgres://localhost/sunmap).
#
# --upload  : pousse ensuite l'archive sur Cloudflare R2 (scripts/r2-upload.py)
#             PUIS purge le cache Cloudflare (scripts/cf-purge.py) — sans
#             quoi les tuiles déjà servies restent périmées jusqu'à un jour.
# --replace : ignore l'archive existante au lieu d'y fusionner (repartir
#             d'une couverture propre, ou changer de découpage).
#
# Relançable sans risque : la fusion réécrit les objets par identifiant OSM
# (le nouvel extrait gagne) et l'import des lieux est un upsert.
set -euo pipefail
cd "$(dirname "$0")/.."

SRC=${1:?Usage: import-zone.sh <zone.osm.pbf | URL Geofabrik> [--upload]}
shift
UPLOAD=0
REPLACE=0
for arg in "$@"; do
  case "$arg" in
    --upload) UPLOAD=1 ;;
    --replace) REPLACE=1 ;;
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
# Fusion par-dessus l'archive existante : ajouter une région ne doit pas
# effacer celles déjà couvertes. tilegen écrit dans un fichier temporaire —
# il lit la base pendant qu'il produit la sortie, les deux ne peuvent pas
# être le même fichier.
ARCHIVE=tiles/sunmap.pmtiles
MERGE=()
if [[ $REPLACE == 0 && -f "$ARCHIVE" ]]; then
  echo "    (fusion dans l'archive existante)"
  MERGE=(--merge "$ARCHIVE")
fi
cargo run --release --bin tilegen -- "${MERGE[@]}" "$GEOJSONL" "$ARCHIVE.tmp"
mv -f "$ARCHIVE.tmp" "$ARCHIVE"

echo "=== 4/4 lieux (établissements + mobilier) → PostgreSQL"
cargo run --release --bin import -- "$GEOJSONL"

if [[ $UPLOAD == 1 ]]; then
  echo "=== upload R2"
  VENV=.venv-tiles
  if [[ ! -x "$VENV/bin/python" ]]; then
    python3 -m venv "$VENV"
    "$VENV/bin/pip" install -q boto3
  fi
  "$VENV/bin/python" scripts/r2-upload.py "$ARCHIVE"

  # Le cache du Worker est indexé sur l'URL et ignore le remplacement de
  # l'archive : sans purge, les tuiles déjà servies restent périmées.
  echo "=== purge du cache Cloudflare"
  python3 scripts/cf-purge.py
fi

echo "=== terminé"
echo "Serveur : VECTOR_TILES=tiles/sunmap.pmtiles (cf. helios-server/.env)."
