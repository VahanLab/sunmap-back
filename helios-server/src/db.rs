//! Accès PostGIS : lecture par bounding box (chemin de requête) et upsert
//! (chemin d'ingestion).
//!
//! Les géométries transitent en WKT plutôt que via un crate `geo` : le moteur
//! d'ombre ne manipule que des anneaux `(lat, lon)`, et `ST_GeomFromText` /
//! `ST_AsText` suffisent dans les deux sens. Une dépendance de plus pour
//! parser du WKB n'apporterait rien ici.

use std::collections::{HashMap, HashSet};

use sqlx::postgres::{PgPool, PgPoolOptions};
use sqlx::Row;

use crate::osm::{Building, Place, Tree, Wood};

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

pub async fn places_in_bbox(
    pool: &PgPool,
    s: f64,
    w: f64,
    n: f64,
    e: f64,
) -> Result<Vec<Place>, sqlx::Error> {
    let sql = format!(
        "SELECT osm_id, name, amenity, outdoor_seating, website, phone, opening_hours, \
                cuisine, wikidata, direction_deg, covered, backrest, seats, material, \
                ST_Y(geom) AS lat, ST_X(geom) AS lng \
         FROM places WHERE geom && {}",
        envelope(s, w, n, e)
    );
    Ok(sqlx::query(&sql)
        .fetch_all(pool)
        .await?
        .into_iter()
        .map(|r| Place {
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
            // `real` PostgreSQL = f32 ; élargi en f64 côté domaine.
            direction_deg: r.get::<Option<f32>, _>("direction_deg").map(f64::from),
            covered: r.get("covered"),
            backrest: r.get("backrest"),
            seats: r.get("seats"),
            material: r.get("material"),
        })
        .collect())
}

// ------------------------------------------- contributions utilisateur

/// Terrasse signalée par un utilisateur pour un établissement.
#[derive(Clone, Debug)]
pub struct TerraceReport {
    pub osm_id: String,
    pub has_terrace: bool,
    /// Position précise de la terrasse, si elle a été située.
    pub lat: Option<f64>,
    pub lng: Option<f64>,
    /// Auteur, quand il est connu. Absent des contributions antérieures à
    /// l'authentification.
    pub author_uid: Option<String>,
    /// Pseudo de l'auteur, joint à la lecture pour que la fiche d'un
    /// établissement puisse citer qui a signalé sa terrasse sans requête de plus.
    pub author_username: Option<String>,
}

/// Compte, tel qu'on le connaît.
#[derive(Clone, Debug)]
pub struct UserRecord {
    pub uid: String,
    pub username: String,
}

/// Le pseudo demandé est déjà pris.
#[derive(Debug)]
pub struct UsernameTaken;

/// Une contribution, vue depuis le profil de son auteur.
///
/// Porte l'établissement (nom, catégorie, position) et non la seule référence :
/// une liste de `node/123456` ne dirait rien à personne, et le client n'a pas de
/// quoi les résoudre — il ne connaît que les établissements de la zone qu'il
/// regarde, or les contributions sont éparpillées.
#[derive(Clone, Debug)]
pub struct ContributionRecord {
    pub osm_id: String,
    pub name: Option<String>,
    pub amenity: Option<String>,
    pub has_terrace: bool,
    pub lat: f64,
    pub lng: f64,
    pub updated_at: chrono::DateTime<chrono::Utc>,
}

/// Contributions couvrant la bbox, indexées par identifiant OSM.
///
/// Filtre sur la géométrie des `places` et non sur celle de la contribution :
/// une terrasse signalée sans position n'a pas de géométrie, et serait donc
/// invisible d'une recherche spatiale.
pub async fn terrace_reports_in_bbox(
    pool: &PgPool,
    s: f64,
    w: f64,
    n: f64,
    e: f64,
) -> Result<HashMap<String, TerraceReport>, sqlx::Error> {
    let sql = format!(
        "SELECT t.osm_id, t.has_terrace, ST_Y(t.geom) AS lat, ST_X(t.geom) AS lng, \
                t.user_uid, u.username \
         FROM place_terraces t \
         JOIN places p USING (osm_id) \
         LEFT JOIN users u ON u.uid = t.user_uid \
         WHERE p.geom && {}",
        envelope(s, w, n, e)
    );
    Ok(sqlx::query(&sql)
        .fetch_all(pool)
        .await?
        .into_iter()
        .map(|r| {
            let osm_id: String = r.get("osm_id");
            (
                osm_id.clone(),
                TerraceReport {
                    osm_id,
                    has_terrace: r.get("has_terrace"),
                    lat: r.get("lat"),
                    lng: r.get("lng"),
                    author_uid: r.get("user_uid"),
                    author_username: r.get("username"),
                },
            )
        })
        .collect())
}

