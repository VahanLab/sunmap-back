#!/usr/bin/env python3
# /// script
# requires-python = ">=3.10"
# dependencies = [
#     "psycopg[binary]",
#     "pmtiles",
#     "mapbox-vector-tile",
#     "boto3",
# ]
# ///
"""Génère ``sunmap.pmtiles`` : l'artefact vectoriel unique de la géométrie.

Une seule archive PMTiles, tuiles **MVT** (Mapbox Vector Tiles) à un seul
niveau de zoom (z14, extent 4096 → ~0,6 m de précision, petit devant le
pixel DSM de ~1,57 m), trois couches :

- ``buildings`` : polygones, attributs ``id`` (osm_id), ``name``,
  ``height_m``, ``height_from_osm`` ;
- ``woods``     : polygones, attributs ``id``, ``name``, ``height_m``,
  ``height_from_osm``, ``leaf_type`` ;
- ``trees``     : points, attributs ``id``, ``height_m``,
  ``crown_radius_m``, ``leaf_type``.

Trois consommateurs, même fichier : le serveur (rasterisation DSM pour la
classification soleil/ombre — ``helios-server/src/vtiles.rs``), le masque
Metal client (rasterisation GPU), l'affichage Mapbox (arbres 3D,
extrusions). C'est ce qui remplace les tables PostGIS ``buildings`` /
``trees`` / ``woods`` au runtime — PostGIS ne reste que la zone de transit
de l'import.

Deux choix hérités de ``btiles.rs`` (tuiles internes HBT) :

- **aucune simplification, aucun élagage** (tippecanoe est exclu pour ça) :
  ces tuiles nourrissent un calcul, pas seulement un rendu ;
- un objet à cheval sur plusieurs tuiles est écrit **entier dans chacune**,
  jamais découpé : la rasterisation garde la géométrie complète, le lecteur
  dédoublonne par ``id``.

La source est **PostGIS** (remplie par ``bin/import``) : les règles tags →
hauteur vivent dans le Rust et ne sont jamais dupliquées ici. Pipeline
complet : ``scripts/import-zone.sh`` / ``docs/import-zone.md``.

Installation (pas d'uv sur la machine) :

    python3 -m venv .venv-tiles
    .venv-tiles/bin/pip install "psycopg[binary]" pmtiles mapbox-vector-tile boto3
    .venv-tiles/bin/python scripts/build-pmtiles.py --selftest

Envoi vers R2 : ``--upload`` (variables ``R2_*`` de l'environnement ou de
``helios-server/.env``), ou rclone. R2 sert l'archive telle quelle en
requêtes HTTP Range.
"""

from __future__ import annotations

import argparse
import gzip
import json
import math
import os
from multiprocessing import Pool

# Zoom unique de l'archive. z14 : tuile ~2,4 km, extent 4096 → pas de
# quantification ~0,6 m. Les lecteurs sur- ou sous-échantillonnent librement
# (le vectoriel n'a pas de résolution).
ZOOM = 14
EXTENT = 4096
R_MERC = 6378137.0

# ------------------------------------------------------------------ géométrie


def mercator(lat: float, lon: float) -> tuple[float, float]:
    """EPSG:4326 → EPSG:3857 (mètres Web Mercator, y vers le nord)."""
    x = R_MERC * math.radians(lon)
    y = R_MERC * math.asinh(math.tan(math.radians(lat)))
    return x, y


def tile_bounds_3857(z: int, x: int, y: int) -> tuple[float, float, float, float]:
    """(minx, miny, maxx, maxy) de la tuile slippy, en EPSG:3857."""
    world = 2.0 * math.pi * R_MERC
    size = world / 2.0**z
    minx = -world / 2.0 + x * size
    maxy = world / 2.0 - y * size
    return minx, maxy - size, minx + size, maxy


def tile_of(lat: float, lon: float, z: int) -> tuple[int, int]:
    n = 2.0**z
    tx = int((lon + 180.0) / 360.0 * n)
    ty = int((1.0 - math.asinh(math.tan(math.radians(lat))) / math.pi) / 2.0 * n)
    return tx, ty


def tile_range(s: float, w: float, n: float, e: float) -> tuple[int, int, int, int]:
    x0, y0 = tile_of(n, w, ZOOM)
    x1, y1 = tile_of(s, e, ZOOM)
    return min(x0, x1), min(y0, y1), max(x0, x1), max(y0, y1)


# ------------------------------------------------------------------- PostGIS
# Une connexion par worker, ouverte paresseusement (fork + libpq ne font pas
# bon ménage avec une connexion héritée du parent).

_worker: dict = {}


