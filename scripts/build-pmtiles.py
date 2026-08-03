#!/usr/bin/env python3
# /// script
# requires-python = ">=3.10"
# dependencies = [
#     "numpy",
#     "pillow",
#     "psycopg[binary]",
#     "pmtiles",
#     "boto3",
# ]
# ///
"""Génère les archives PMTiles servies statiquement depuis Cloudflare R2.

Deux archives, une par nature d'obstacle (même séparation que les tables
PostGIS — un bâtiment est opaque, une canopée se traverse) :

- ``canopy.pmtiles``    : tuiles PNG identiques à ``GET /canopy/{z}/{x}/{y}``
  (port exact de ``helios-server/src/canopy_tiles.rs``) — R = sommet de
  couronne ×2, G = base ×2 (mètres au-dessus du sol, pas de 0,5 m),
  B = classe de végétation (pas de 40 : feuillu/conifère/palmier ×
  arbre isolé/emprise boisée).
- ``buildings.pmtiles`` : tuiles PNG de hauteurs de bâtiments —
  hauteur en **décimètres** sur deux canaux (R = octet fort, G = octet
  faible, soit ``h_m = (R×256 + G) / 10``, plafond 6 553,5 m),
  B = 255 si la hauteur vient d'un tag OSM, 0 si elle est estimée
  (médiane locale). Le pas décimétrique évite le plafond de 127,5 m du
  codage canopée (La Défense, tour Eiffel).

Grille : slippy 512 px, z12–15 — les mêmes bornes que le masque Metal
client (cf. ``canopy_tiles::{MIN_Z, MAX_Z}``). Une tuile sans donnée n'est
pas écrite : PMTiles est creux, le client traite l'absence comme « vide ».

La source est **PostGIS**, pas le PBF directement : les règles tags →
hauteur (``osm::building_from``, ``osm::height_from_tags``) vivent dans le
Rust et ne doivent jamais être dupliquées (cf. AGENTS.md). Le pipeline
complet depuis un extrait Geofabrik est donc :

    scripts/osm-extract.sh ile-de-france     # PBF → GeoJSON filtré
    cargo run --release --bin import -- ...  # → PostGIS (règles canoniques)
    python3 scripts/build-pmtiles.py         # PostGIS → PMTiles

Installation (pas d'uv sur la machine) :

    python3 -m venv .venv-tiles
    .venv-tiles/bin/pip install numpy pillow "psycopg[binary]" pmtiles boto3
    .venv-tiles/bin/python scripts/build-pmtiles.py --selftest

Génération Île-de-France (DATABASE_URL lu dans helios-server/.env) :

    .venv-tiles/bin/python scripts/build-pmtiles.py --out-dir tiles/

Envoi vers R2 — au choix le flag ``--upload`` (boto3, variables
R2_ACCOUNT_ID / R2_ACCESS_KEY_ID / R2_SECRET_ACCESS_KEY / R2_BUCKET) ou
rclone :

    rclone copyto tiles/canopy.pmtiles r2:sunmap-tiles/canopy.pmtiles

Côté client, R2 sert les archives en l'état : un lecteur PMTiles fait des
requêtes HTTP Range sur le fichier (pas besoin de Worker pour un usage
natif iOS ; le Worker officiel protomaps n'est utile que pour exposer des
URLs ``/z/x/y`` classiques).
"""

from __future__ import annotations

import argparse
import io
import json
import math
import os
import sys
from dataclasses import dataclass
from multiprocessing import Pool

import numpy as np
from PIL import Image

TILE_SIZE = 512
# Bornes du masque client — garder synchro avec canopy_tiles::{MIN_Z, MAX_Z}.
MIN_Z = 12
MAX_Z = 15
# Canal B canopée : classes espacées de 40 (cf. canopy_tiles::CLASS_STEP).
CLASS_STEP = 40
LEAF_CODES = {"broadleaved": 0, "needleleaved": 1, "palm": 2}
# Marge de requête : un houppier (~30 m) dont le tronc est dans la tuile
# voisine déborde ici — même valeur que le handler `canopy_tile`.
PAD_M = 30.0

# ------------------------------------------------------------------ géométrie
# Ports exacts de dem.rs (world_px) et canopy_tiles.rs (tile_bounds).


