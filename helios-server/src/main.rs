//! Serveur de query ensoleillement.
//!
//! - `GET /sunlit?lat=&lng=[&t=][&observer_height=]` : le point est-il au
//!   soleil à l'instant t ? (t = RFC3339 ou secondes Unix, défaut maintenant)
//! - `POST /sunlit/batch` : même question pour une liste de points
//!   (classification d'établissements).
//!
//! DSM : tuiles Mapterhorn (webp 512 px, encodage Terrarium — la même source
//! que l'app iOS), assemblées 3×3 autour du point pour donner de la marge aux
//! casters (ombres portées venant de l'extérieur de la tuile centrale).

use std::collections::HashMap;
use std::sync::Arc;

use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::routing::{get, post, put};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;

use helios_core::dsm::Dsm;
use helios_core::shadow::{shadow_hit_from_ground, ShadowHit, ShadowParams};
use helios_core::sun::sun_position;
use helios_server::auth;
use helios_server::db;
use helios_server::i18n::{self, Lang};
use helios_server::opening_hours;
use helios_server::dem::{self, latlon_of_world_px, world_px, TileCache, TILE_SIZE, ZOOM};
use helios_server::osm::Building;
use helios_server::username;

/// Emprises déjà lues en base pour une bbox de tuiles donnée. PostGIS répond
/// en quelques ms, mais la même fenêtre est redemandée à chaque tick du slider
/// et le cache évite surtout de refaire le parsing WKT.
type BuildingCache = RwLock<HashMap<String, Arc<Vec<Building>>>>;
/// Résultat déjà calculé de `/places` (classification soleil/ombre), par
/// clé bbox+instant+hauteur d'observateur — évite de refaire tout le ray
/// marching quand la même requête (même minute, même zone) revient.
type PlacesResultCache = RwLock<HashMap<String, Arc<PlacesResponse>>>;

/// Valeur de la grille `owner` pour « aucun bâtiment ici » (relief nu).
const OWNER_TERRAIN: u32 = u32::MAX;

struct AppState {
    auth: auth::FirebaseAuth,
    http: reqwest::Client,
    pool: sqlx::PgPool,
    tiles: TileCache,
    buildings: BuildingCache,
    places_results: PlacesResultCache,
}

#[tokio::main]
async fn main() {
    let pool = match db::connect().await {
        Ok(p) => p,
        Err(e) => {
            eprintln!("Connexion PostgreSQL impossible : {e}");
            eprintln!(
                "Attendu : {}. Créer avec `createdb sunmap`, appliquer \
                 `helios-server/schema.sql`, puis remplir avec `cargo run --bin ingest`.",
                db::DEFAULT_URL
            );
            std::process::exit(1);
        }
    };

    for (table, label) in [
        ("buildings", "bâtiments"),
        ("trees", "arbres"),
        ("places", "établissements"),
    ] {
        let n: i64 = sqlx::query_scalar(&format!("SELECT count(*) FROM {table}"))
            .fetch_one(&pool)
            .await
            .unwrap_or(-1);
        println!("base : {n} {label}");
    }

    // Projet Firebase attendu comme destinataire des jetons. Surchargeable par
    // l'environnement pour pointer un projet de test sans recompiler.
    let project_id = std::env::var("FIREBASE_PROJECT_ID")
        .unwrap_or_else(|_| "sunmap-d06df".to_string());
    println!("authentification Firebase : projet {project_id}");

    let state = Arc::new(AppState {
        auth: auth::FirebaseAuth::new(project_id),
        // Ne sert plus qu'aux tuiles DEM Mapterhorn — la géométrie OSM vient
        // de PostGIS, plus d'Overpass au runtime.
        http: reqwest::Client::builder()
            .user_agent("sunmap-helios/0.1 (+https://github.com/VahanLab/sunmap-back)")
            .timeout(std::time::Duration::from_secs(20))
            .build()
            .expect("client HTTP"),
        pool,
        tiles: RwLock::new(HashMap::new()),
        buildings: RwLock::new(HashMap::new()),
        places_results: RwLock::new(HashMap::new()),
    });

    let app = Router::new()
        .route("/sunlit", get(sunlit))
        .route("/sunlit/batch", post(sunlit_batch))
        .route("/places", get(places))
        .route("/places/terrace", post(report_terrace))
        .route("/users/me", get(current_user))
        .route("/users/username", put(set_username))
        .route("/users/username/suggestions", get(username_suggestions))
        .route("/trees", get(trees))
        .route("/sun-hours", get(sun_hours))
        .route("/debug/ray", get(debug_ray))
        .with_state(state);

    let addr = "0.0.0.0:8080";
    println!("helios-server sur http://{addr}");
    let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
    axum::serve(listener, app).await.unwrap();
}

// ---------------------------------------------------------------- handlers

#[derive(Deserialize)]
struct SunlitQuery {
    lat: f64,
    lng: f64,
    /// RFC3339 ("2026-07-25T17:00:00Z") ou secondes Unix. Défaut : maintenant.
    t: Option<String>,
    /// Hauteur d'observateur en mètres (1.5 ≈ personne attablée). Défaut 0.
    observer_height: Option<f64>,
}

#[derive(Serialize)]
struct SunlitResponse {
    sunlit: bool,
    elevation_m: f32,
    sun_azimuth_deg: f64,
    sun_elevation_deg: f64,
    t_unix: f64,
    /// Absent si le point est au soleil (ou le soleil couché) : ce qui bloque.
    #[serde(skip_serializing_if = "Option::is_none")]
    blocker: Option<Blocker>,
}

async fn sunlit(
    State(state): State<Arc<AppState>>,
    Query(q): Query<SunlitQuery>,
) -> Result<Json<SunlitResponse>, (StatusCode, String)> {
    let t = parse_time(q.t.as_deref())?;
    let resp = classify(&state, q.lat, q.lng, t, q.observer_height.unwrap_or(0.0)).await?;
    Ok(Json(resp))
}

#[derive(Deserialize)]
struct BatchRequest {
    points: Vec<Point>,
    t: Option<String>,
    observer_height: Option<f64>,
}

#[derive(Deserialize)]
struct Point {
    lat: f64,
    lng: f64,
}

async fn sunlit_batch(
    State(state): State<Arc<AppState>>,
    Json(req): Json<BatchRequest>,
) -> Result<Json<Vec<SunlitResponse>>, (StatusCode, String)> {
    let t = parse_time(req.t.as_deref())?;
    let h = req.observer_height.unwrap_or(0.0);
    let mut out = Vec::with_capacity(req.points.len());
    for p in &req.points {
        out.push(classify(&state, p.lat, p.lng, t, h).await?);
    }
    Ok(Json(out))
}

// ------------------------------------------------------------ cœur métier

