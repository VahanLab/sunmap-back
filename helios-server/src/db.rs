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

use crate::osm::Place;

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

/// Enregistre (ou remplace) la contribution pour un établissement, et
/// journalise le geste dans `place_terrace_contributions` — `place_terraces`
/// ne garde que la dernière valeur, l'historique sert à savoir qui a signalé
/// en premier et si quelqu'un d'autre a corrigé depuis.
pub async fn upsert_terrace_report(
    pool: &PgPool,
    report: &TerraceReport,
) -> Result<(), sqlx::Error> {
    let mut tx = pool.begin().await?;

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
    .execute(&mut *tx)
    .await?;

    sqlx::query(
        "INSERT INTO place_terrace_contributions \
           (place_id, user_uid, has_terrace, lat, lng) \
         VALUES ($1, $2, $3, $4, $5)",
    )
    .bind(&report.osm_id)
    .bind(&report.author_uid)
    .bind(report.has_terrace)
    .bind(report.lat)
    .bind(report.lng)
    .execute(&mut *tx)
    .await?;

    tx.commit().await?;
    Ok(())
}

/// Historique des signalements de terrasse d'un établissement, le plus
/// récent d'abord.
pub struct TerraceContributionRecord {
    pub username: Option<String>,
    pub has_terrace: bool,
    pub lat: Option<f64>,
    pub lng: Option<f64>,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

pub async fn terrace_contributions(
    pool: &PgPool,
    place_id: &str,
) -> Result<Vec<TerraceContributionRecord>, sqlx::Error> {
    sqlx::query(
        "SELECT u.username, c.has_terrace, c.lat, c.lng, c.created_at \
         FROM place_terrace_contributions c \
         LEFT JOIN users u ON u.uid = c.user_uid \
         WHERE c.place_id = $1 \
         ORDER BY c.created_at DESC",
    )
    .bind(place_id)
    .fetch_all(pool)
    .await
    .map(|rows| {
        rows.into_iter()
            .map(|r| TerraceContributionRecord {
                username: r.get("username"),
                has_terrace: r.get("has_terrace"),
                lat: r.get("lat"),
                lng: r.get("lng"),
                created_at: r.get("created_at"),
            })
            .collect()
    })
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

/// Ajoute un banc ou une table de pique-nique contribué depuis l'app.
///
/// `osm_id` synthétique généré côté base (`gen_random_uuid()`, disponible
/// nativement depuis PostgreSQL 13) plutôt qu'en Rust : éviter une dépendance
/// `uuid` pour un identifiant qui ne sert qu'à ne jamais collisionner avec un
/// vrai identifiant OSM.
///
/// Journalise aussi la toute première ligne d'historique (`applied = true`) :
/// sans elle, la liste des contributions d'un meuble tout juste posé serait
/// vide jusqu'à sa première correction.
pub async fn insert_user_furniture(
    pool: &PgPool,
    amenity: &str,
    lat: f64,
    lng: f64,
    direction_deg: Option<f64>,
    backrest: Option<bool>,
    contributor_uid: &str,
) -> Result<String, sqlx::Error> {
    let mut tx = pool.begin().await?;
    let osm_id: String = sqlx::query_scalar(
        "INSERT INTO places (osm_id, amenity, direction_deg, backrest, contributor_uid, geom) \
         VALUES ('user/' || gen_random_uuid(), $1, $2, $3, $4, \
                 ST_SetSRID(ST_MakePoint($5, $6), 4326)) \
         RETURNING osm_id",
    )
    .bind(amenity)
    .bind(direction_deg)
    .bind(backrest)
    .bind(contributor_uid)
    .bind(lng)
    .bind(lat)
    .fetch_one(&mut *tx)
    .await?;

    sqlx::query(
        "INSERT INTO place_furniture_contributions \
           (place_id, user_uid, lat, lng, direction_deg, backrest, applied) \
         VALUES ($1, $2, $3, $4, $5, $6, true)",
    )
    .bind(&osm_id)
    .bind(contributor_uid)
    .bind(lat)
    .bind(lng)
    .bind(direction_deg)
    .bind(backrest)
    .execute(&mut *tx)
    .await?;

    tx.commit().await?;
    Ok(osm_id)
}

/// Soumet une correction de position/orientation/dossier sur un meuble déjà en
/// base — posé depuis l'app ou importé d'OSM.
///
/// Toujours appliquée — dernier écrit gagne sur la ligne entière — et toujours
/// journalisée. Pas de verrou de propriété : un premier contributeur pose le
/// banc, un deuxième corrige l'orientation, un troisième précise l'absence de
/// dossier, et chacun voit son tour appliqué. Ça ne perd rien des deux
/// premiers tant que l'écran d'édition préremplit son formulaire depuis
/// l'état courant avant modification (`FurnitureContributionView`) : le
/// troisième renvoie donc l'orientation corrigée par le deuxième, même s'il
/// ne l'a pas lui-même touchée. `contributor_uid` ne sert plus qu'à afficher
/// qui a soumis en dernier, plus à décider qui a le droit d'écrire.
///
/// Contrepartie assumée : deux corrections envoyées en même temps depuis des
/// formulaires ouverts avant l'une et l'autre peuvent se courir après, la
/// seconde écrasant un champ que la première venait de changer. Risque jugé
/// faible pour du mobilier urbain, et déjà le même que celui de
/// `place_terraces`.
///
/// `WHERE amenity IN (...)` interdit d'appeler ceci sur autre chose qu'un
/// meuble — sans ce garde-fou, l'endpoint pourrait déplacer n'importe quel
/// établissement. Renvoie `None` si l'identifiant ne correspond à aucun
/// meuble.
pub async fn submit_furniture_contribution(
    pool: &PgPool,
    id: &str,
    lat: f64,
    lng: f64,
    direction_deg: Option<f64>,
    backrest: Option<bool>,
    author_uid: &str,
) -> Result<Option<String>, sqlx::Error> {
    let mut tx = pool.begin().await?;

    let amenity: Option<String> = sqlx::query_scalar(
        "UPDATE places SET \
           direction_deg = $2, backrest = $3, contributor_uid = $4, \
           geom = ST_SetSRID(ST_MakePoint($5, $6), 4326) \
         WHERE osm_id = $1 AND amenity IN ('bench', 'picnic_table') \
         RETURNING amenity",
    )
    .bind(id)
    .bind(direction_deg)
    .bind(backrest)
    .bind(author_uid)
    .bind(lng)
    .bind(lat)
    .fetch_optional(&mut *tx)
    .await?;
    let Some(amenity) = amenity else {
        return Ok(None);
    };

    sqlx::query(
        "INSERT INTO place_furniture_contributions \
           (place_id, user_uid, lat, lng, direction_deg, backrest, applied) \
         VALUES ($1, $2, $3, $4, $5, $6, true)",
    )
    .bind(id)
    .bind(author_uid)
    .bind(lat)
    .bind(lng)
    .bind(direction_deg)
    .bind(backrest)
    .execute(&mut *tx)
    .await?;

    tx.commit().await?;
    Ok(Some(amenity))
}

/// Historique des contributions d'un meuble, la plus récente d'abord —
/// pseudo et date de chacune, pour que l'app puisse toutes les montrer.
pub struct FurnitureContributionRecord {
    pub username: Option<String>,
    pub lat: f64,
    pub lng: f64,
    pub direction_deg: Option<f64>,
    pub backrest: Option<bool>,
    pub applied: bool,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

pub async fn furniture_contributions(
    pool: &PgPool,
    place_id: &str,
) -> Result<Vec<FurnitureContributionRecord>, sqlx::Error> {
    sqlx::query(
        "SELECT u.username, c.lat, c.lng, c.direction_deg, c.backrest, c.applied, c.created_at \
         FROM place_furniture_contributions c \
         LEFT JOIN users u ON u.uid = c.user_uid \
         WHERE c.place_id = $1 \
         ORDER BY c.created_at DESC",
    )
    .bind(place_id)
    .fetch_all(pool)
    .await
    .map(|rows| {
        rows.into_iter()
            .map(|r| FurnitureContributionRecord {
                username: r.get("username"),
                lat: r.get("lat"),
                lng: r.get("lng"),
                // Colonne `real` (FLOAT4) en base : décoder directement en
                // `f64` fait paniquer sqlx (type SQL/Rust non compatibles),
                // d'où le détour par `f32`.
                direction_deg: r.get::<Option<f32>, _>("direction_deg").map(f64::from),
                backrest: r.get("backrest"),
                applied: r.get("applied"),
                created_at: r.get("created_at"),
            })
            .collect()
    })
}

// ----------------------------------------------------------- ingestion
//
// Seuls les lieux s'écrivent encore ici : la géométrie (bâtiments, arbres,
// bois) va directement de l'extrait OSM à l'archive vectorielle
// (`bin/tilegen`), sans passer par la base.

pub async fn upsert_places(pool: &PgPool, pois: &[Place]) -> Result<u64, sqlx::Error> {
    // Un statement par LOT (UNNEST de tableaux), pas par ligne : chaque
    // statement paie un aller-retour réseau complet, et la base est distante
    // (managée OVH). En ligne à ligne, la France entière (523 k lieux) prenait
    // ~3 h depuis un poste (~20 ms de RTT par INSERT) ; en lots de 1 000,
    // ~500 aller-retours suffisent — quelques minutes.
    let mut written = 0u64;
    for chunk in pois.chunks(1000) {
        let mut osm_ids = Vec::with_capacity(chunk.len());
        let mut names = Vec::with_capacity(chunk.len());
        let mut amenities = Vec::with_capacity(chunk.len());
        let mut outdoor = Vec::with_capacity(chunk.len());
        let mut websites = Vec::with_capacity(chunk.len());
        let mut phones = Vec::with_capacity(chunk.len());
        let mut hours = Vec::with_capacity(chunk.len());
        let mut cuisines = Vec::with_capacity(chunk.len());
        let mut wikidatas = Vec::with_capacity(chunk.len());
        let mut lngs = Vec::with_capacity(chunk.len());
        let mut lats = Vec::with_capacity(chunk.len());
        let mut directions = Vec::with_capacity(chunk.len());
        let mut covereds = Vec::with_capacity(chunk.len());
        let mut backrests = Vec::with_capacity(chunk.len());
        let mut seats = Vec::with_capacity(chunk.len());
        let mut materials = Vec::with_capacity(chunk.len());
        for p in chunk {
            osm_ids.push(p.osm_id.clone());
            names.push(p.name.clone());
            amenities.push(p.amenity.clone());
            outdoor.push(p.outdoor_seating);
            websites.push(p.website.clone());
            phones.push(p.phone.clone());
            hours.push(p.opening_hours.clone());
            cuisines.push(p.cuisine.clone());
            wikidatas.push(p.wikidata.clone());
            lngs.push(p.lng);
            lats.push(p.lat);
            directions.push(p.direction_deg.map(|d| d as f32));
            covereds.push(p.covered);
            backrests.push(p.backrest);
            seats.push(p.seats);
            materials.push(p.material.clone());
        }
        written += sqlx::query(
            "INSERT INTO places (osm_id, name, amenity, outdoor_seating, website, phone, \
                                   opening_hours, cuisine, wikidata, geom, \
                                   direction_deg, covered, backrest, seats, material) \
             SELECT osm_id, name, amenity, outdoor_seating, website, phone, \
                    opening_hours, cuisine, wikidata, \
                    ST_SetSRID(ST_MakePoint(lng, lat), 4326), \
                    direction_deg, covered, backrest, seats, material \
             FROM UNNEST($1::text[], $2::text[], $3::text[], $4::bool[], $5::text[], \
                         $6::text[], $7::text[], $8::text[], $9::text[], $10::float8[], \
                         $11::float8[], $12::real[], $13::bool[], $14::bool[], \
                         $15::int[], $16::text[]) \
                  AS t(osm_id, name, amenity, outdoor_seating, website, phone, \
                       opening_hours, cuisine, wikidata, lng, lat, \
                       direction_deg, covered, backrest, seats, material) \
             ON CONFLICT (osm_id) DO UPDATE SET \
               name = EXCLUDED.name, amenity = EXCLUDED.amenity, \
               outdoor_seating = EXCLUDED.outdoor_seating, website = EXCLUDED.website, \
               phone = EXCLUDED.phone, opening_hours = EXCLUDED.opening_hours, \
               cuisine = EXCLUDED.cuisine, wikidata = EXCLUDED.wikidata, geom = EXCLUDED.geom, \
               direction_deg = EXCLUDED.direction_deg, covered = EXCLUDED.covered, \
               backrest = EXCLUDED.backrest, seats = EXCLUDED.seats, \
               material = EXCLUDED.material",
        )
        .bind(&osm_ids)
        .bind(&names)
        .bind(&amenities)
        .bind(&outdoor)
        .bind(&websites)
        .bind(&phones)
        .bind(&hours)
        .bind(&cuisines)
        .bind(&wikidatas)
        .bind(&lngs)
        .bind(&lats)
        .bind(&directions)
        .bind(&covereds)
        .bind(&backrests)
        .bind(&seats)
        .bind(&materials)
        .execute(pool)
        .await?
        .rows_affected();
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

/// Nombre de contributions **affichables**, c'est-à-dire dont l'établissement
/// existe encore.
///
/// Distinct de `contribution_count`, et pas par excès de zèle : ce dernier
/// compte tout, y compris les contributions dont l'établissement a disparu
/// d'OSM depuis, que `contributions_by_user` écarte par son `JOIN`. Servir le
/// total brut comme total de pagination promettrait des pages qu'on n'atteint
/// jamais — le défilement s'arrêterait sur un chargement perpétuel.
pub async fn listable_contribution_count(pool: &PgPool, uid: &str) -> Result<i64, sqlx::Error> {
    sqlx::query_scalar::<_, i64>(
        "SELECT count(*) FROM place_terraces t \
         JOIN places p USING (osm_id) \
         WHERE t.user_uid = $1",
    )
    .bind(uid)
    .fetch_one(pool)
    .await
}

/// Supprime le compte, et lui seul.
///
/// Les contributions **restent** : toutes les clés étrangères qui pointent sur
/// `users(uid)` sont en `ON DELETE SET NULL` (cf. `schema.sql`), donc les
/// terrasses signalées, le mobilier ajouté et les historiques survivent,
/// simplement désolidarisés de leur auteur. C'est voulu — les effacer
/// dégraderait la carte de tout le monde pour le départ d'une personne, alors
/// que ce qui est personnel (le pseudo, le lien vers l'identité Firebase) part
/// bien avec la ligne.
///
/// Renvoie `false` si aucun compte ne portait cet uid : supprimer deux fois
/// n'est pas une erreur, l'état visé est atteint dans les deux cas.
pub async fn delete_user(pool: &PgPool, uid: &str) -> Result<bool, sqlx::Error> {
    let result = sqlx::query("DELETE FROM users WHERE uid = $1")
        .bind(uid)
        .execute(pool)
        .await?;
    Ok(result.rows_affected() > 0)
}

/// Établissements auxquels un compte a contribué, du plus récent au plus ancien.
///
/// `JOIN places` et non `LEFT JOIN` : une contribution dont l'établissement a
/// disparu d'OSM depuis n'a plus rien à montrer — ni nom, ni catégorie, ni
/// position. Elle reste comptée dans le total, mais n'a pas sa place dans une
/// liste dont chaque ligne est censée être cliquable.
///
/// Tri sur `(updated_at, osm_id)` et non sur la seule date : deux contributions
/// enregistrées dans la même transaction partagent la même horodate, et
/// PostgreSQL est alors libre de les rendre dans un ordre différent d'une page
/// à l'autre — de quoi voir une ligne deux fois et en perdre une autre au
/// défilement. `osm_id` est unique par utilisateur, il tranche donc toujours.
pub async fn contributions_by_user(
    pool: &PgPool,
    uid: &str,
    limit: i64,
    offset: i64,
) -> Result<Vec<ContributionRecord>, sqlx::Error> {
    Ok(sqlx::query(
        "SELECT t.osm_id, t.has_terrace, t.updated_at, \
                p.name, p.amenity, ST_Y(p.geom) AS lat, ST_X(p.geom) AS lng \
         FROM place_terraces t \
         JOIN places p USING (osm_id) \
         WHERE t.user_uid = $1 \
         ORDER BY t.updated_at DESC, t.osm_id DESC \
         LIMIT $2 OFFSET $3",
    )
    .bind(uid)
    .bind(limit)
    .bind(offset)
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

// MARK: - Liaison OpenStreetMap

/// Ce qu'on retient d'un compte OSM lié.
pub struct OsmLink {
    pub user_id: i64,
    pub display_name: String,
    pub access_token: String,
    pub refresh_token: Option<String>,
    pub expires_at: Option<chrono::DateTime<chrono::Utc>>,
}

/// Rattache un compte OSM à un compte SunMap.
///
/// Renvoie `false` si ce compte OSM est déjà lié **ailleurs** : deux comptes
/// SunMap poussant sous la même identité rendraient les changesets illisibles,
/// et en révoquer un ne révoquerait pas l'autre.
pub async fn link_osm_account(
    pool: &PgPool,
    uid: &str,
    link: &OsmLink,
) -> Result<bool, sqlx::Error> {
    let result = sqlx::query(
        "UPDATE users SET osm_user_id = $2, osm_display_name = $3, \
                osm_access_token = $4, osm_refresh_token = $5, \
                osm_token_expires_at = $6, osm_linked_at = now(), updated_at = now() \
         WHERE uid = $1",
    )
    .bind(uid)
    .bind(link.user_id)
    .bind(&link.display_name)
    .bind(&link.access_token)
    .bind(&link.refresh_token)
    .bind(link.expires_at)
    .execute(pool)
    .await;

    match result {
        Ok(done) => Ok(done.rows_affected() > 0),
        // Violation de `users_osm_user_id_key` : le compte OSM est pris.
        Err(sqlx::Error::Database(e)) if e.code().as_deref() == Some("23505") => Ok(false),
        Err(e) => Err(e),
    }
}

/// Détache le compte OSM. Le jeton part avec — c'est la seule façon d'être sûr
/// qu'aucun envoi ne repartira en son nom.
pub async fn unlink_osm_account(pool: &PgPool, uid: &str) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE users SET osm_user_id = NULL, osm_display_name = NULL, \
                osm_access_token = NULL, osm_refresh_token = NULL, \
                osm_token_expires_at = NULL, osm_linked_at = NULL, updated_at = now() \
         WHERE uid = $1",
    )
    .bind(uid)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn osm_link(pool: &PgPool, uid: &str) -> Result<Option<OsmLink>, sqlx::Error> {
    Ok(sqlx::query(
        "SELECT osm_user_id, osm_display_name, osm_access_token, osm_refresh_token, \
                osm_token_expires_at \
         FROM users WHERE uid = $1 AND osm_access_token IS NOT NULL",
    )
    .bind(uid)
    .fetch_optional(pool)
    .await?
    .map(|r| OsmLink {
        user_id: r.get("osm_user_id"),
        display_name: r.get("osm_display_name"),
        access_token: r.get("osm_access_token"),
        refresh_token: r.get("osm_refresh_token"),
        expires_at: r.get("osm_token_expires_at"),
    }))
}

/// Met à jour le seul jeton, après un rafraîchissement.
pub async fn update_osm_token(
    pool: &PgPool,
    uid: &str,
    link: &OsmLink,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE users SET osm_access_token = $2, osm_refresh_token = $3, \
                osm_token_expires_at = $4, updated_at = now() WHERE uid = $1",
    )
    .bind(uid)
    .bind(&link.access_token)
    .bind(&link.refresh_token)
    .bind(link.expires_at)
    .execute(pool)
    .await?;
    Ok(())
}

/// Un envoi vers OSM en attente.
pub struct OsmPush {
    pub id: i64,
    pub user_uid: String,
    pub kind: String,
    pub place_id: String,
    pub payload: serde_json::Value,
    pub attempts: i32,
}

/// Met un envoi en file. Toujours appelé **après** l'écriture côté SunMap :
/// la carte doit être juste même si OSM est indisponible.
pub async fn enqueue_osm_push(
    pool: &PgPool,
    uid: &str,
    kind: &str,
    place_id: &str,
    payload: serde_json::Value,
) -> Result<i64, sqlx::Error> {
    sqlx::query_scalar::<_, i64>(
        "INSERT INTO osm_pushes (user_uid, kind, place_id, payload) \
         VALUES ($1, $2, $3, $4) RETURNING id",
    )
    .bind(uid)
    .bind(kind)
    .bind(place_id)
    .bind(payload)
    .fetch_one(pool)
    .await
}

/// Les envois à retenter, du plus ancien au plus récent.
///
/// Bornés en tentatives : un envoi qui échoue cinq fois échoue pour une raison
/// qui ne se règlera pas toute seule — élément supprimé d'OSM, jeton révoqué —
/// et le relancer indéfiniment ne ferait que marteler l'API.
pub async fn pending_osm_pushes(
    pool: &PgPool,
    limit: i64,
) -> Result<Vec<OsmPush>, sqlx::Error> {
    Ok(sqlx::query(
        "SELECT id, user_uid, kind, place_id, payload, attempts FROM osm_pushes \
         WHERE status = 'pending' AND attempts < 5 AND user_uid IS NOT NULL \
         ORDER BY created_at LIMIT $1",
    )
    .bind(limit)
    .fetch_all(pool)
    .await?
    .into_iter()
    .map(|r| OsmPush {
        id: r.get("id"),
        user_uid: r.get("user_uid"),
        kind: r.get("kind"),
        place_id: r.get("place_id"),
        payload: r.get("payload"),
        attempts: r.get("attempts"),
    })
    .collect())
}

pub async fn mark_osm_push_sent(
    pool: &PgPool,
    id: i64,
    changeset: i64,
    element: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE osm_pushes SET status = 'sent', changeset_id = $2, osm_element = $3, \
                attempts = attempts + 1, last_error = NULL, updated_at = now() WHERE id = $1",
    )
    .bind(id)
    .bind(changeset)
    .bind(element)
    .execute(pool)
    .await?;
    Ok(())
}

/// Compte la tentative et retient l'erreur. Le statut ne bascule sur `failed`
/// qu'au dernier essai : entre-temps l'envoi reste `pending`, donc rejouable.
pub async fn mark_osm_push_failed(
    pool: &PgPool,
    id: i64,
    error: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE osm_pushes SET attempts = attempts + 1, last_error = $2, \
                status = CASE WHEN attempts + 1 >= 5 THEN 'failed' ELSE 'pending' END, \
                updated_at = now() WHERE id = $1",
    )
    .bind(id)
    .bind(error)
    .execute(pool)
    .await?;
    Ok(())
}