def world_px(lat: float, lng: float) -> tuple[float, float]:
    """Pixel monde Web Mercator z15 (tuiles 512 px)."""
    n = TILE_SIZE * 2.0**15
    wx = (lng + 180.0) / 360.0 * n
    wy = (1.0 - math.asinh(math.tan(math.radians(lat))) / math.pi) / 2.0 * n
    return wx, wy


def tile_bounds(z: int, x: int, y: int) -> tuple[float, float, float, float]:
    """Bornes géographiques (s, w, n, e) d'une tuile slippy."""
    n = 2.0**z
    lon_w = x / n * 360.0 - 180.0
    lon_e = (x + 1) / n * 360.0 - 180.0
    lat_n = math.degrees(math.atan(math.sinh(math.pi * (1.0 - 2.0 * y / n))))
    lat_s = math.degrees(math.atan(math.sinh(math.pi * (1.0 - 2.0 * (y + 1) / n))))
    return lat_s, lon_w, lat_n, lon_e


def tile_of(lat: float, lon: float, z: int) -> tuple[int, int]:
    n = 2.0**z
    x = int((lon + 180.0) / 360.0 * n)
    y = int((1.0 - math.asinh(math.tan(math.radians(lat))) / math.pi) / 2.0 * n)
    return x, y


class TilePx:
    """Conversion lat/lon → pixel de la tuile (z, x, y) demandée."""

    def __init__(self, z: int, x: int, y: int):
        self.scale = 2.0 ** (15 - z)
        self.origin_x = x * float(TILE_SIZE)
        self.origin_y = y * float(TILE_SIZE)
        lat_s, _, lat_n, _ = tile_bounds(z, x, y)
        mid_lat = (lat_s + lat_n) / 2.0
        self.meters_per_px = (
            40_075_016.686 * math.cos(math.radians(mid_lat)) / (TILE_SIZE * 2.0**z)
        )

    def to_px(self, lat: float, lon: float) -> tuple[float, float]:
        wx, wy = world_px(lat, lon)
        return wx / self.scale - self.origin_x, wy / self.scale - self.origin_y


# -------------------------------------------------------------- rasterisation
# Port du scanline pair-impair de canopy_tiles::rasterize : tous les anneaux
# d'un multipolygone ensemble (une clairière rebascule en « dehors »), test au
# centre du pixel (scan_y = ligne + 0,5), bornes arrondies comme en Rust.


def scanline_rows(rings_px: list[np.ndarray]):
    """Itère (ligne, cx0, cx1) des travées intérieures du polygone."""
    edges = []
    for ring in rings_px:
        if len(ring) < 3:
            continue
        nxt = np.roll(ring, -1, axis=0)
        edges.append(np.column_stack([ring, nxt]))
    if not edges:
        return
    e = np.concatenate(edges)  # colonnes : x1, y1, x2, y2
    x1, y1, x2, y2 = e[:, 0], e[:, 1], e[:, 2], e[:, 3]

    y_min = max(math.floor(min(np.min(y1), np.min(y2))), 0)
    y_max = min(math.ceil(max(np.max(y1), np.max(y2))), TILE_SIZE - 1)
    for row in range(y_min, y_max + 1):
        scan_y = row + 0.5
        m = (y1 <= scan_y) != (y2 <= scan_y)
        if not m.any():
            continue
        xs = np.sort(x1[m] + (scan_y - y1[m]) / (y2[m] - y1[m]) * (x2[m] - x1[m]))
        for i in range(0, len(xs) - 1, 2):
            cx0 = int(round(max(xs[i], 0.0)))
            cx1 = int(round(min(xs[i + 1], TILE_SIZE - 1.0)))
            if cx1 >= cx0:
                yield row, cx0, min(cx1, TILE_SIZE - 1)


def geojson_rings(geom: dict, px: TilePx) -> list[np.ndarray]:
    """Tous les anneaux (extérieurs et trous) du (Multi)Polygon, en px tuile."""
    if geom["type"] == "Polygon":
        polys = [geom["coordinates"]]
    elif geom["type"] == "MultiPolygon":
        polys = geom["coordinates"]
    else:
        return []
    out = []
    for poly in polys:
        for ring in poly:
            pts = np.array(
                [px.to_px(lat, lon) for lon, lat in ring], dtype=np.float64
            )
            out.append(pts)
    return out


# --------------------------------------------------------------------- tuiles