/// Assemble une DSM couvrant l'intervalle de tuiles `[x0..=x1] × [y0..=y1]`.
/// Renvoie la grille + l'origine (coin nord-ouest) en pixels monde.
async fn assemble_grid(
    state: &AppState,
    x0: u32,
    y0: u32,
    x1: u32,
    y1: u32,
    mid_lat: f64,
) -> Result<(Dsm, f64, f64), (StatusCode, String)> {
    let nx = (x1 - x0 + 1) as usize;
    let ny = (y1 - y0 + 1) as usize;
    let width = nx * TILE_SIZE;
    let height = ny * TILE_SIZE;
    let mut data = vec![0f32; width * height];

    for tyi in y0..=y1 {
        for txi in x0..=x1 {
            let tile = fetch_tile(state, ZOOM, txi, tyi).await?;
            let ox = (txi - x0) as usize * TILE_SIZE;
            let oy = (tyi - y0) as usize * TILE_SIZE;
            for row in 0..TILE_SIZE {
                let src = row * TILE_SIZE;
                let dst = (oy + row) * width + ox;
                data[dst..dst + TILE_SIZE].copy_from_slice(&tile[src..src + TILE_SIZE]);
            }
        }
    }

    let meters_per_pixel =
        40_075_016.686 * mid_lat.to_radians().cos() / ((TILE_SIZE as f64) * f64::powi(2.0, ZOOM as i32));
    let dsm = Dsm {
        width,
        height,
        meters_per_pixel,
        data,
    };
    Ok((
        dsm,
        (x0 as f64) * TILE_SIZE as f64,
        (y0 as f64) * TILE_SIZE as f64,
    ))
}

/// Rasterise les bâtiments dans la DSM par vraie rasterisation polygone
/// (scanline, règle pair-impair). L'approximation bbox-rectangle testée
/// initialement s'est révélée fausser des points hors du bâtiment : un
/// bâtiment en L ou avec cour intérieure a une bbox qui déborde largement
/// sur le trottoir voisin, stampant à tort des terrasses qui n'y sont pas
/// (observé : terrasse à +20 m d'altitude alors qu'elle est au niveau rue,
/// simplement parce que son point tombait dans la bbox d'un immeuble en L).
///
/// `owner` (même dimensions que la DSM) reçoit l'index du bâtiment qui a fixé
/// l'altitude de chaque cellule — c'est ce qui permet de répondre « quel
/// immeuble fait cette ombre » après le ray marching.
/// `terrain` est le relief nu (la DSM AVANT tout stamping) : c'est lui qui
/// donne l'altitude du sol sous chaque bâtiment, cf. commentaire plus bas.
fn stamp_buildings(
    dsm: &mut Dsm,
    terrain: &Dsm,
    owner: &mut [u32],
    origin_x: f64,
    origin_y: f64,
    buildings: &[Building],
) {
    for (bi, b) in buildings.iter().enumerate() {
        // Tous les anneaux ensemble : extérieur + cours. La règle pair-impair
        // du scanline ci-dessous fait le reste — les traversées d'un anneau
        // intérieur re-basculent en « dehors », donc la cour reste creuse.
        let rings: Vec<Vec<(f64, f64)>> = b
            .rings
            .iter()
            .filter(|r| r.len() >= 3)
            .map(|r| {
                r.iter()
                    .map(|&(lat, lon)| {
                        let (wx, wy) = world_px(lat, lon);
                        (wx - origin_x, wy - origin_y)
                    })
                    .collect()
            })
            .collect();
        if rings.is_empty() {
            continue;
        }

        let pixels: Vec<(f64, f64)> = rings.concat();
        let min_y = pixels.iter().map(|p| p.1).fold(f64::MAX, f64::min);
        let max_y = pixels.iter().map(|p| p.1).fold(f64::MIN, f64::max);
        let min_x = pixels.iter().map(|p| p.0).fold(f64::MAX, f64::min);
        let max_x = pixels.iter().map(|p| p.0).fold(f64::MIN, f64::max);
        if max_x < 0.0 || max_y < 0.0 || min_x >= dsm.width as f64 || min_y >= dsm.height as f64 {
            continue; // entièrement hors grille
        }

        // Sol de référence : centre du polygone (bbox center, approximation
        // suffisante pour l'altitude de départ — un bâtiment n'a
        // généralement pas de dénivelé notable sous son emprise).
        //
        // Échantillonné sur le RELIEF SEUL, jamais sur la DSM en cours de
        // construction : sinon un bâtiment posé sur une emprise déjà stampée
        // prend le toit du précédent pour sol et les hauteurs s'additionnent.
        // Sans conséquence tant qu'on ne chargeait que `way[building]` (les
        // empreintes se recouvrent peu), mais `building:part` recouvre par
        // construction son bâtiment parent, et les membres d'une relation se
        // recouvrent entre eux — on a observé un toit à 102 m pour un
        // bâtiment de 25 m sur un sol à 35 m, soit trois empilements.
        let cx = ((min_x + max_x) / 2.0).clamp(0.0, dsm.width as f64 - 1.0);
        let cy = ((min_y + max_y) / 2.0).clamp(0.0, dsm.height as f64 - 1.0);
        let target = terrain.sample(cx, cy).unwrap_or(0.0) + b.height_m;

        let y0 = min_y.max(0.0).floor() as usize;
        let y1 = max_y.min(dsm.height as f64 - 1.0).ceil() as usize;
        for y in y0..=y1.min(dsm.height - 1) {
            let scan_y = y as f64 + 0.5;
            let mut xs: Vec<f64> = Vec::new();
            for ring in &rings {
                for i in 0..ring.len() {
                    let (x1, y1p) = ring[i];
                    let (x2, y2p) = ring[(i + 1) % ring.len()];
                    if (y1p <= scan_y && y2p > scan_y) || (y2p <= scan_y && y1p > scan_y) {
                        let t = (scan_y - y1p) / (y2p - y1p);
                        xs.push(x1 + t * (x2 - x1));
                    }
                }
            }
            xs.sort_by(|a, b| a.partial_cmp(b).unwrap());

            let mut i = 0;
            while i + 1 < xs.len() {
                let x_start = xs[i].round().max(0.0) as usize;
                let x_end = xs[i + 1].round().min(dsm.width as f64 - 1.0);
                if x_end >= 0.0 && x_end as usize >= x_start {
                    for x in x_start..=(x_end as usize).min(dsm.width - 1) {
                        let idx = y * dsm.width + x;
                        if dsm.data[idx] < target {
                            dsm.data[idx] = target;
                            owner[idx] = bi as u32;
                        }
                    }
                }
                i += 2;
            }
        }
    }
}

/// Récupère les bâtiments couvrant l'emprise de la DSM et les rasterise
/// dedans. Bounds calculées depuis l'origine + la taille de la grille
/// (même étendue que les tuiles Mapterhorn assemblées).
async fn add_buildings(
    state: &AppState,
    dsm: &mut Dsm,
    origin_x: f64,
    origin_y: f64,
) -> Result<(Arc<Vec<Building>>, Vec<u32>), (StatusCode, String)> {
    let (north, west) = latlon_of_world_px(origin_x, origin_y);
    let (south, east) = latlon_of_world_px(origin_x + dsm.width as f64, origin_y + dsm.height as f64);
    let buildings = load_buildings(state, south, west, north, east).await?;
    let mut owner = vec![OWNER_TERRAIN; dsm.width * dsm.height];
    let terrain = dsm.clone();
    stamp_buildings(dsm, &terrain, &mut owner, origin_x, origin_y, &buildings);
    Ok((buildings, owner))
}

