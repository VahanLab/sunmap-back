//! Accès PostGIS : lecture par bounding box (chemin de requête) et upsert
//! (chemin d'ingestion).
//!
//! Les géométries transitent en WKT plutôt que via un crate `geo` : le moteur
//! d'ombre ne manipule que des anneaux `(lat, lon)`, et `ST_GeomFromText` /
//! `ST_AsText` suffisent dans les deux sens. Une dépendance de plus pour
//! parser du WKB n'apporterait rien ici.

use sqlx::postgres::{PgPool, PgPoolOptions};
use sqlx::Row;

use crate::osm::{Building, Poi, Tree};

/// URL par défaut en dev local. Surchargeable par `DATABASE_URL`.
pub const DEFAULT_URL: &str = "postgres://localhost/sunmap";

pub async fn connect() -> Result<PgPool, sqlx::Error> {
    let url = std::env::var("DATABASE_URL").unwrap_or_else(|_| DEFAULT_URL.to_string());
    PgPoolOptions::new().max_connections(8).connect(&url).await
}

/// Enveloppe PostGIS d'une bbox géographique.
fn envelope(s: f64, w: f64, n: f64, e: f64) -> String {
    format!("ST_MakeEnvelope({w}, {s}, {e}, {n}, 4326)")
}

// ------------------------------------------------------------- lecture

/// Bâtiments dont l'emprise intersecte la bbox.
///
/// `&&` (intersection des boîtes englobantes) plutôt que `ST_Intersects` :
/// l'index GIST le sert directement, et un bâtiment dont seule la bbox touche
/// la zone reste un caster valide — on ne cherche pas une réponse exacte, on
/// remplit une DSM.
pub async fn buildings_in_bbox(
    pool: &PgPool,
    s: f64,
    w: f64,
    n: f64,
    e: f64,
) -> Result<Vec<Building>, sqlx::Error> {
    let sql = format!(
        "SELECT osm_id, name, height_m, height_from_osm, ST_AsText(geom) AS wkt \
         FROM buildings WHERE geom && {}",
        envelope(s, w, n, e)
    );
    let rows = sqlx::query(&sql).fetch_all(pool).await?;

    Ok(rows
        .into_iter()
        .filter_map(|r| {
            let wkt: String = r.get("wkt");
            let rings = parse_multipolygon_wkt(&wkt);
            if rings.is_empty() {
                return None;
            }
            Some(Building {
                osm_id: r.get("osm_id"),
                name: r.get("name"),
                rings,
                height_m: r.get("height_m"),
                height_from_osm: r.get("height_from_osm"),
            })
        })
        .collect())
}

pub async fn trees_in_bbox(
    pool: &PgPool,
    s: f64,
    w: f64,
    n: f64,
    e: f64,
) -> Result<Vec<Tree>, sqlx::Error> {
    let sql = format!(
        "SELECT osm_id, height_m, crown_radius_m, ST_Y(geom) AS lat, ST_X(geom) AS lng \
         FROM trees WHERE geom && {}",
        envelope(s, w, n, e)
    );
    Ok(sqlx::query(&sql)
        .fetch_all(pool)
        .await?
        .into_iter()
        .map(|r| Tree {
            osm_id: r.get("osm_id"),
            lat: r.get("lat"),
            lng: r.get("lng"),
            height_m: r.get("height_m"),
            crown_radius_m: r.get("crown_radius_m"),
        })
        .collect())
}

pub async fn terraces_in_bbox(
    pool: &PgPool,
    s: f64,
    w: f64,
    n: f64,
    e: f64,
) -> Result<Vec<Poi>, sqlx::Error> {
    let sql = format!(
        "SELECT osm_id, name, amenity, outdoor_seating, website, phone, opening_hours, \
                cuisine, wikidata, \
                ST_Y(geom) AS lat, ST_X(geom) AS lng \
         FROM terraces WHERE geom && {}",
        envelope(s, w, n, e)
    );
    Ok(sqlx::query(&sql)
        .fetch_all(pool)
        .await?
        .into_iter()
        .map(|r| Poi {
            osm_id: r.get("osm_id"),
            name: r.get("name"),
            amenity: r.get("amenity"),
            outdoor_seating: r.get("outdoor_seating"),
            lat: r.get("lat"),
            lng: r.get("lng"),
            website: r.get("website"),
            phone: r.get("phone"),
            opening_hours: r.get("opening_hours"),
            cuisine: r.get("cuisine"),
            wikidata: r.get("wikidata"),
        })
        .collect())
}

// ----------------------------------------------------------- ingestion

/// Upsert : une tuile d'ingestion recouvre ses voisines, et un même bâtiment
/// revient donc plusieurs fois. `ON CONFLICT DO UPDATE` garde la dernière
/// version plutôt que d'échouer.
/// ST_CollectionExtract(..., 3) avant ST_Multi : une emprise OSM
/// auto-intersectée (fréquent) fait sortir `ST_MakeValid` en
/// GeometryCollection — les polygones plus les lignes de découpe — que
/// `ST_Multi` ne convertit pas. L'extraction ne garde que les polygones.
const INSERT_BUILDING: &str =
    "INSERT INTO buildings (osm_id, name, height_m, height_from_osm, geom) \
     VALUES ($1, $2, $3, $4, \
       ST_Multi(ST_CollectionExtract(ST_MakeValid(ST_GeomFromText($5, 4326)), 3))) \
     ON CONFLICT (osm_id) DO UPDATE SET \
       name = EXCLUDED.name, height_m = EXCLUDED.height_m, \
       height_from_osm = EXCLUDED.height_from_osm, geom = EXCLUDED.geom";