def render_canopy(z, x, y, woods, trees) -> bytes | None:
    """Tuile canopée — mêmes règles de fusion que canopy_tiles::rasterize."""
    px = TilePx(z, x, y)
    top = np.zeros((TILE_SIZE, TILE_SIZE), dtype=np.float32)
    base = np.zeros((TILE_SIZE, TILE_SIZE), dtype=np.float32)
    wood_mask = np.zeros((TILE_SIZE, TILE_SIZE), dtype=bool)
    leaf = np.zeros((TILE_SIZE, TILE_SIZE), dtype=np.uint8)

    # Bois : le plus haut gagne le pixel, base au sol (pas de tronc dégagé).
    for height_m, leaf_type, geom in woods:
        code = LEAF_CODES.get(leaf_type or "", 0)
        for row, cx0, cx1 in scanline_rows(geojson_rings(geom, px)):
            seg = slice(cx0, cx1 + 1)
            wood_mask[row, seg] = True
            taller = height_m > top[row, seg]
            top[row, seg][taller] = height_m
            base[row, seg][taller] = 0.0
            leaf[row, seg][taller] = code

    # Arbres isolés : disque de couronne, le tronc laisse passer dessous.
    for height_m, crown_radius_m, leaf_type, lat, lng in trees:
        cx, cy = px.to_px(lat, lng)
        radius_px = max(crown_radius_m / px.meters_per_px, 0.5)
        t_top = np.float32(height_m)
        t_base = np.float32(max(min(t_top - 2.0 * crown_radius_m, t_top - 1.0), 0.0))

        x0 = int(max(math.floor(cx - radius_px), 0))
        x1 = int(min(math.ceil(cx + radius_px), TILE_SIZE - 1))
        y0 = int(max(math.floor(cy - radius_px), 0))
        y1 = int(min(math.ceil(cy + radius_px), TILE_SIZE - 1))
        if x1 < x0 or y1 < y0:
            continue
        xs = np.arange(x0, x1 + 1) + 0.5 - cx
        ys = np.arange(y0, y1 + 1) + 0.5 - cy
        inside = xs[None, :] ** 2 + ys[:, None] ** 2 <= radius_px**2

        win = (slice(y0, y1 + 1), slice(x0, x1 + 1))
        had_canopy = top[win] > 0.0
        upd = inside & (t_top > top[win])
        new_base = np.where(had_canopy, np.minimum(base[win], t_base), t_base)
        top[win][upd] = t_top
        base[win][upd] = new_base[upd]
        leaf[win][upd] = LEAF_CODES.get(leaf_type or "", 0)

    if not top.any():
        return None
    rgb = np.zeros((TILE_SIZE, TILE_SIZE, 3), dtype=np.uint8)
    rgb[:, :, 0] = np.clip(np.round(top * 2.0), 0, 255)
    rgb[:, :, 1] = np.clip(np.round(base * 2.0), 0, 255)
    has = top > 0.0
    klass = CLASS_STEP * (1 + leaf + np.where(wood_mask, 3, 0))
    rgb[:, :, 2] = np.where(has, klass, 0).astype(np.uint8)
    return encode_png(rgb)


def render_buildings(z, x, y, buildings) -> bytes | None:
    """Tuile bâtiments : hauteur en décimètres (R fort, G faible), B = source."""
    px = TilePx(z, x, y)
    height = np.zeros((TILE_SIZE, TILE_SIZE), dtype=np.float32)
    from_osm = np.zeros((TILE_SIZE, TILE_SIZE), dtype=bool)

    for height_m, height_from_osm, geom in buildings:
        for row, cx0, cx1 in scanline_rows(geojson_rings(geom, px)):
            seg = slice(cx0, cx1 + 1)
            taller = height_m > height[row, seg]
            height[row, seg][taller] = height_m
            from_osm[row, seg][taller] = height_from_osm

    if not height.any():
        return None
    dm = np.clip(np.round(height * 10.0), 0, 65535).astype(np.uint16)
    rgb = np.zeros((TILE_SIZE, TILE_SIZE, 3), dtype=np.uint8)
    rgb[:, :, 0] = dm >> 8
    rgb[:, :, 1] = dm & 0xFF
    rgb[:, :, 2] = np.where(from_osm, 255, 0)
    return encode_png(rgb)


def encode_png(rgb: np.ndarray) -> bytes:
    buf = io.BytesIO()
    Image.fromarray(rgb, "RGB").save(buf, "PNG", optimize=True)
    return buf.getvalue()