/// Emprises de la zone, depuis PostGIS. Mémoïsé par bbox : celle-ci est
/// alignée sur les tuiles DEM, donc la même fenêtre revient à chaque tick du
/// slider — le cache évite surtout de refaire le parsing WKT, la requête
/// spatiale elle-même étant servie par l'index GIST en quelques ms.
async fn load_buildings(
    state: &AppState,
    s: f64,
    w: f64,
    n: f64,
    e: f64,
) -> Result<Arc<Vec<Building>>, (StatusCode, String)> {
    let key = format!("{s:.5},{w:.5},{n:.5},{e:.5}");
    if let Some(hit) = state.buildings.read().await.get(&key) {
        return Ok(hit.clone());
    }

    let buildings = db::buildings_in_bbox(&state.pool, s, w, n, e)
        .await
        .map_err(|err| (StatusCode::INTERNAL_SERVER_ERROR, format!("PostGIS : {err}")))?;
    println!("[buildings] {key} → {} emprises", buildings.len());

    let arc = Arc::new(buildings);
    state.buildings.write().await.insert(key, arc.clone());
    Ok(arc)
}

/// Ce qui bloque le soleil sur un point donné, tel que renvoyé aux clients.
#[derive(Serialize, Clone)]
struct Blocker {
    /// "way/123456" si c'est un bâtiment OSM, "terrain" si c'est le relief.
    id: String,
    /// Nom OSM du bâtiment s'il en a un.
    name: Option<String>,
    /// Hauteur retenue pour le bâtiment, et si elle vient d'un tag OSM ou du
    /// défaut (`DEFAULT_BUILDING_HEIGHT_M`) — un `false` ici explique la
    /// plupart des désaccords visuels avec la réalité.
    height_m: Option<f32>,
    height_from_osm: bool,
    /// Position (centre de la cellule DSM) et distance depuis le point testé.
    lat: f64,
    lng: f64,
    distance_m: f64,
    /// Altitude de l'obstacle vs altitude du rayon à cet endroit : l'écart dit
    /// de combien il manque au point pour voir le soleil.
    obstacle_elevation_m: f32,
    ray_elevation_m: f64,
}

/// Traduit un `ShadowHit` (cellule DSM) en `Blocker` nommé.
fn describe_blocker(
    hit: &ShadowHit,
    dsm: &Dsm,
    owner: &[u32],
    buildings: &[Building],
    origin_x: f64,
    origin_y: f64,
) -> Blocker {
    let (lat, lng) = latlon_of_world_px(origin_x + hit.x as f64 + 0.5, origin_y + hit.y as f64 + 0.5);
    let b = owner
        .get(hit.y * dsm.width + hit.x)
        .copied()
        .filter(|&o| o != OWNER_TERRAIN)
        .and_then(|o| buildings.get(o as usize));

    Blocker {
        id: b.map_or_else(|| "terrain".to_string(), |b| b.osm_id.clone()),
        name: b.and_then(|b| b.name.clone()),
        height_m: b.map(|b| b.height_m),
        height_from_osm: b.is_some_and(|b| b.height_from_osm),
        lat,
        lng,
        distance_m: hit.distance_m,
        obstacle_elevation_m: hit.obstacle_elevation_m,
        ray_elevation_m: hit.ray_elevation_m,
    }
}

/// Assemble la DSM 3×3 tuiles autour d'un point + bâtiments, et renvoie tout
/// ce qu'il faut pour classer ce point à n'importe quel instant : la DSM
/// stampée (obstacles), ses coordonnées pixel locales, et son altitude de
/// sol sur le relief SEUL (avant bâtiments — cf. `is_shadowed_from_ground` :
/// un POI dont les coordonnées OSM tombent par erreur à l'intérieur d'un
/// immeuble ne doit pas hériter de l'altitude du toit).
async fn assemble_point(
    state: &AppState,
    lat: f64,
    lng: f64,
) -> Result<PointCtx, (StatusCode, String)> {
    if !(-85.0..=85.0).contains(&lat) || !(-180.0..=180.0).contains(&lng) {
        return Err((StatusCode::BAD_REQUEST, "lat/lng hors bornes".into()));
    }

    // Tuile centrale + marge d'une tuile ≈ 1,2 km de casters à z15.
    let (wx, wy) = world_px(lat, lng);
    let tx = (wx / TILE_SIZE as f64) as u32;
    let ty = (wy / TILE_SIZE as f64) as u32;
    let max_tile = (1u32 << ZOOM) - 1;
    let (mut dsm, origin_x, origin_y) = assemble_grid(
        state,
        tx.saturating_sub(1),
        ty.saturating_sub(1),
        (tx + 1).min(max_tile),
        (ty + 1).min(max_tile),
        lat,
    )
    .await?;

    let px = wx - origin_x;
    let py = wy - origin_y;
    let ground = dsm.sample(px, py).unwrap_or(0.0);
    let (buildings, owner) = add_buildings(state, &mut dsm, origin_x, origin_y).await?;

    Ok(PointCtx {
        dsm,
        owner,
        buildings,
        origin_x,
        origin_y,
        px,
        py,
        ground,
    })
}

/// Tout ce qu'il faut pour classer un point à n'importe quel instant, plus de
/// quoi nommer l'obstacle rencontré.
struct PointCtx {
    dsm: Dsm,
    owner: Vec<u32>,
    buildings: Arc<Vec<Building>>,
    origin_x: f64,
    origin_y: f64,
    px: f64,
    py: f64,
    /// Altitude du relief SEUL sous le point (avant stamping des bâtiments).
    ground: f32,
}

impl PointCtx {
    /// Le point est-il à l'ombre à cet instant, et si oui à cause de quoi ?
    fn classify_at(&self, sun: &helios_core::sun::SunPosition, params: &ShadowParams) -> (bool, Option<Blocker>) {
        if !sun.is_up() {
            return (false, None);
        }
        match shadow_hit_from_ground(&self.dsm, sun, self.px, self.py, self.ground, params) {
            None => (true, None),
            Some(hit) => (
                false,
                Some(describe_blocker(
                    &hit,
                    &self.dsm,
                    &self.owner,
                    &self.buildings,
                    self.origin_x,
                    self.origin_y,
                )),
            ),
        }
    }
}

async fn classify(
    state: &AppState,
    lat: f64,
    lng: f64,
    t_unix: f64,
    observer_height_m: f64,
) -> Result<SunlitResponse, (StatusCode, String)> {
    let sun = sun_position(t_unix, lat, lng);
    let ctx = assemble_point(state, lat, lng).await?;

    let params = ShadowParams {
        max_distance_m: 5_000.0, // relief : ombres longues possibles
        observer_height_m,
        step_px: 1.0,
    };
    let (sunlit, blocker) = ctx.classify_at(&sun, &params);

    Ok(SunlitResponse {
        sunlit,
        elevation_m: ctx.ground,
        sun_azimuth_deg: sun.azimuth_deg,
        sun_elevation_deg: sun.elevation_deg,
        t_unix,
        blocker,
    })
}

// ------------------------------------------------------------ sun-hours