pub async fn upsert_buildings(pool: &PgPool, buildings: &[Building]) -> Result<u64, sqlx::Error> {
    let mut written = 0u64;
    // Par paquets : une transaction unique sur 200 000 emprises ferait gonfler
    // le WAL sans bénéfice, et un échec perdrait toute la tuile.
    for chunk in buildings.chunks(500) {
        match insert_chunk(pool, chunk).await {
            Ok(n) => written += n,
            // Une géométrie irrécupérable empoisonne la transaction et
            // ferait perdre les 499 autres. On rejoue le paquet ligne à
            // ligne, hors transaction, en sautant les fautives : une emprise
            // aberrante ne doit pas coûter une tuile entière.
            Err(_) => written += insert_one_by_one(pool, chunk).await,
        }
    }
    Ok(written)
}

async fn insert_chunk(pool: &PgPool, chunk: &[Building]) -> Result<u64, sqlx::Error> {
    let mut written = 0u64;
    let mut tx = pool.begin().await?;
    for b in chunk {
        let Some(wkt) = multipolygon_wkt(&b.rings) else { continue };
        written += sqlx::query(INSERT_BUILDING)
            .bind(&b.osm_id)
            .bind(&b.name)
            .bind(b.height_m)
            .bind(b.height_from_osm)
            .bind(&wkt)
            .execute(&mut *tx)
            .await?
            .rows_affected();
    }
    tx.commit().await?;
    Ok(written)
}

async fn insert_one_by_one(pool: &PgPool, chunk: &[Building]) -> u64 {
    let mut written = 0u64;
    for b in chunk {
        let Some(wkt) = multipolygon_wkt(&b.rings) else { continue };
        match sqlx::query(INSERT_BUILDING)
            .bind(&b.osm_id)
            .bind(&b.name)
            .bind(b.height_m)
            .bind(b.height_from_osm)
            .bind(&wkt)
            .execute(pool)
            .await
        {
            Ok(r) => written += r.rows_affected(),
            Err(e) => eprintln!("  emprise ignorée {} : {e}", b.osm_id),
        }
    }
    written
}

pub async fn upsert_trees(pool: &PgPool, trees: &[Tree]) -> Result<u64, sqlx::Error> {
    let mut written = 0u64;
    for chunk in trees.chunks(1000) {
        let mut tx = pool.begin().await?;
        for t in chunk {
            written += sqlx::query(
                "INSERT INTO trees (osm_id, height_m, crown_radius_m, geom) \
                 VALUES ($1, $2, $3, ST_SetSRID(ST_MakePoint($4, $5), 4326)) \
                 ON CONFLICT (osm_id) DO UPDATE SET \
                   height_m = EXCLUDED.height_m, crown_radius_m = EXCLUDED.crown_radius_m, \
                   geom = EXCLUDED.geom",
            )
            .bind(&t.osm_id)
            .bind(t.height_m)
            .bind(t.crown_radius_m)
            .bind(t.lng)
            .bind(t.lat)
            .execute(&mut *tx)
            .await?
            .rows_affected();
        }
        tx.commit().await?;
    }
    Ok(written)
}

pub async fn upsert_terraces(pool: &PgPool, pois: &[Poi]) -> Result<u64, sqlx::Error> {
    let mut written = 0u64;
    for chunk in pois.chunks(1000) {
        let mut tx = pool.begin().await?;
        for p in chunk {
            written += sqlx::query(
                "INSERT INTO terraces (osm_id, name, amenity, outdoor_seating, website, phone, \
                                       opening_hours, cuisine, wikidata, geom) \
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, \
                         ST_SetSRID(ST_MakePoint($10, $11), 4326)) \
                 ON CONFLICT (osm_id) DO UPDATE SET \
                   name = EXCLUDED.name, amenity = EXCLUDED.amenity, \
                   outdoor_seating = EXCLUDED.outdoor_seating, website = EXCLUDED.website, \
                   phone = EXCLUDED.phone, opening_hours = EXCLUDED.opening_hours, \
                   cuisine = EXCLUDED.cuisine, wikidata = EXCLUDED.wikidata, geom = EXCLUDED.geom",
            )
            .bind(&p.osm_id)
            .bind(&p.name)
            .bind(&p.amenity)
            .bind(p.outdoor_seating)
            .bind(&p.website)
            .bind(&p.phone)
            .bind(&p.opening_hours)
            .bind(&p.cuisine)
            .bind(&p.wikidata)
            .bind(p.lng)
            .bind(p.lat)
            .execute(&mut *tx)
            .await?
            .rows_affected();
        }
        tx.commit().await?;
    }
    Ok(written)
}