def worker_init(dsn: str):
    _worker["dsn"] = dsn
    _worker["conn"] = None


def _conn():
    if _worker["conn"] is None:
        import psycopg

        _worker["conn"] = psycopg.connect(_worker["dsn"])
    return _worker["conn"]


def fetch_features(bbox4326: tuple[float, float, float, float]) -> dict[str, list]:
    """Objets des trois tables intersectant la bbox (w, s, e, n) — entiers."""
    w, s, e, n = bbox4326
    cur = _conn().cursor()
    env = (w, s, e, n)
    layers: dict[str, list] = {}

    cur.execute(
        "SELECT osm_id, name, height_m, height_from_osm, ST_AsGeoJSON(geom) "
        "FROM buildings WHERE geom && ST_MakeEnvelope(%s, %s, %s, %s, 4326)",
        env,
    )
    layers["buildings"] = [
        (oid, {"id": oid, "name": name, "height_m": round(h, 1), "height_from_osm": src}, json.loads(g))
        for oid, name, h, src, g in cur.fetchall()
    ]
    cur.execute(
        "SELECT osm_id, name, height_m, height_from_osm, leaf_type, ST_AsGeoJSON(geom) "
        "FROM woods WHERE geom && ST_MakeEnvelope(%s, %s, %s, %s, 4326)",
        env,
    )
    layers["woods"] = [
        (
            oid,
            {
                "id": oid,
                "name": name,
                "height_m": round(h, 1),
                "height_from_osm": src,
                "leaf_type": leaf or "broadleaved",
            },
            json.loads(g),
        )
        for oid, name, h, src, leaf, g in cur.fetchall()
    ]
    cur.execute(
        "SELECT osm_id, height_m, crown_radius_m, leaf_type, ST_AsGeoJSON(geom) "
        "FROM trees WHERE geom && ST_MakeEnvelope(%s, %s, %s, %s, 4326)",
        env,
    )
    layers["trees"] = [
        (
            oid,
            {
                "id": oid,
                "height_m": round(h, 1),
                "crown_radius_m": round(r, 1),
                "leaf_type": leaf or "broadleaved",
            },
            json.loads(g),
        )
        for oid, h, r, leaf, g in cur.fetchall()
    ]
    return layers


# ------------------------------------------------------------------ encodage


def geojson_to_shapely_3857(geom: dict):
    """GeoJSON EPSG:4326 → géométrie shapely en EPSG:3857."""
    from shapely.geometry import MultiPolygon, Point, Polygon

    def ring_3857(ring):
        return [mercator(lat, lon) for lon, lat in ring]

    if geom["type"] == "Point":
        lon, lat = geom["coordinates"]
        return Point(mercator(lat, lon))
    if geom["type"] == "Polygon":
        rings = geom["coordinates"]
        return Polygon(ring_3857(rings[0]), [ring_3857(r) for r in rings[1:]])
    if geom["type"] == "MultiPolygon":
        return MultiPolygon(
            [
                (ring_3857(rings[0]), [ring_3857(r) for r in rings[1:]])
                for rings in geom["coordinates"]
            ]
        )
    raise ValueError(f"géométrie inattendue : {geom['type']}")


def encode_tile(z: int, x: int, y: int, layers: dict[str, list]) -> bytes | None:
    """Tuile MVT gzip, ou None si aucune couche n'a d'objet."""
    import mapbox_vector_tile

    payload = []
    for name, features in layers.items():
        if not features:
            continue
        payload.append(
            {
                "name": name,
                "features": [
                    {"geometry": geojson_to_shapely_3857(g), "properties": props}
                    for _, props, g in features
                ],
            }
        )
    if not payload:
        return None
    raw = mapbox_vector_tile.encode(
        payload,
        default_options={
            # Bornes 3857 de la tuile : la quantification vers l'extent 4096
            # et le retournement de l'axe y (spec MVT, y vers le bas) sont
            # faits par la bibliothèque.
            "quantize_bounds": tile_bounds_3857(z, x, y),
            "extents": EXTENT,
        },
    )
    # Compression par tuile, déclarée dans l'en-tête PMTiles : c'est ce que
    # les lecteurs MVT attendent, et le vectoriel se compresse très bien.
    return gzip.compress(raw)


def render_tile(zxy: tuple[int, int, int]):
    z, x, y = zxy
    minx, miny, maxx, maxy = tile_bounds_3857(z, x, y)

    def inv_lat(y_m: float) -> float:
        return math.degrees(math.atan(math.sinh(y_m / R_MERC)))

    def inv_lon(x_m: float) -> float:
        return math.degrees(x_m / R_MERC)

    bbox = (inv_lon(minx), inv_lat(miny), inv_lon(maxx), inv_lat(maxy))
    data = encode_tile(z, x, y, fetch_features(bbox))
    if data is None:
        return None
    from pmtiles.tile import zxy_to_tileid

    return zxy_to_tileid(z, x, y), data