# ------------------------------------------------------------------- PostGIS
# Une connexion par worker, ouverte paresseusement (fork + libpq ne font pas
# bon ménage avec une connexion héritée du parent).

_worker = {}


def worker_init(dsn: str, layer: str):
    _worker["dsn"] = dsn
    _worker["layer"] = layer
    _worker["conn"] = None


def _conn():
    if _worker["conn"] is None:
        import psycopg

        _worker["conn"] = psycopg.connect(_worker["dsn"])
    return _worker["conn"]


def render_tile(zxy: tuple[int, int, int]):
    """Rend une tuile ; retourne (tileid, png) ou None si elle est vide."""
    z, x, y = zxy
    s, w, n, e = tile_bounds(z, x, y)
    pad_lat = PAD_M / 111_320.0
    pad_lon = pad_lat / math.cos(math.radians((s + n) / 2.0))
    bbox = (w - pad_lon, s - pad_lat, e + pad_lon, n + pad_lat)

    cur = _conn().cursor()
    if _worker["layer"] == "canopy":
        cur.execute(
            "SELECT height_m, leaf_type, ST_AsGeoJSON(geom) FROM woods "
            "WHERE geom && ST_MakeEnvelope(%s, %s, %s, %s, 4326)",
            bbox,
        )
        woods = [(h, lt, json.loads(g)) for h, lt, g in cur.fetchall()]
        cur.execute(
            "SELECT height_m, crown_radius_m, leaf_type, ST_Y(geom), ST_X(geom) "
            "FROM trees WHERE geom && ST_MakeEnvelope(%s, %s, %s, %s, 4326)",
            bbox,
        )
        trees = cur.fetchall()
        png = render_canopy(z, x, y, woods, trees)
    else:
        cur.execute(
            "SELECT height_m, height_from_osm, ST_AsGeoJSON(geom) FROM buildings "
            "WHERE geom && ST_MakeEnvelope(%s, %s, %s, %s, 4326)",
            bbox,
        )
        buildings = [(h, src, json.loads(g)) for h, src, g in cur.fetchall()]
        png = render_buildings(z, x, y, buildings)

    if png is None:
        return None
    from pmtiles.tile import zxy_to_tileid

    return zxy_to_tileid(z, x, y), png


def db_extent(dsn: str, layer: str) -> tuple[float, float, float, float]:
    """Emprise (s, w, n, e) des données du layer — délimite les tuiles à rendre."""
    import psycopg

    tables = ["buildings"] if layer == "buildings" else ["woods", "trees"]
    s, w, n, e = math.inf, math.inf, -math.inf, -math.inf
    with psycopg.connect(dsn) as conn:
        for table in tables:
            row = conn.execute(
                f"SELECT ST_XMin(x), ST_YMin(x), ST_XMax(x), ST_YMax(x) "
                f"FROM (SELECT ST_Extent(geom) AS x FROM {table}) AS sub"
            ).fetchone()
            if row and row[0] is not None:
                w, s = min(w, row[0]), min(s, row[1])
                e, n = max(e, row[2]), max(n, row[3])
    if not math.isfinite(s):
        raise SystemExit(f"tables {tables} vides — lancer bin/import d'abord")
    return s, w, n, e


# -------------------------------------------------------------------- PMTiles