#[derive(Deserialize)]
struct SunHoursQuery {
    lat: f64,
    lng: f64,
    /// N'importe quel instant DANS la journée voulue (RFC3339 ou secondes
    /// Unix). La journée est le jour calendaire UTC contenant `t`. Défaut :
    /// maintenant.
    t: Option<String>,
    /// Défaut 1,5 m : usage principal = "est-ce que je peux m'asseoir là".
    observer_height: Option<f64>,
    /// Décalage horaire du LIEU par rapport à UTC, en minutes (Paris en été :
    /// 120). Détermine où sont les bornes de la journée renvoyée. Défaut 0,
    /// soit la journée UTC — rarement ce qu'on veut, le client doit
    /// l'envoyer.
    utc_offset_minutes: Option<i32>,
}

#[derive(Serialize)]
struct SunHoursResponse {
    lat: f64,
    lng: f64,
    elevation_m: f32,
    /// Instant demandé et sa classification, pour un statut "maintenant"
    /// immédiat sans avoir à chercher dans `intervals`.
    t_unix: f64,
    state_now: SunState,
    /// Ce qui bloque le soleil à `t_unix` (absent si au soleil).
    #[serde(skip_serializing_if = "Option::is_none")]
    blocker_now: Option<Blocker>,
    day_start_unix: f64,
    day_end_unix: f64,
    /// Décalage effectivement utilisé pour découper la journée — permet au
    /// client de vérifier que le serveur a bien compris son fuseau.
    utc_offset_minutes: i32,
    total_sunlit_minutes: u32,
    /// Ombre portée de jour uniquement. La nuit est comptée à part : un point
    /// « à l'ombre 16 h » alors qu'il fait nuit 10 h de ces 16 h ne dit rien
    /// d'utile sur la qualité de l'endroit.
    total_shadow_minutes: u32,
    total_night_minutes: u32,
    intervals: Vec<SunInterval>,
}

/// État d'un point vis-à-vis du soleil.
///
/// Trois cas et non deux : confondre l'ombre portée et la nuit rendait les
/// cumuls trompeurs, et la frise de la journée illisible.
#[derive(Serialize, Clone, Copy, PartialEq, Eq, Debug)]
#[serde(rename_all = "lowercase")]
enum SunState {
    Sunlit,
    /// Soleil levé, mais un obstacle le masque.
    Shadow,
    /// Soleil sous l'horizon.
    Night,
}

#[derive(Serialize)]
struct SunInterval {
    start_unix: f64,
    end_unix: f64,
    state: SunState,
}

/// Journée calendaire **locale** (00:00 → 24:00) contenant l'instant donné,
/// pour un décalage donné en minutes par rapport à UTC.
///
/// Découper sur la journée UTC était faux dès qu'on sort du méridien de
/// Greenwich : à Paris en été (UTC+2), un slider réglé sur 00h30 renvoyait la
/// journée de la veille, et la timeline s'affichait bornée à 02:00 → 02:00 au
/// lieu de minuit à minuit.
fn day_bounds_local(t_unix: f64, utc_offset_minutes: i32) -> (f64, f64) {
    let offset = utc_offset_minutes as f64 * 60.0;
    // Passer en heure locale, tronquer au jour, revenir en UTC.
    let local = t_unix + offset;
    let start_local = (local / 86_400.0).floor() * 86_400.0;
    let start_unix = start_local - offset;
    (start_unix, start_unix + 86_400.0)
}

/// Un point, une journée : les heures au soleil et à l'ombre. Échantillonne
/// toutes les 5 min (pas du slider iOS) et regroupe en intervalles
/// contigus — plus léger à consommer côté client qu'une valeur par tick.
async fn sun_hours(
    State(state): State<Arc<AppState>>,
    Query(q): Query<SunHoursQuery>,
) -> Result<Json<SunHoursResponse>, (StatusCode, String)> {
    let t = parse_time(q.t.as_deref())?;
    let h = q.observer_height.unwrap_or(1.5);
    let utc_offset_minutes = q.utc_offset_minutes.unwrap_or(0);
    let (day_start, day_end) = day_bounds_local(t, utc_offset_minutes);

    let ctx = assemble_point(&state, q.lat, q.lng).await?;
    let params = ShadowParams {
        max_distance_m: 5_000.0,
        observer_height_m: h,
        step_px: 1.0,
    };

    const STEP_S: f64 = 300.0; // 5 min
    let steps = (86_400.0 / STEP_S) as usize;

    let mut intervals: Vec<SunInterval> = Vec::new();
    let mut state_now = SunState::Night;
    let mut blocker_now = None;
    let (mut sunlit_steps, mut shadow_steps, mut night_steps) = (0u32, 0u32, 0u32);

    for i in 0..steps {
        let step_t = day_start + i as f64 * STEP_S;
        let sun = sun_position(step_t, q.lat, q.lng);
        let (sunlit, blocker) = ctx.classify_at(&sun, &params);
        let state = if !sun.is_up() {
            SunState::Night
        } else if sunlit {
            SunState::Sunlit
        } else {
            SunState::Shadow
        };
        match state {
            SunState::Sunlit => sunlit_steps += 1,
            SunState::Shadow => shadow_steps += 1,
            SunState::Night => night_steps += 1,
        }
        if step_t <= t && t < step_t + STEP_S {
            state_now = state;
            blocker_now = blocker;
        }

        match intervals.last_mut() {
            Some(last) if last.state == state => last.end_unix = step_t + STEP_S,
            _ => intervals.push(SunInterval {
                start_unix: step_t,
                end_unix: step_t + STEP_S,
                state,
            }),
        }
    }

    let total_sunlit_minutes = sunlit_steps * 5;
    let total_shadow_minutes = shadow_steps * 5;
    let total_night_minutes = night_steps * 5;

    Ok(Json(SunHoursResponse {
        lat: q.lat,
        lng: q.lng,
        elevation_m: ctx.ground,
        t_unix: t,
        state_now,
        blocker_now,
        day_start_unix: day_start,
        day_end_unix: day_end,
        utc_offset_minutes,
        total_sunlit_minutes,
        total_shadow_minutes,
        total_night_minutes,
        intervals,
    }))
}