# -------------------------------------------------------------------- PMTiles


def db_extent(dsn: str) -> tuple[float, float, float, float]:
    """Emprise (s, w, n, e) de toutes les tables géométriques."""
    import psycopg

    s, w, n, e = math.inf, math.inf, -math.inf, -math.inf
    with psycopg.connect(dsn) as conn:
        for table in ("buildings", "woods", "trees"):
            row = conn.execute(
                f"SELECT ST_XMin(x), ST_YMin(x), ST_XMax(x), ST_YMax(x) "
                f"FROM (SELECT ST_Extent(geom) AS x FROM {table}) AS sub"
            ).fetchone()
            if row and row[0] is not None:
                w, s = min(w, row[0]), min(s, row[1])
                e, n = max(e, row[2]), max(n, row[3])
    if not math.isfinite(s):
        raise SystemExit("tables géométriques vides — lancer bin/import d'abord")
    return s, w, n, e


def build(dsn: str, out_path: str, bbox, jobs: int):
    from pmtiles.tile import Compression, TileType, zxy_to_tileid
    from pmtiles.writer import Writer

    s, w, n, e = bbox
    x0, y0, x1, y1 = tile_range(s, w, n, e)
    tiles = sorted(
        (zxy_to_tileid(ZOOM, x, y), (ZOOM, x, y))
        for x in range(x0, x1 + 1)
        for y in range(y0, y1 + 1)
    )
    print(f"{len(tiles)} tuiles candidates (z{ZOOM})")

    written = 0
    with open(out_path, "wb") as f:
        writer = Writer(f)
        with Pool(jobs, initializer=worker_init, initargs=(dsn,)) as pool:
            for i, result in enumerate(
                pool.imap(render_tile, [zxy for _, zxy in tiles], chunksize=4)
            ):
                if result is not None:
                    writer.write_tile(*result)
                    written += 1
                if (i + 1) % 200 == 0:
                    print(f"{i + 1}/{len(tiles)} rendues, {written} non vides")

        if written == 0:
            raise SystemExit("aucune tuile non vide dans l'emprise")
        writer.finalize(
            {
                "tile_type": TileType.MVT,
                "tile_compression": Compression.GZIP,
                "min_zoom": ZOOM,
                "max_zoom": ZOOM,
                "min_lon_e7": int(w * 1e7),
                "min_lat_e7": int(s * 1e7),
                "max_lon_e7": int(e * 1e7),
                "max_lat_e7": int(n * 1e7),
                "center_zoom": ZOOM,
                "center_lon_e7": int((w + e) / 2 * 1e7),
                "center_lat_e7": int((s + n) / 2 * 1e7),
            },
            {
                "name": "sunmap",
                "attribution": "© OpenStreetMap contributors",
                "description": "Géométrie SunMap : buildings, woods, trees — sans "
                "simplification, objets entiers dupliqués par tuile (dédoublonner par id)",
                "vector_layers": [
                    {"id": "buildings", "fields": {"id": "String", "name": "String", "height_m": "Number", "height_from_osm": "Boolean"}},
                    {"id": "woods", "fields": {"id": "String", "name": "String", "height_m": "Number", "height_from_osm": "Boolean", "leaf_type": "String"}},
                    {"id": "trees", "fields": {"id": "String", "height_m": "Number", "crown_radius_m": "Number", "leaf_type": "String"}},
                ],
            },
        )
    size_mb = os.path.getsize(out_path) / 1e6
    print(f"{written} tuiles écrites → {out_path} ({size_mb:.1f} Mo)")


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


# ------------------------------------------------------- selftest et fixture

# Objets synthétiques déterministes, partagés entre le selftest Python et la
# fixture des tests Rust (`helios-server/testdata/mini.pmtiles`) : les deux
# décodeurs doivent lire exactement ces valeurs.
FIXTURE_TILE = (14, 8298, 5636)  # tuile z14 contenant Notre-Dame