def build_layer(layer: str, dsn: str, out_path: str, bbox, zooms, jobs: int):
    from pmtiles.tile import Compression, TileType, zxy_to_tileid
    from pmtiles.writer import Writer

    s, w, n, e = bbox
    # Toutes les tuiles de l'emprise, triées par tileid : le writer exige des
    # écritures ordonnées pour produire une archive « clustered » (lectures
    # Range voisines côté client).
    tiles = []
    for z in zooms:
        x0, y0 = tile_of(n, w, z)
        x1, y1 = tile_of(s, e, z)
        for x in range(min(x0, x1), max(x0, x1) + 1):
            for y in range(min(y0, y1), max(y0, y1) + 1):
                tiles.append((zxy_to_tileid(z, x, y), (z, x, y)))
    tiles.sort()
    print(f"[{layer}] {len(tiles)} tuiles candidates (z{zooms[0]}–z{zooms[-1]})")

    written = 0
    with open(out_path, "wb") as f:
        writer = Writer(f)
        with Pool(jobs, initializer=worker_init, initargs=(dsn, layer)) as pool:
            for i, result in enumerate(
                pool.imap(render_tile, [zxy for _, zxy in tiles], chunksize=8)
            ):
                if result is not None:
                    writer.write_tile(*result)
                    written += 1
                if (i + 1) % 500 == 0:
                    print(f"[{layer}] {i + 1}/{len(tiles)} rendues, {written} non vides")

        if written == 0:
            # finalize() plante sur une archive vide, et un fichier sans tuile
            # ne servirait à rien : autant échouer clairement.
            raise SystemExit(f"[{layer}] aucune tuile non vide dans l'emprise")
        writer.finalize(
            {
                "tile_type": TileType.PNG,
                "tile_compression": Compression.NONE,
                "min_zoom": zooms[0],
                "max_zoom": zooms[-1],
                "min_lon_e7": int(w * 1e7),
                "min_lat_e7": int(s * 1e7),
                "max_lon_e7": int(e * 1e7),
                "max_lat_e7": int(n * 1e7),
                "center_zoom": zooms[0],
                "center_lon_e7": int((w + e) / 2 * 1e7),
                "center_lat_e7": int((s + n) / 2 * 1e7),
            },
            {
                "name": f"sunmap-{layer}",
                "attribution": "© OpenStreetMap contributors",
                "description": (
                    "Hauteurs de canopée (R=sommet×2, G=base×2, B=classe)"
                    if layer == "canopy"
                    else "Hauteurs de bâtiments (décimètres : R×256+G, B=255 si tag OSM)"
                ),
            },
        )
    size_mb = os.path.getsize(out_path) / 1e6
    print(f"[{layer}] {written} tuiles écrites → {out_path} ({size_mb:.1f} Mo)")


# ------------------------------------------------------------------------- R2


def upload_r2(paths: list[str]):
    """Pousse les archives sur R2 (API S3, multipart géré par boto3).

    Les identifiants viennent de l'environnement, sinon de
    ``helios-server/.env`` (gitignoré) — même logique que ``DATABASE_URL``.
    """
    import boto3

    keys = ("R2_ACCOUNT_ID", "R2_ACCESS_KEY_ID", "R2_SECRET_ACCESS_KEY", "R2_BUCKET")
    r2 = {k: os.environ.get(k) or dotenv_value(k) for k in keys}
    missing = [k for k, v in r2.items() if not v]
    if missing:
        raise SystemExit(
            f"variables R2 manquantes : {', '.join(missing)} "
            "(environnement ou helios-server/.env — cf. docs/import-zone.md)"
        )
    s3 = boto3.client(
        "s3",
        endpoint_url=f"https://{r2['R2_ACCOUNT_ID']}.r2.cloudflarestorage.com",
        aws_access_key_id=r2["R2_ACCESS_KEY_ID"],
        aws_secret_access_key=r2["R2_SECRET_ACCESS_KEY"],
        region_name="auto",
    )
    for path in paths:
        key = os.path.basename(path)
        print(f"[r2] {path} → s3://{r2['R2_BUCKET']}/{key}")
        s3.upload_file(path, r2["R2_BUCKET"], key)


# -------------------------------------------------------------------- selftest


