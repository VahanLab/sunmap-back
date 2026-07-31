#!/usr/bin/env bash
# Allège un export Meshy pour l'embarquer dans l'app : décimation du maillage
# et réduction des textures.
#
#   scripts/glb-optimize.sh entrée.glb sortie.glb [ratio] [taille_texture]
#
# Les exports Meshy pèsent 30 à 75 Mo — inembarquables, et surtout inutiles :
# sur la carte, un banc mesure quelques dizaines de pixels. `terrace.glb`, la
# référence en place, tient en 2,4 Mo pour 59 000 triangles et une texture de
# 1024 px ; c'est la cible.
set -euo pipefail

IN=${1:?Usage: glb-optimize.sh <entrée.glb> <sortie.glb> [ratio] [taille_texture]}
OUT=${2:?Usage: glb-optimize.sh <entrée.glb> <sortie.glb> [ratio] [taille_texture]}
RATIO=${3:-0.25}
TEX=${4:-1024}

# Version 3 et non 4 : la 4 exige `node:util.styleText`, absent avant
# Node 20.12 — la machine de build tourne en 20.10.
GT="npx --yes @gltf-transform/cli@3"

echo "1/3 — nettoyage (dédoublonnage, suppression du non référencé)…"
$GT dedup "$IN" "$OUT.tmp.glb"

echo "2/3 — décimation du maillage (ratio $RATIO)…"
# `--error` large : sur un objet vu à quelques dizaines de pixels, une
# tolérance serrée empêche la décimation d'atteindre le ratio demandé.
$GT simplify "$OUT.tmp.glb" "$OUT.tmp2.glb" --ratio "$RATIO" --error 0.01

echo "3/3 — réduction des textures (${TEX}px, JPEG)…"
$GT resize "$OUT.tmp2.glb" "$OUT.tmp3.glb" --width "$TEX" --height "$TEX"
$GT jpeg "$OUT.tmp3.glb" "$OUT" --quality 85

rm -f "$OUT.tmp.glb" "$OUT.tmp2.glb" "$OUT.tmp3.glb"
echo "→ $OUT ($(du -h "$OUT" | cut -f1))"