def fixture_layers() -> dict[str, list]:
    s, w = 48.8525, 2.3480
    n, e = 48.8535, 2.3500
    square = {
        "type": "Polygon",
        "coordinates": [[[w, s], [e, s], [e, n], [w, n], [w, s]]],
    }
    hole = {
        "type": "Polygon",
        "coordinates": [
            [[2.3400, 48.8500], [2.3450, 48.8500], [2.3450, 48.8540], [2.3400, 48.8540], [2.3400, 48.8500]],
            [[2.3410, 48.8510], [2.3440, 48.8510], [2.3440, 48.8530], [2.3410, 48.8530], [2.3410, 48.8510]],
        ],
    }
    return {
        "buildings": [
            ("way/1", {"id": "way/1", "name": "Tour Test", "height_m": 42.5, "height_from_osm": True}, square),
        ],
        "woods": [
            ("relation/2", {"id": "relation/2", "name": None, "height_m": 18.0, "height_from_osm": False, "leaf_type": "needleleaved"}, hole),
        ],
        "trees": [
            ("node/3", {"id": "node/3", "height_m": 10.0, "crown_radius_m": 3.0, "leaf_type": "palm"},
             {"type": "Point", "coordinates": [2.3499, 48.8530]}),
        ],
    }


def write_fixture(out_path: str):
    """Archive minuscule et déterministe pour les tests Rust (`vtiles.rs`)."""
    from pmtiles.tile import Compression, TileType, zxy_to_tileid
    from pmtiles.writer import Writer

    z, x, y = FIXTURE_TILE
    data = encode_tile(z, x, y, fixture_layers())
    assert data is not None
    with open(out_path, "wb") as f:
        writer = Writer(f)
        writer.write_tile(zxy_to_tileid(z, x, y), data)
        writer.finalize(
            {
                "tile_type": TileType.MVT,
                "tile_compression": Compression.GZIP,
                "min_zoom": z,
                "max_zoom": z,
            },
            {"name": "sunmap-fixture"},
        )
    print(f"fixture → {out_path} ({os.path.getsize(out_path)} octets)")


def selftest():
    """Aller-retour d'encodage MVT — sans base de données."""
    import mapbox_vector_tile

    z, x, y = FIXTURE_TILE
    data = encode_tile(z, x, y, fixture_layers())
    assert data is not None
    decoded = mapbox_vector_tile.decode(gzip.decompress(data))

    assert set(decoded) == {"buildings", "woods", "trees"}, sorted(decoded)
    b = decoded["buildings"]["features"][0]
    assert b["properties"]["id"] == "way/1"
    assert b["properties"]["height_m"] == 42.5
    assert b["properties"]["height_from_osm"] is True

    wd = decoded["woods"]["features"][0]
    assert wd["properties"]["leaf_type"] == "needleleaved"
    assert len(wd["geometry"]["coordinates"]) == 2, "extérieur + trou attendus"

    t = decoded["trees"]["features"][0]
    assert t["properties"]["crown_radius_m"] == 3.0
    # Position du point : dé-quantifiée à mieux que le pas d'extent (~0,6 m,
    # soit ~1e-5 degré). decode() renvoie des coordonnées en unités de tuile
    # (y remonté) : on les reprojette pour comparer.
    gx, gy = t["geometry"]["coordinates"]
    minx, miny, maxx, maxy = tile_bounds_3857(z, x, y)
    lon = math.degrees((minx + gx / EXTENT * (maxx - minx)) / R_MERC)
    lat = math.degrees(math.atan(math.sinh((miny + gy / EXTENT * (maxy - miny)) / R_MERC)))
    assert abs(lon - 2.3499) < 2e-5 and abs(lat - 48.8530) < 2e-5, (lon, lat)

    print("selftest OK — encodage MVT conforme (aller-retour vérifié)")


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
    p.add_argument("--jobs", type=int, default=os.cpu_count() or 4)
    p.add_argument("--upload", action="store_true", help="pousse sur R2 après génération")
    p.add_argument("--selftest", action="store_true", help="tests sans base de données")
    p.add_argument(
        "--fixture",
        metavar="CHEMIN",
        help="écrit l'archive de test des tests Rust (sans base de données)",
    )
    args = p.parse_args()

    if args.selftest:
        selftest()
        return
    if args.fixture:
        write_fixture(args.fixture)
        return

    dsn = args.database_url or read_env_database_url()
    if args.bbox:
        s, w, n, e = (float(v) for v in args.bbox.split(","))
    else:
        s, w, n, e = db_extent(dsn)
    print(f"emprise : {s:.4f},{w:.4f} → {n:.4f},{e:.4f}")

    os.makedirs(args.out_dir, exist_ok=True)
    out_path = os.path.join(args.out_dir, "sunmap.pmtiles")
    build(dsn, out_path, (s, w, n, e), args.jobs)

    if args.upload:
        upload_r2([out_path])


if __name__ == "__main__":
    main()