def selftest():
    """Reproduit les tests unitaires de canopy_tiles.rs — sans base de données."""
    # bounds_roundtrip : la tuile z15 de Notre-Dame contient Notre-Dame.
    lat, lon = 48.853, 2.3499
    x, y = tile_of(lat, lon, 15)
    s, w, n, e = tile_bounds(15, x, y)
    assert s < lat < n and w < lon < e, "tile_bounds"

    # tree_disc_rasterizes_with_trunk_clearance : sommet 10 m, base 4 m.
    s, w, n, e = tile_bounds(15, 16596, 11273)
    trees = [(10.0, 3.0, "broadleaved", (s + n) / 2.0, (w + e) / 2.0)]
    png = render_canopy(15, 16596, 11273, [], trees)
    assert png is not None
    img = np.asarray(Image.open(io.BytesIO(png)))
    lit = np.argwhere(img[:, :, 0] > 0)
    assert len(lit) > 0, "la couronne doit couvrir des pixels"
    r, c = lit[0]
    assert img[r, c, 0] == 20, f"sommet ×2 : {img[r, c, 0]}"  # 10 m → 20
    assert img[r, c, 1] == 8, f"base ×2 : {img[r, c, 1]}"  # 10 − 2×3 = 4 m → 8
    assert img[r, c, 2] == CLASS_STEP, "arbre isolé feuillu"

    # Bois feuillu 18 m sur toute la tuile : classe 40×(1+0+3)=160, base 0.
    ring = [[w, s], [e, s], [e, n], [w, n], [w, s]]
    geom = {"type": "Polygon", "coordinates": [ring]}
    png = render_canopy(15, 16596, 11273, [(18.0, "broadleaved", geom)], [])
    img = np.asarray(Image.open(io.BytesIO(png)))
    mid = img[256, 256]
    assert mid[0] == 36 and mid[1] == 0 and mid[2] == 160, f"bois : {mid}"

    # Bâtiments : 12,5 m estimé → 125 dm → (0, 125, 0).
    png = render_buildings(15, 16596, 11273, [(12.5, False, geom)])
    img = np.asarray(Image.open(io.BytesIO(png)))
    mid = img[256, 256]
    assert tuple(mid) == (0, 125, 0), f"bâtiment : {mid}"
    # 260 m avec tag → 2600 dm → (10, 40, 255).
    png = render_buildings(15, 16596, 11273, [(260.0, True, geom)])
    mid = np.asarray(Image.open(io.BytesIO(png)))[256, 256]
    assert tuple(mid) == (10, 40, 255), f"tour : {mid}"

    print("selftest OK — rasterisation conforme à canopy_tiles.rs")


# ------------------------------------------------------------------------ CLI


def dotenv_value(key: str) -> str | None:
    """Valeur de ``key`` dans helios-server/.env (vide = absente)."""
    env_path = os.path.join(os.path.dirname(__file__), "..", "helios-server", ".env")
    try:
        with open(env_path) as f:
            for line in f:
                line = line.strip()
                if line.startswith(f"{key}="):
                    return line.split("=", 1)[1] or None
    except OSError:
        pass
    return None


def read_env_database_url() -> str:
    """DATABASE_URL de l'environnement, sinon de helios-server/.env."""
    return (
        os.environ.get("DATABASE_URL")
        or dotenv_value("DATABASE_URL")
        # Même défaut que le serveur (cf. helios-server/README.md).
        or "postgres://localhost/sunmap"
    )


def main():
    p = argparse.ArgumentParser(
        description=__doc__.split("\n")[0],
        formatter_class=argparse.RawDescriptionHelpFormatter,
    )
    p.add_argument(
        "--layer",
        choices=["buildings", "canopy", "both"],
        default="both",
        help="archives à générer (défaut : les deux)",
    )
    p.add_argument("--out-dir", default="tiles", help="dossier de sortie")
    p.add_argument(
        "--database-url",
        default=None,
        help="PostGIS (défaut : $DATABASE_URL, sinon helios-server/.env, "
        "sinon postgres://localhost/sunmap)",
    )
    p.add_argument(
        "--bbox",
        default=None,
        metavar="S,W,N,E",
        help="emprise en degrés (défaut : ST_Extent des tables)",
    )
    p.add_argument("--min-zoom", type=int, default=MIN_Z)
    p.add_argument("--max-zoom", type=int, default=MAX_Z)
    p.add_argument("--jobs", type=int, default=os.cpu_count() or 4)
    p.add_argument("--upload", action="store_true", help="pousse sur R2 après génération")
    p.add_argument("--selftest", action="store_true", help="tests sans base de données")
    args = p.parse_args()

    if args.selftest:
        selftest()
        return

    dsn = args.database_url or read_env_database_url()
    if not dsn:
        raise SystemExit("DATABASE_URL introuvable (env, --database-url ou helios-server/.env)")

    zooms = list(range(args.min_zoom, args.max_zoom + 1))
    layers = ["buildings", "canopy"] if args.layer == "both" else [args.layer]
    os.makedirs(args.out_dir, exist_ok=True)

    outputs = []
    for layer in layers:
        if args.bbox:
            s, w, n, e = (float(v) for v in args.bbox.split(","))
        else:
            s, w, n, e = db_extent(dsn, layer)
        print(f"[{layer}] emprise : {s:.4f},{w:.4f} → {n:.4f},{e:.4f}")
        out_path = os.path.join(args.out_dir, f"{layer}.pmtiles")
        build_layer(layer, dsn, out_path, (s, w, n, e), zooms, args.jobs)
        outputs.append(out_path)

    if args.upload:
        upload_r2(outputs)


if __name__ == "__main__":
    main()