/// Déplace un point tombé *dans* une emprise bâtie vers le sol libre le plus
/// proche (trottoir), et renvoie `(px, py, distance parcourue en mètres)`.
///
/// Nécessaire parce que les POI OSM d'un bar ou d'un restaurant sont posés sur
/// le bâtiment, pas sur sa terrasse : le nœud tombe à l'intérieur du polygone
/// dans la grande majorité des cas. Le rayon solaire percute alors le mur du
/// bâtiment hôte au tout premier pas et *tout* est classé à l'ombre — observé :
/// 410 terrasses sur 416 bloquées à 1,57 m, soit exactement une cellule.
///
/// Sortir le point est plus juste que d'exclure le bâtiment hôte du test :
/// depuis le trottoir, sa façade continue de porter ombre le soir, ce qui est
/// bien le comportement attendu.
///
/// Recherche en anneaux croissants sur la grille de propriétaires, plus une
/// cellule de marge pour ne pas rester collé à la façade. Renvoie le point
/// d'origine si rien de libre dans le rayon (POI au cœur d'un grand bâtiment).
fn nudge_out_of_building(
    dsm: &Dsm,
    owner: &[u32],
    px: f64,
    py: f64,
    max_radius_px: i32,
) -> (f64, f64, f64) {
    let at = |x: i32, y: i32| -> Option<u32> {
        if x < 0 || y < 0 || x >= dsm.width as i32 || y >= dsm.height as i32 {
            return None;
        }
        Some(owner[y as usize * dsm.width + x as usize])
    };

    let (cx, cy) = (px.round() as i32, py.round() as i32);
    if at(cx, cy) == Some(OWNER_TERRAIN) {
        return (px, py, 0.0); // déjà dehors
    }

    for r in 1..=max_radius_px {
        let mut best: Option<(f64, i32, i32)> = None;
        for dy in -r..=r {
            for dx in -r..=r {
                // Seulement le bord de l'anneau : l'intérieur a déjà été vu.
                if dx.abs() != r && dy.abs() != r {
                    continue;
                }
                if at(cx + dx, cy + dy) != Some(OWNER_TERRAIN) {
                    continue;
                }
                let d = ((dx * dx + dy * dy) as f64).sqrt();
                if best.as_ref().is_none_or(|(bd, _, _)| d < *bd) {
                    best = Some((d, dx, dy));
                }
            }
        }
        if let Some((d, dx, dy)) = best {
            // Une cellule de marge dans la même direction, pour se poser sur
            // le trottoir plutôt que contre le mur.
            let (ux, uy) = (dx as f64 / d, dy as f64 / d);
            let (nx, ny) = (px + (dx as f64) + ux, py + (dy as f64) + uy);
            let (nx, ny) = if at(nx.round() as i32, ny.round() as i32) == Some(OWNER_TERRAIN) {
                (nx, ny)
            } else {
                (px + dx as f64, py + dy as f64)
            };
            let moved = ((nx - px).hypot(ny - py)) * dsm.meters_per_pixel;
            return (nx, ny, moved);
        }
    }
    (px, py, 0.0)
}

// ---------------------------------------------------------------- debug

/// Profil de la DSM le long du rayon solaire : altitude du terrain+bâtiments
/// contre altitude du rayon, pas à pas. Sert à voir *pourquoi* un point est
/// classé comme il l'est — bâtiment manquant, trop bas, mal placé, etc.
#[derive(Serialize)]
struct RayStep {
    distance_m: f64,
    lat: f64,
    lng: f64,
    /// Altitude de la DSM (relief + bâtiments stampés) à ce pas.
    dsm_m: f32,
    /// Altitude du rayon solaire à ce pas.
    ray_m: f64,
    /// Bâtiment occupant la cellule, s'il y en a un.
    building: Option<String>,
    building_height_m: Option<f32>,
    /// `true` sur le pas qui bloque le rayon.
    blocks: bool,
}

#[derive(Serialize)]
struct DebugRayResponse {
    sun_azimuth_deg: f64,
    sun_elevation_deg: f64,
    ground_m: f32,
    observer_m: f64,
    meters_per_pixel: f64,
    /// Bâtiments chargés dans la fenêtre DSM — un nombre anormalement bas
    /// signale une troncature Overpass.
    buildings_loaded: usize,
    sunlit: bool,
    steps: Vec<RayStep>,
}

async fn debug_ray(
    State(state): State<Arc<AppState>>,
    Query(q): Query<SunlitQuery>,
) -> Result<Json<DebugRayResponse>, (StatusCode, String)> {
    let t = parse_time(q.t.as_deref())?;
    let h = q.observer_height.unwrap_or(1.5);
    let sun = sun_position(t, q.lat, q.lng);
    let ctx = assemble_point(&state, q.lat, q.lng).await?;

    let rad = std::f64::consts::PI / 180.0;
    let (dx, dy) = ((sun.azimuth_deg * rad).sin(), -(sun.azimuth_deg * rad).cos());
    let tan_elev = (sun.elevation_deg * rad).tan();
    let step_m = ctx.dsm.meters_per_pixel;
    let z0 = ctx.ground as f64 + h;

    // 200 pas ≈ 315 m : largement de quoi couvrir les casters urbains.
    let mut steps = Vec::new();
    let mut blocked = false;
    for i in 1..=200usize {
        let (x, y) = (ctx.px + dx * i as f64, ctx.py + dy * i as f64);
        let Some(dsm_m) = ctx.dsm.sample(x, y) else { break };
        let ray_m = z0 + i as f64 * step_m * tan_elev;
        let blocks = !blocked && (dsm_m as f64) > ray_m;
        if blocks {
            blocked = true;
        }
        let (xi, yi) = (x.round().max(0.0) as usize, y.round().max(0.0) as usize);
        let b = ctx
            .owner
            .get(yi * ctx.dsm.width + xi)
            .copied()
            .filter(|&o| o != OWNER_TERRAIN)
            .and_then(|o| ctx.buildings.get(o as usize));
        let (lat, lng) = latlon_of_world_px(ctx.origin_x + x, ctx.origin_y + y);
        steps.push(RayStep {
            distance_m: i as f64 * step_m,
            lat,
            lng,
            dsm_m,
            ray_m,
            building: b.map(|b| b.osm_id.clone()),
            building_height_m: b.map(|b| b.height_m),
            blocks,
        });
    }

    Ok(Json(DebugRayResponse {
        sun_azimuth_deg: sun.azimuth_deg,
        sun_elevation_deg: sun.elevation_deg,
        ground_m: ctx.ground,
        observer_m: z0,
        meters_per_pixel: ctx.dsm.meters_per_pixel,
        buildings_loaded: ctx.buildings.len(),
        sunlit: !blocked && sun.is_up(),
        steps,
    }))
}

/// Tuile Mapterhorn décodée, via le cache partagé du process.
async fn fetch_tile(
    state: &AppState,
    z: u32,
    x: u32,
    y: u32,
) -> Result<Arc<Vec<f32>>, (StatusCode, String)> {
    dem::fetch_tile(&state.http, &state.tiles, z, x, y)
        .await
        .map_err(|e| (StatusCode::BAD_GATEWAY, e))
}

// ---------------------------------------------------------- établissements

#[derive(Deserialize)]
struct PlacesQuery {
    /// `min_lon,min_lat,max_lon,max_lat`
    bbox: String,
    /// Langue des libellés ("fr", "en"). Défaut : français.
    lang: Option<String>,
    t: Option<String>,
    /// Défaut 1,5 m : personne attablée en terrasse.
    observer_height: Option<f64>,
}

#[derive(Serialize, Clone)]
struct PlacesResponse {
    t_unix: f64,
    sun_azimuth_deg: f64,
    sun_elevation_deg: f64,
    count: usize,
    places: Vec<Place>,
}