/// Tuiles déjà absorbées pour cette couche — permet de reprendre une
/// ingestion interrompue sans refaire les dizaines de requêtes Overpass déjà
/// payées.
pub async fn done_chunks(pool: &PgPool, layer: &str) -> Result<Vec<String>, sqlx::Error> {
    Ok(
        sqlx::query("SELECT chunk_key FROM ingest_log WHERE layer = $1")
            .bind(layer)
            .fetch_all(pool)
            .await?
            .into_iter()
            .map(|r| r.get::<String, _>("chunk_key"))
            .collect(),
    )
}

pub async fn mark_chunk(
    pool: &PgPool,
    layer: &str,
    chunk_key: &str,
    feature_count: i32,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO ingest_log (layer, chunk_key, feature_count) VALUES ($1, $2, $3) \
         ON CONFLICT (layer, chunk_key) DO UPDATE SET \
           feature_count = EXCLUDED.feature_count, ingested_at = now()",
    )
    .bind(layer)
    .bind(chunk_key)
    .bind(feature_count)
    .execute(pool)
    .await?;
    Ok(())
}

// ------------------------------------------------------------------ WKT

/// Anneaux `(lat, lon)` → `MULTIPOLYGON` WKT (qui est en `lon lat`).
///
/// Tous les anneaux d'un bâtiment vont dans UN polygone : le premier est
/// l'extérieur, les suivants sont des trous. C'est ce qui préserve les cours
/// intérieures jusque dans la base. Les anneaux sont refermés si OSM ne l'a
/// pas fait, sinon PostGIS rejette la géométrie.
fn multipolygon_wkt(rings: &[Vec<(f64, f64)>]) -> Option<String> {
    let parts: Vec<String> = rings
        .iter()
        .filter(|r| r.len() >= 3)
        .map(|r| {
            let mut pts: Vec<(f64, f64)> = r.clone();
            if pts.first() != pts.last() {
                pts.push(pts[0]);
            }
            let coords: Vec<String> = pts
                .iter()
                .map(|(lat, lon)| format!("{lon} {lat}"))
                .collect();
            format!("({})", coords.join(","))
        })
        .collect();
    if parts.is_empty() {
        return None;
    }
    // Un seul polygone dans le multipolygone : `(((ext),(trou)))`.
    Some(format!("MULTIPOLYGON(({}))", parts.join(",")))
}

/// `MULTIPOLYGON(((lon lat, …),(…)),((…)))` → anneaux `(lat, lon)`.
///
/// Parseur minimal, suffisant parce qu'on ne lit que ce qu'on a écrit :
/// tous les groupes de coordonnées sont extraits à plat, un anneau par
/// parenthèse la plus interne. La distinction extérieur/trou est inutile côté
/// moteur — la rasterisation pair-impair traite les anneaux indifféremment.
fn parse_multipolygon_wkt(wkt: &str) -> Vec<Vec<(f64, f64)>> {
    let mut rings = Vec::new();
    let mut current = String::new();
    let mut depth = 0usize;

    for c in wkt.chars() {
        match c {
            '(' => {
                depth += 1;
                current.clear();
            }
            ')' => {
                if !current.trim().is_empty() {
                    let ring: Vec<(f64, f64)> = current
                        .split(',')
                        .filter_map(|p| {
                            let mut it = p.split_whitespace();
                            let lon: f64 = it.next()?.parse().ok()?;
                            let lat: f64 = it.next()?.parse().ok()?;
                            Some((lat, lon))
                        })
                        .collect();
                    if ring.len() >= 3 {
                        rings.push(ring);
                    }
                }
                current.clear();
                depth = depth.saturating_sub(1);
            }
            _ => {
                if depth > 0 {
                    current.push(c);
                }
            }
        }
    }
    rings
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Un carré avec un trou doit survivre à l'aller-retour WKT, trou compris —
    /// c'est ce qui garde les cours intérieures creuses.
    #[test]
    fn wkt_roundtrip_keeps_hole() {
        let outer = vec![(48.0, 2.0), (48.0, 2.1), (48.1, 2.1), (48.1, 2.0)];
        let hole = vec![(48.04, 2.04), (48.04, 2.06), (48.06, 2.06), (48.06, 2.04)];
        let wkt = multipolygon_wkt(&[outer.clone(), hole.clone()]).unwrap();
        assert!(wkt.starts_with("MULTIPOLYGON(("));

        let back = parse_multipolygon_wkt(&wkt);
        assert_eq!(back.len(), 2, "extérieur + trou attendus");
        // Les anneaux sont refermés à l'écriture : un point de plus.
        assert_eq!(back[0].len(), outer.len() + 1);
        assert!((back[0][0].0 - 48.0).abs() < 1e-9);
        assert!((back[0][0].1 - 2.0).abs() < 1e-9);
        assert_eq!(back[1].len(), hole.len() + 1);
    }

    #[test]
    fn wkt_rejects_degenerate_ring() {
        assert!(multipolygon_wkt(&[vec![(48.0, 2.0), (48.1, 2.1)]]).is_none());
    }
}