/// Enregistre (ou remplace) la contribution pour un établissement.
pub async fn upsert_terrace_report(
    pool: &PgPool,
    report: &TerraceReport,
) -> Result<(), sqlx::Error> {
    // Position construite seulement si les deux coordonnées sont là, sinon
    // NULL : une terrasse peut être signalée sans être située.
    sqlx::query(
        "INSERT INTO place_terraces (osm_id, has_terrace, geom, updated_at, user_uid) \
         VALUES ($1, $2, \
                 CASE WHEN $3::float8 IS NULL OR $4::float8 IS NULL THEN NULL \
                      ELSE ST_SetSRID(ST_MakePoint($3, $4), 4326) END, \
                 now(), $5) \
         ON CONFLICT (osm_id) DO UPDATE SET \
           has_terrace = EXCLUDED.has_terrace, geom = EXCLUDED.geom, \
           updated_at = EXCLUDED.updated_at, user_uid = EXCLUDED.user_uid",
    )
    .bind(&report.osm_id)
    .bind(report.has_terrace)
    .bind(report.lng)
    .bind(report.lat)
    .bind(&report.author_uid)
    .execute(pool)
    .await?;
    Ok(())
}

/// L'établissement existe-t-il ? Garde-fou avant d'accepter une contribution,
/// pour ne pas accumuler des lignes orphelines sur des identifiants inventés.
pub async fn place_exists(pool: &PgPool, osm_id: &str) -> Result<bool, sqlx::Error> {
    sqlx::query_scalar::<_, i64>("SELECT count(*) FROM places WHERE osm_id = $1")
        .bind(osm_id)
        .fetch_one(pool)
        .await
        .map(|n| n > 0)
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

/// Emprises boisées couvrant la bbox. Même lecture que les bâtiments.
pub async fn woods_in_bbox(
    pool: &PgPool,
    s: f64,
    w: f64,
    n: f64,
    e: f64,
) -> Result<Vec<Wood>, sqlx::Error> {
    let sql = format!(
        "SELECT osm_id, name, height_m, height_from_osm, ST_AsText(geom) AS wkt \
         FROM woods WHERE geom && {}",
        envelope(s, w, n, e)
    );
    Ok(sqlx::query(&sql)
        .fetch_all(pool)
        .await?
        .into_iter()
        .filter_map(|r| {
            let wkt: String = r.get("wkt");
            Some(Wood {
                osm_id: r.get("osm_id"),
                name: r.get("name"),
                height_m: r.get("height_m"),
                height_from_osm: r.get("height_from_osm"),
                rings: Some(parse_multipolygon_wkt(&wkt)).filter(|r| !r.is_empty())?,
            })
        })
        .collect())
}

const INSERT_WOOD: &str =
    "INSERT INTO woods (osm_id, name, height_m, height_from_osm, geom) \
     VALUES ($1, $2, $3, $4, \
       ST_Multi(ST_CollectionExtract(ST_MakeValid(ST_GeomFromText($5, 4326)), 3))) \
     ON CONFLICT (osm_id) DO UPDATE SET \
       name = EXCLUDED.name, height_m = EXCLUDED.height_m, \
       height_from_osm = EXCLUDED.height_from_osm, geom = EXCLUDED.geom";

/// Mêmes précautions que pour les bâtiments : par paquets, avec repli ligne à
/// ligne — une emprise forestière auto-intersectée ne doit pas coûter la tuile.
pub async fn upsert_woods(pool: &PgPool, woods: &[Wood]) -> Result<u64, sqlx::Error> {
    let mut written = 0u64;
    for chunk in woods.chunks(500) {
        let mut ok = true;
        let mut tx = pool.begin().await?;
        let mut n = 0u64;
        for w in chunk {
            let Some(wkt) = multipolygon_wkt(&w.rings) else { continue };
            match sqlx::query(INSERT_WOOD)
                .bind(&w.osm_id)
                .bind(&w.name)
                .bind(w.height_m)
                .bind(w.height_from_osm)
                .bind(&wkt)
                .execute(&mut *tx)
                .await
            {
                Ok(r) => n += r.rows_affected(),
                Err(_) => {
                    ok = false;
                    break;
                }
            }
        }
        if ok {
            tx.commit().await?;
            written += n;
        } else {
            tx.rollback().await.ok();
            for w in chunk {
                let Some(wkt) = multipolygon_wkt(&w.rings) else { continue };
                written += sqlx::query(INSERT_WOOD)
                    .bind(&w.osm_id)
                    .bind(&w.name)
                    .bind(w.height_m)
                    .bind(w.height_from_osm)
                    .bind(&wkt)
                    .execute(pool)
                    .await
                    .map(|r| r.rows_affected())
                    .unwrap_or(0);
            }
        }
    }
    Ok(written)
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

pub async fn upsert_places(pool: &PgPool, pois: &[Place]) -> Result<u64, sqlx::Error> {
    let mut written = 0u64;
    for chunk in pois.chunks(1000) {
        let mut tx = pool.begin().await?;
        for p in chunk {
            written += sqlx::query(
                "INSERT INTO places (osm_id, name, amenity, outdoor_seating, website, phone, \
                                       opening_hours, cuisine, wikidata, geom, \
                                       direction_deg, covered, backrest, seats, material) \
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, \
                         ST_SetSRID(ST_MakePoint($10, $11), 4326), \
                         $12, $13, $14, $15, $16) \
                 ON CONFLICT (osm_id) DO UPDATE SET \
                   name = EXCLUDED.name, amenity = EXCLUDED.amenity, \
                   outdoor_seating = EXCLUDED.outdoor_seating, website = EXCLUDED.website, \
                   phone = EXCLUDED.phone, opening_hours = EXCLUDED.opening_hours, \
                   cuisine = EXCLUDED.cuisine, wikidata = EXCLUDED.wikidata, geom = EXCLUDED.geom, \
                   direction_deg = EXCLUDED.direction_deg, covered = EXCLUDED.covered, \
                   backrest = EXCLUDED.backrest, seats = EXCLUDED.seats, \
                   material = EXCLUDED.material",
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
            .bind(p.direction_deg.map(|d| d as f32))
            .bind(p.covered)
            .bind(p.backrest)
            .bind(p.seats)
            .bind(&p.material)
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

// ------------------------------------------------------------------ comptes

pub async fn user_by_uid(pool: &PgPool, uid: &str) -> Result<Option<UserRecord>, sqlx::Error> {
    Ok(
        sqlx::query("SELECT uid, username FROM users WHERE uid = $1")
            .bind(uid)
            .fetch_optional(pool)
            .await?
            .map(|r| UserRecord {
                uid: r.get("uid"),
                username: r.get("username"),
            }),
    )
}

/// Compte désigné par son pseudo, sans tenir compte de la casse.
///
/// Recherche sur `username_key` et non sur `username` : c'est la colonne
/// générée en minuscules qui porte l'index unique, donc la seule qui garantisse
/// à la fois l'unicité du résultat et une lecture indexée. Ouvrir le profil de
/// « Karl » depuis une fiche doit marcher, quelle que soit la casse du lien.
pub async fn user_by_username(
    pool: &PgPool,
    username: &str,
) -> Result<Option<UserRecord>, sqlx::Error> {
    Ok(
        sqlx::query("SELECT uid, username FROM users WHERE username_key = $1")
            .bind(username.to_lowercase())
            .fetch_optional(pool)
            .await?
            .map(|r| UserRecord {
                uid: r.get("uid"),
                username: r.get("username"),
            }),
    )
}

/// Nombre de contributions d'un compte.
///
/// Compté à part de la liste plutôt que déduit de sa longueur : la liste est
/// tronquée pour l'affichage, et le total est justement ce qui décide du palier
/// — le déduire d'une liste plafonnée figerait tout le monde au plafond.
pub async fn contribution_count(pool: &PgPool, uid: &str) -> Result<i64, sqlx::Error> {
    sqlx::query_scalar::<_, i64>("SELECT count(*) FROM place_terraces WHERE user_uid = $1")
        .bind(uid)
        .fetch_one(pool)
        .await
}

/// Établissements auxquels un compte a contribué, du plus récent au plus ancien.
///
/// `JOIN places` et non `LEFT JOIN` : une contribution dont l'établissement a
/// disparu d'OSM depuis n'a plus rien à montrer — ni nom, ni catégorie, ni
/// position. Elle reste comptée dans le total, mais n'a pas sa place dans une
/// liste dont chaque ligne est censée être cliquable.
pub async fn contributions_by_user(
    pool: &PgPool,
    uid: &str,
    limit: i64,
) -> Result<Vec<ContributionRecord>, sqlx::Error> {
    Ok(sqlx::query(
        "SELECT t.osm_id, t.has_terrace, t.updated_at, \
                p.name, p.amenity, ST_Y(p.geom) AS lat, ST_X(p.geom) AS lng \
         FROM place_terraces t \
         JOIN places p USING (osm_id) \
         WHERE t.user_uid = $1 \
         ORDER BY t.updated_at DESC \
         LIMIT $2",
    )
    .bind(uid)
    .bind(limit)
    .fetch_all(pool)
    .await?
    .into_iter()
    .map(|r| ContributionRecord {
        osm_id: r.get("osm_id"),
        name: r.get("name"),
        amenity: r.get("amenity"),
        has_terrace: r.get("has_terrace"),
        lat: r.get("lat"),
        lng: r.get("lng"),
        updated_at: r.get("updated_at"),
    })
    .collect())
}

/// Pseudos déjà pris parmi ceux proposés, comparés sans tenir compte de la casse.
///
/// Une requête pour toute la liste plutôt qu'une par candidat : c'est la même
/// latence pour quatre suggestions que pour quarante.
pub async fn taken_usernames(
    pool: &PgPool,
    candidates: &[String],
) -> Result<HashSet<String>, sqlx::Error> {
    if candidates.is_empty() {
        return Ok(HashSet::new());
    }
    let lowered: Vec<String> = candidates.iter().map(|c| c.to_lowercase()).collect();
    Ok(
        sqlx::query("SELECT username_key FROM users WHERE username_key = ANY($1)")
            .bind(&lowered)
            .fetch_all(pool)
            .await?
            .into_iter()
            .map(|r| r.get::<String, _>("username_key"))
            .collect(),
    )
}

/// Attribue un pseudo à un compte, en créant le compte au besoin.
///
/// L'unicité n'est pas vérifiée avant d'écrire : entre la lecture et l'écriture,
/// quelqu'un d'autre peut prendre le même pseudo. C'est la contrainte de la base
/// qui tranche, et sa violation qu'on traduit ici — un `SELECT` préalable ne
/// ferait qu'élargir la fenêtre tout en donnant l'illusion d'être protégé.
pub async fn set_username(
    pool: &PgPool,
    uid: &str,
    username: &str,
) -> Result<Result<UserRecord, UsernameTaken>, sqlx::Error> {
    let result = sqlx::query(
        "INSERT INTO users (uid, username) VALUES ($1, $2) \
         ON CONFLICT (uid) DO UPDATE SET username = EXCLUDED.username, updated_at = now() \
         RETURNING uid, username",
    )
    .bind(uid)
    .bind(username)
    .fetch_one(pool)
    .await;

    match result {
        Ok(r) => Ok(Ok(UserRecord {
            uid: r.get("uid"),
            username: r.get("username"),
        })),
        // 23505 = violation d'unicité. Le seul index unique en jeu ici est celui
        // du pseudo, la clé primaire étant gérée par le `ON CONFLICT`.
        Err(sqlx::Error::Database(e)) if e.code().as_deref() == Some("23505") => {
            Ok(Err(UsernameTaken))
        }
        Err(e) => Err(e),
    }
}