#[derive(Serialize, Clone)]
struct Place {
    /// Identifiant OSM, ex. "node/123456" ou "way/789".
    id: String,
    name: Option<String>,
    amenity: Option<String>,
    /// Tag `outdoor_seating` d'OSM. **Absent de la réponse si non renseigné** —
    /// ce qui est le cas le plus fréquent, et ne signifie pas « pas de
    /// terrasse ». Le client ne doit rien afficher dans ce cas.
    #[serde(skip_serializing_if = "Option::is_none")]
    outdoor_seating: Option<bool>,
    /// `true` quand la présence et/ou la position de la terrasse viennent d'une
    /// contribution utilisateur plutôt que d'OSM. Dans ce cas `snapped_*` est
    /// la position exacte de la terrasse, pas une estimation.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    terrace_from_user: bool,
    lat: f64,
    lng: f64,
    sunlit: bool,
    /// Distingue l'ombre portée de la nuit, ce que `sunlit` seul ne peut pas
    /// dire — les deux y valent `false`.
    state: SunState,
    /// Ce qui bloque le soleil (absent si `sunlit`) — sert au debug visuel
    /// côté client : « c'est cet immeuble-là qui te met à l'ombre ».
    #[serde(skip_serializing_if = "Option::is_none")]
    blocker: Option<Blocker>,
    /// Absents si le nœud OSM était déjà sur du sol libre. Sinon : le point
    /// réellement classé, ramené hors de l'emprise du bâtiment hôte, et la
    /// distance parcourue pour y arriver.
    #[serde(skip_serializing_if = "Option::is_none")]
    snapped_lat: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    snapped_lng: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    snapped_distance_m: Option<f64>,
    elevation_m: f32,
    /// Champs OSM optionnels — couverture très inégale selon les POI.
    website: Option<String>,
    phone: Option<String>,
    /// Libellé traduit de la catégorie — le client n'a plus à connaître les
    /// valeurs de tags OSM.
    category_label: Option<String>,
    /// Types de cuisine traduits. Le tag OSM est une liste séparée par `;` et
    /// remplie de clés techniques (`coffee_shop`), inutilisables telles quelles.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    cuisine_labels: Vec<String>,
    opening_hours: Option<OpeningHoursPayload>,
    cuisine: Option<String>,
    /// Identifiant Wikidata (ex. "Q123456") : présent sur ~15% des POI,
    /// utilisable côté client pour aller chercher une photo (propriété P18)
    /// via l'API Wikidata/Commons — OSM ne stocke pas de photos lui-même.
    wikidata: Option<String>,
}

#[derive(Deserialize)]
struct TerraceReportBody {
    /// Identifiant OSM de l'établissement ("node/123"). Dans le corps et non
    /// dans le chemin : il contient une barre oblique, qui casserait le
    /// routage.
    osm_id: String,
    has_terrace: bool,
    /// Position de la terrasse. Facultative : on peut signaler une terrasse
    /// sans la situer, ou signaler son absence.
    lat: Option<f64>,
    lng: Option<f64>,
}

#[derive(Serialize)]
struct TerraceReportResponse {
    osm_id: String,
    has_terrace: bool,
    located: bool,
}

/// Enregistre la terrasse signalée par un utilisateur : sa présence, et sa
/// position si elle a été pointée sur la carte.
///
/// Cette position vaut mieux que tout ce qu'on peut déduire : OSM place le nœud
/// d'un bar sur son bâtiment, et notre repli le ressort au jugé sur le sol
/// libre le plus proche, sans savoir de quel côté est la rue.
async fn report_terrace(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(body): Json<TerraceReportBody>,
) -> Result<Json<TerraceReportResponse>, (StatusCode, String)> {
    // Contribuer demande un compte : sans quoi n'importe qui écrase la
    // contribution de n'importe qui, `osm_id` étant la clé primaire.
    let identity = authenticate(&state, &headers).await?;
    if let (Some(lat), Some(lng)) = (body.lat, body.lng) {
        if !(-85.0..=85.0).contains(&lat) || !(-180.0..=180.0).contains(&lng) {
            return Err((StatusCode::BAD_REQUEST, "lat/lng hors bornes".into()));
        }
    }

    // Refuse les identifiants inconnus, sinon la table se remplit de lignes
    // orphelines qu'aucune requête ne rattachera jamais à un établissement.
    let exists = db::place_exists(&state.pool, &body.osm_id)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("PostGIS : {e}")))?;
    if !exists {
        return Err((
            StatusCode::NOT_FOUND,
            format!("établissement inconnu : {}", body.osm_id),
        ));
    }

    let report = db::TerraceReport {
        osm_id: body.osm_id.clone(),
        has_terrace: body.has_terrace,
        // Une terrasse absente n'a pas de position à retenir.
        author_uid: Some(identity.uid.clone()),
        author_username: None,
        lat: body.has_terrace.then_some(body.lat).flatten(),
        lng: body.has_terrace.then_some(body.lng).flatten(),
    };
    db::upsert_terrace_report(&state.pool, &report)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("PostGIS : {e}")))?;

    // Les classifications déjà calculées ignorent cette contribution : les
    // jeter force le recalcul au prochain appel, sinon l'utilisateur ne verrait
    // aucun effet à sa contribution.
    state.places_results.write().await.clear();

    println!(
        "[terrace] {} → has_terrace={} {}",
        report.osm_id,
        report.has_terrace,
        match (report.lat, report.lng) {
            (Some(lat), Some(lng)) => format!("({lat:.6}, {lng:.6})"),
            _ => "sans position".to_string(),
        }
    );

    Ok(Json(TerraceReportResponse {
        located: report.lat.is_some() && report.lng.is_some(),
        osm_id: report.osm_id,
        has_terrace: report.has_terrace,
    }))
}

/// Horaires d'ouverture prêts à afficher.
///
/// `weekly` absent = chaîne non décodable : le client affiche `raw` tel quel.
/// Le décodage vit ici pour que les clients n'aient qu'à rendre, et pour
/// qu'Android n'ait pas à réécrire la grammaire `opening_hours`.
#[derive(Serialize, Clone)]
struct OpeningHoursPayload {
    /// Toujours présent, pour l'affichage de repli et pour le debug.
    raw: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    weekly: Option<Vec<OpeningHoursDay>>,
}

#[derive(Serialize, Clone)]
struct OpeningHoursDay {
    /// 0 = lundi.
    index: usize,
    label: String,
    /// Vide = fermé ce jour-là.
    ranges: Vec<String>,
}

fn opening_hours_payload(raw: &str, lang: Lang) -> OpeningHoursPayload {
    let weekly = opening_hours::parse(raw).map(|week| {
        let labels = lang.weekdays();
        week.iter()
            .enumerate()
            .map(|(index, ranges)| OpeningHoursDay {
                index,
                label: labels[index].to_string(),
                ranges: ranges
                    .iter()
                    .map(|r| {
                        if r.is_all_day() {
                            lang.all_day().to_string()
                        } else {
                            r.text()
                        }
                    })
                    .collect(),
            })
            .collect()
    });
    OpeningHoursPayload {
        raw: raw.to_string(),
        weekly,
    }
}

