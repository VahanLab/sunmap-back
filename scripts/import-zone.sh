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
#   4. bin/vegoverview — sunmap.pmtiles → tiles/sunmap-veg.pmtiles, l'aperçu
#      de canopée (couche woods seule, z12/z13) que le client lit sous z14.
#      Toujours régénéré en entier : il DÉRIVE de l'archive, il ne se
#      fusionne pas ;
#   5. bin/import — lieux (établissements + mobilier urbain) vers PostgreSQL,
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
  RUN_VEGOVERVIEW=(cargo run --release --quiet --bin vegoverview --)
  RUN_IMPORT=(cargo run --release --quiet --bin import --)
else
  IMAGE=${SUNMAP_TOOLS_IMAGE:-sunmap-tools:local}
  TOOLS=(tilegen vegoverview import)
  # On vérifie la présence des BINAIRES, pas seulement celle de l'image : une
  # image construite avant l'ajout d'un outil existe toujours et passerait un
  # simple `image inspect`, pour échouer en plein import. C'est précisément ce
  # qui attendait `vegoverview` sur une VM ayant déjà importé une fois.
  has_tools() {
    docker image inspect "$1" >/dev/null 2>&1 \
      && docker run --rm --entrypoint /bin/sh "$1" -c \
           'for t in "$@"; do command -v "$t" >/dev/null || exit 1; done' _ "${TOOLS[@]}" \
           >/dev/null 2>&1
  }
  if ! has_tools "$IMAGE"; then
    if [[ -n ${SUNMAP_TOOLS_IMAGE:-} ]]; then
      # Image imposée (registre, tag précis) : la reconstruire sous son nom
      # masquerait ce qui est réellement déployé. On s'arrête et on dit quoi
      # faire.
      echo "l'image $IMAGE n'a pas tous les outils attendus (${TOOLS[*]})." >&2
      echo "Tirer une image à jour (docker pull $IMAGE) ou laisser" >&2
      echo "SUNMAP_TOOLS_IMAGE vide pour en construire une localement." >&2
      exit 1
    fi
    echo "=== construction de $IMAGE (outils manquants ou image absente)"
    docker build -q -t "$IMAGE" . >/dev/null
  fi
  # Image dédiée à l'outillage, jamais celle que sert `docker compose` : la
  # construire ici ne doit pas remplacer l'image de production, qui vient du
  # registre avec son tag.
  DOCKER_RUN=(docker run --rm -v "$PWD:/work" -w /work -u "$(id -u):$(id -g)")
  RUN_TILEGEN=("${DOCKER_RUN[@]}" "$IMAGE" tilegen)
  RUN_VEGOVERVIEW=("${DOCKER_RUN[@]}" "$IMAGE" vegoverview)
  RUN_IMPORT=("${DOCKER_RUN[@]}" -e DATABASE_URL "$IMAGE" import)
fi

case "$SRC" in
  http://*|https://*)
    PBF="pbf/$(basename "$SRC")"
    echo "=== 1/5 téléchargement → $PBF"
    curl -fL --retry 3 -o "$PBF" "$SRC"
    ;;
  *)
    [[ -f "$SRC" ]] || { echo "fichier introuvable : $SRC" >&2; exit 1; }
    PBF="$SRC"
    echo "=== 1/5 PBF local : $PBF"
    ;;
esac

BASE=$(basename "$PBF")
GEOJSONL="pbf/${BASE%.osm.pbf}.geojsonl"
echo "=== 2/5 extraction osmium → $GEOJSONL"
scripts/osm-extract.sh "$PBF" "$GEOJSONL"

echo "=== 3/5 archive vectorielle → tiles/sunmap.pmtiles"
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

echo "=== 4/5 aperçu de canopée → tiles/sunmap-veg.pmtiles"
# Dérivé de l'archive qu'on vient d'écrire, jamais de l'extrait : c'est ce qui
# garantit que l'aperçu porte exactement les emprises boisées que le serveur
# classe. Rien à fusionner ici — il se refait en entier à chaque import, et sa
# couverture suit celle de l'archive.
OVERVIEW=tiles/sunmap-veg.pmtiles
"${RUN_VEGOVERVIEW[@]}" "$ARCHIVE" "$OVERVIEW.tmp"
mv -f "$OVERVIEW.tmp" "$OVERVIEW"

echo "=== 5/5 lieux (établissements + mobilier) → PostgreSQL"
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
  "$VENV/bin/python" scripts/r2-upload.py "$ARCHIVE" "$OVERVIEW"

  # Le cache du Worker est indexé sur l'URL et ignore le remplacement de
  # l'archive : sans purge, les tuiles déjà servies restent périmées.
  echo "=== purge du cache Cloudflare"
  python3 scripts/cf-purge.py
fi

echo "=== terminé"
echo "Serveur : VECTOR_TILES=tiles/sunmap.pmtiles (cf. helios-server/.env)."
echo "Client  : sunmap.pmtiles dès z14, sunmap-veg.pmtiles (z12/z13) en dessous."
