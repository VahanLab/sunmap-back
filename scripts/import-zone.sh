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
#      la seule chose qui y reste. DATABASE_URL est OBLIGATOIRE et sans
#      défaut ; la base visée est annoncée avant écriture.
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

# Les binaires viennent de `cargo` en développement, de l'image Docker sur la
# VM — qui n'a pas de toolchain Rust, et où ces imports ont vocation à tourner
# (les identifiants de la base managée n'ont pas à en sortir).
if command -v cargo >/dev/null 2>&1; then
  RUN_TILEGEN=(cargo run --release --quiet --bin tilegen --)
  RUN_IMPORT=(cargo run --release --quiet --bin import --)
else
  IMAGE=${SUNMAP_TOOLS_IMAGE:-sunmap-tools:local}
  if ! docker image inspect "$IMAGE" >/dev/null 2>&1; then
    echo "=== construction de $IMAGE (une fois)"
    docker build -q -t "$IMAGE" . >/dev/null
  fi
  # Image dédiée à l'outillage, jamais celle que sert `docker compose` : la
  # construire ici ne doit pas remplacer l'image de production, qui vient du
  # registre avec son tag.
  DOCKER_RUN=(docker run --rm -v "$PWD:/work" -w /work -u "$(id -u):$(id -g)")
  RUN_TILEGEN=("${DOCKER_RUN[@]}" "$IMAGE" tilegen)
  RUN_IMPORT=("${DOCKER_RUN[@]}" -e DATABASE_URL "$IMAGE" import)
fi

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
"${RUN_TILEGEN[@]}" "${MERGE[@]}" "$GEOJSONL" "$ARCHIVE.tmp"
mv -f "$ARCHIVE.tmp" "$ARCHIVE"

echo "=== 4/4 lieux (établissements + mobilier) → PostgreSQL"
# `bin/import` charge lui-même `helios-server/.env` et annonce la base visée
# avant d'écrire. Il n'y a plus de repli silencieux : sans DATABASE_URL il
# s'arrête. Pour viser une autre base que celle du `.env` — la production
# depuis un poste de dev, typiquement — la passer explicitement :
#
#   DATABASE_URL='postgres://…' scripts/import-zone.sh …
#
# Une variable déjà présente dans l'environnement l'emporte sur le `.env`
# (dotenvy n'écrase pas), le geste reste donc ponctuel et visible.
"${RUN_IMPORT[@]}" "$GEOJSONL"

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