/// Bars/restaurants/cafés avec terrasse dans la bbox, classés soleil/ombre.
///
/// Limite assumée (POC) : classification binaire au centroïde — une terrasse
/// est un polygone, potentiellement mi-ombre mi-soleil. Prochaine étape :
/// échantillonner 3-5 points dans un buffer côté rue et renvoyer un %.
async fn places(
    State(state): State<Arc<AppState>>,
    Query(q): Query<PlacesQuery>,
) -> Result<Json<PlacesResponse>, (StatusCode, String)> {
    let t = parse_time(q.t.as_deref())?;
    let h = q.observer_height.unwrap_or(1.5);
    let lang = Lang::parse(q.lang.as_deref());

    // bbox = min_lon,min_lat,max_lon,max_lat
    let parts: Vec<f64> = q
        .bbox
        .split(',')
        .filter_map(|s| s.trim().parse().ok())
        .collect();
    let [w, s, e, n] = parts[..] else {
        return Err((
            StatusCode::BAD_REQUEST,
            "bbox attendue : min_lon,min_lat,max_lon,max_lat".into(),
        ));
    };
    if s >= n || w >= e || !(-85.0..=85.0).contains(&s) || !(-85.0..=85.0).contains(&n) {
        return Err((StatusCode::BAD_REQUEST, "bbox invalide".into()));
    }

    // Cache de résultat : même bbox + même tranche de 5 min + même hauteur
    // d'observateur → renvoie directement la classification déjà calculée,
    // sans refaire tuiles/bâtiments/ray marching.
    let bucket = (t / 300.0).round() as i64;
    let result_key = format!("{w:.4},{s:.4},{e:.4},{n:.4},{bucket},{h:.2},{lang:?}");
    if let Some(hit) = state.places_results.read().await.get(&result_key) {
        return Ok(Json((**hit).clone()));
    }

    // Intervalle de tuiles couvrant la bbox + 1 tuile de marge (casters).
    let (wx0, wy0) = world_px(n, w); // coin nord-ouest
    let (wx1, wy1) = world_px(s, e); // coin sud-est
    let max_tile = (1u32 << ZOOM) - 1;
    let tx0 = ((wx0 / TILE_SIZE as f64) as u32).saturating_sub(1);
    let ty0 = ((wy0 / TILE_SIZE as f64) as u32).saturating_sub(1);
    let tx1 = ((wx1 / TILE_SIZE as f64) as u32 + 1).min(max_tile);
    let ty1 = ((wy1 / TILE_SIZE as f64) as u32 + 1).min(max_tile);
    if (tx1 - tx0 + 1) > 8 || (ty1 - ty0 + 1) > 8 {
        return Err((
            StatusCode::BAD_REQUEST,
            "bbox trop grande (max ~3 km de côté à ce zoom)".into(),
        ));
    }

    let pois = db::places_in_bbox(&state.pool, s, w, n, e)
        .await
        .map_err(|err| (StatusCode::INTERNAL_SERVER_ERROR, format!("PostGIS : {err}")))?;
    let reports = db::terrace_reports_in_bbox(&state.pool, s, w, n, e)
        .await
        .map_err(|err| (StatusCode::INTERNAL_SERVER_ERROR, format!("PostGIS : {err}")))?;
    let (mut dsm, origin_x, origin_y) = assemble_grid(&state, tx0, ty0, tx1, ty1, (s + n) / 2.0).await?;
    // Relief seul, avant stamping des bâtiments : sert de sol pour chaque
    // terrasse (cf. commentaire dans classify() — un POI mal placé dans un
    // bâtiment côté OSM ne doit pas hériter de l'altitude du toit).
    let terrain_only = dsm.clone();
    let (buildings, owner) = add_buildings(&state, &mut dsm, origin_x, origin_y).await?;

    let mid_lat = (s + n) / 2.0;
    let mid_lng = (w + e) / 2.0;
    let sun = sun_position(t, mid_lat, mid_lng);
    let params = ShadowParams {
        max_distance_m: 5_000.0,
        observer_height_m: h,
        step_px: 1.0,
    };

    let places: Vec<Place> = pois
        .iter()
        .map(|p| {
            let report = reports.get(&p.osm_id);
            let terrace = report.and_then(|r| Some((r.lat?, r.lng?)));
            let (osm_wx, osm_wy) = world_px(p.lat, p.lng);

            // Une terrasse située par un utilisateur est prise telle quelle :
            // il l'a pointée sur la carte, donc elle est déjà au bon endroit.
            // La rectifier avec `nudge_out_of_building` détruirait justement la
            // précision qu'on lui a demandée — une terrasse sous arcade ou
            // adossée à la façade se retrouverait déplacée au milieu de la rue.
            let (px, py, moved_m) = match terrace {
                Some((lat, lng)) => {
                    let (tx, ty) = world_px(lat, lng);
                    let moved = ((tx - osm_wx).hypot(ty - osm_wy)) * dsm.meters_per_pixel;
                    (tx - origin_x, ty - origin_y, moved)
                }
                // Sinon le nœud OSM est presque toujours posé sur le bâtiment
                // et non sur la terrasse : on le ramène sur le sol libre
                // voisin (12 m max).
                None => nudge_out_of_building(&dsm, &owner, osm_wx - origin_x,
                                              osm_wy - origin_y, 8),
            };
            let ground = terrain_only.sample(px, py).unwrap_or(0.0);
            let hit = sun
                .is_up()
                .then(|| shadow_hit_from_ground(&dsm, &sun, px, py, ground, &params))
                .flatten();
            let (snapped_lat, snapped_lng) =
                latlon_of_world_px(origin_x + px, origin_y + py);
            Place {
                id: p.osm_id.clone(),
                name: p.name.clone(),
                amenity: p.amenity.clone(),
                // La contribution prime sur le tag OSM, y compris pour le
                // contredire : quelqu'un sur place en sait plus que le tag.
                outdoor_seating: report.map(|r| r.has_terrace).or(p.outdoor_seating),
                terrace_from_user: report.is_some(),
                lat: p.lat,
                lng: p.lng,
                sunlit: sun.is_up() && hit.is_none(),
                state: if !sun.is_up() {
                    SunState::Night
                } else if hit.is_none() {
                    SunState::Sunlit
                } else {
                    SunState::Shadow
                },
                blocker: hit
                    .map(|h| describe_blocker(&h, &dsm, &owner, &buildings, origin_x, origin_y)),
                snapped_lat: (moved_m > 0.0).then_some(snapped_lat),
                snapped_lng: (moved_m > 0.0).then_some(snapped_lng),
                snapped_distance_m: (moved_m > 0.0).then_some(moved_m),
                elevation_m: ground,
                website: p.website.clone(),
                phone: p.phone.clone(),
                category_label: p.amenity.as_deref().map(|a| i18n::amenity_label(a, lang)),
                cuisine_labels: p
                    .cuisine
                    .as_deref()
                    .map(|c| i18n::cuisine_labels(c, lang))
                    .unwrap_or_default(),
                opening_hours: p
                    .opening_hours
                    .as_deref()
                    .map(|raw| opening_hours_payload(raw, lang)),
                cuisine: p.cuisine.clone(),
                wikidata: p.wikidata.clone(),
            }
        })
        .collect();

    let response = PlacesResponse {
        t_unix: t,
        sun_azimuth_deg: sun.azimuth_deg,
        sun_elevation_deg: sun.elevation_deg,
        count: places.len(),
        places,
    };
    state
        .places_results
        .write()
        .await
        .insert(result_key, Arc::new(response.clone()));
    Ok(Json(response))
}

// ---------------------------------------------------------------- arbres

#[derive(Deserialize)]
struct TreesQuery {
    /// `min_lon,min_lat,max_lon,max_lat`
    bbox: String,
}

#[derive(Serialize)]
struct TreesResponse {
    count: usize,
    trees: Vec<Tree>,
}

#[derive(Serialize, Clone)]
struct Tree {
    /// Identifiant OSM ("node/123"). Sert au client à dédoublonner les arbres
    /// entre deux bbox qui se recouvrent, et à cibler leur suppression quand
    /// il purge son cache.
    id: String,
    lat: f64,
    lng: f64,
    height_m: f64,
    crown_radius_m: f64,
}

/// Arbres OSM (`natural=tree`) de la zone — aucun calcul soleil/ombre ici
/// (le rendu/l'extrusion restent côté client), juste la géométrie servie
/// depuis PostGIS.
async fn trees(
    State(state): State<Arc<AppState>>,
    Query(q): Query<TreesQuery>,
) -> Result<Json<TreesResponse>, (StatusCode, String)> {
    let parts: Vec<f64> = q
        .bbox
        .split(',')
        .filter_map(|s| s.trim().parse().ok())
        .collect();
    let [w, s, e, n] = parts[..] else {
        return Err((
            StatusCode::BAD_REQUEST,
            "bbox attendue : min_lon,min_lat,max_lon,max_lat".into(),
        ));
    };
    if s >= n || w >= e || !(-85.0..=85.0).contains(&s) || !(-85.0..=85.0).contains(&n) {
        return Err((StatusCode::BAD_REQUEST, "bbox invalide".into()));
    }

    let trees: Vec<Tree> = db::trees_in_bbox(&state.pool, s, w, n, e)
        .await
        .map_err(|err| (StatusCode::INTERNAL_SERVER_ERROR, format!("PostGIS : {err}")))?
        .into_iter()
        .map(|t| Tree {
            id: t.osm_id,
            lat: t.lat,
            lng: t.lng,
            height_m: t.height_m,
            crown_radius_m: t.crown_radius_m,
        })
        .collect();
    Ok(Json(TreesResponse {
        count: trees.len(),
        trees,
    }))
}


fn parse_time(raw: Option<&str>) -> Result<f64, (StatusCode, String)> {
    let Some(raw) = raw else {
        return Ok(chrono::Utc::now().timestamp() as f64);
    };
    if let Ok(unix) = raw.parse::<f64>() {
        return Ok(unix);
    }
    chrono::DateTime::parse_from_rfc3339(raw)
        .map(|d| d.timestamp() as f64)
        .map_err(|_| {
            (
                StatusCode::BAD_REQUEST,
                format!("t invalide : {raw} (RFC3339 ou secondes Unix)"),
            )
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Une journée doit commencer à minuit LOCAL. Le bug d'origine découpait
    /// sur UTC : à Paris en été, un instant à 00h30 locale (22:30Z la veille)
    /// renvoyait la journée précédente, bornée à 02:00 → 02:00.
    #[test]
    fn day_bounds_follow_local_midnight() {
        // 2026-07-25T22:30:00Z = 2026-07-26 00:30 à Paris (UTC+2).
        let t = 1785018600.0;
        let (start, end) = day_bounds_local(t, 120);

        // Minuit local = 22:00Z la veille.
        assert_eq!(start, 1785016800.0);
        assert_eq!(end - start, 86_400.0);
        // L'instant demandé tombe bien dans la journée renvoyée.
        assert!(start <= t && t < end);
    }

    /// Sans décalage, on retombe sur la journée UTC — l'ancien comportement,
    /// conservé comme défaut pour ne pas casser un appel sans paramètre.
    #[test]
    fn zero_offset_is_utc_day() {
        let t = 1785018600.0; // 2026-07-25T22:30:00Z
        let (start, _) = day_bounds_local(t, 0);
        assert_eq!(start, 1784937600.0); // 2026-07-25T00:00:00Z
    }

    /// Fuseaux négatifs (Amériques) : même règle, minuit local.
    #[test]
    fn negative_offset_works() {
        // 2026-07-26T03:00:00Z = 2026-07-25 23:00 à New York (UTC-4).
        let t = 1785034800.0;
        let (start, end) = day_bounds_local(t, -240);
        assert!(start <= t && t < end);
        // Minuit à New York le 25 = 04:00Z le 25.
        assert_eq!(start, 1784952000.0);
    }
}

// -------------------------------------------------------------- comptes

/// Vérifie le jeton porté par la requête, ou refuse.
async fn authenticate(
    state: &Arc<AppState>,
    headers: &HeaderMap,
) -> Result<auth::Identity, (StatusCode, String)> {
    let header = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok());
    state
        .auth
        .verify_header(header)
        .await
        .map_err(|e| (e.status(), e.message()))
}

#[derive(Serialize)]
struct UserResponse {
    uid: String,
    /// Absent tant que l'utilisateur n'a pas choisi son pseudo.
    #[serde(skip_serializing_if = "Option::is_none")]
    username: Option<String>,
}

async fn current_user(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Result<Json<UserResponse>, (StatusCode, String)> {
    let identity = authenticate(&state, &headers).await?;
    let record = db::user_by_uid(&state.pool, &identity.uid)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("PostGIS : {e}")))?;
    Ok(Json(UserResponse {
        uid: identity.uid,
        username: record.map(|r| r.username),
    }))
}

#[derive(Deserialize)]
struct UsernameBody {
    username: String,
}

async fn set_username(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(body): Json<UsernameBody>,
) -> Result<Json<UserResponse>, (StatusCode, String)> {
    let identity = authenticate(&state, &headers).await?;
    let username = username::validate(&body.username)
        .map_err(|e| (StatusCode::BAD_REQUEST, e.message()))?;

    match db::set_username(&state.pool, &identity.uid, &username).await {
        Ok(Ok(record)) => Ok(Json(UserResponse {
            uid: record.uid,
            username: Some(record.username),
        })),
        // 409 et non 400 : la demande est bien formée, c'est l'état du monde qui
        // s'y oppose — et il peut s'y opposer entre deux tentatives identiques.
        Ok(Err(_)) => Err((StatusCode::CONFLICT, "pseudo déjà pris".into())),
        Err(e) => Err((StatusCode::INTERNAL_SERVER_ERROR, format!("PostGIS : {e}"))),
    }
}

#[derive(Serialize)]
struct SuggestionsResponse {
    suggestions: Vec<String>,
}

/// Quatre pseudos libres à proposer à la saisie.
///
/// Le nom d'affichage du fournisseur passe en premier quand il donne quelque
/// chose d'exploitable : « KarlGochgarian » se reconnaît mieux que
/// « SunLover3448 ». Le reste vient de la liste thématique.
async fn username_suggestions(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Result<Json<SuggestionsResponse>, (StatusCode, String)> {
    const WANTED: usize = 4;

    let identity = authenticate(&state, &headers).await?;

    let mut pool_of_candidates = Vec::new();
    if let Some(from_name) = identity.display_name.as_deref().and_then(username::from_display_name) {
        pool_of_candidates.push(from_name);
    }
    // Large marge : les pseudos pris sont écartés ensuite, et repartir en base
    // pour compléter coûterait un aller-retour de plus.
    let seed = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(1);
    pool_of_candidates.extend(username::candidates(seed, WANTED * 4));

    let taken = db::taken_usernames(&state.pool, &pool_of_candidates)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("PostGIS : {e}")))?;

    let suggestions: Vec<String> = pool_of_candidates
        .into_iter()
        .filter(|c| !taken.contains(&c.to_lowercase()))
        .take(WANTED)
        .collect();
    Ok(Json(SuggestionsResponse { suggestions }))
}
