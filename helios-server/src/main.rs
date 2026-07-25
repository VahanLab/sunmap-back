//! Serveur de query ensoleillement.
//!
//! - `GET /sunlit?lat=&lng=[&t=][&observer_height=]` : le point est-il au
//!   soleil à l'instant t ? (t = RFC3339 ou secondes Unix, défaut maintenant)
//! - `POST /sunlit/batch` : même question pour une liste de points
//!   (classification de terrasses).
//!
//! DSM : tuiles Mapterhorn (webp 512 px, encodage Terrarium — la même source
//! que l'app iOS), assemblées 3×3 autour du point pour donner de la marge aux
//! casters (ombres portées venant de l'extérieur de la tuile centrale).

use std::collections::HashMap;
use std::sync::Arc;

use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;

use helios_core::dsm::Dsm;
use helios_core::shadow::{is_shadowed_from_ground, ShadowParams};
use helios_core::sun::sun_position;

const TILE_SIZE: usize = 512;
/// z15 ≈ 2,4 m/pixel à 45° de latitude : suffisant pour du relief et des
/// terrasses. (z16 dispo si besoin de plus fin, 4× plus de données.)
const ZOOM: u32 = 15;
const TILE_URL: &str = "https://tiles.mapterhorn.com";

type TileCache = RwLock<HashMap<(u32, u32, u32), Arc<Vec<f32>>>>;
type PoiCache = RwLock<HashMap<String, Arc<Vec<OverpassPoi>>>>;
type BuildingCache = RwLock<HashMap<String, Arc<Vec<Building>>>>;

/// Hauteur par défaut quand ni `height` ni `building:levels` ne sont taggés
/// sur OSM (fréquent) — ~3 étages, prudent en zone dense plutôt que de sous-
/// estimer les ombres portées.
const DEFAULT_BUILDING_HEIGHT_M: f32 = 9.0;

struct AppState {
    http: reqwest::Client,
    tiles: TileCache,
    pois: PoiCache,
    buildings: BuildingCache,
}

#[tokio::main]
async fn main() {
    let state = Arc::new(AppState {
        // User-Agent identifiable : requis par Overpass (406 sinon).
        http: reqwest::Client::builder()
            .user_agent("sunmap-helios/0.1 (+https://github.com/VahanLab/sunmap-back)")
            .timeout(std::time::Duration::from_secs(20))
            .build()
            .expect("client HTTP"),
        tiles: RwLock::new(HashMap::new()),
        pois: RwLock::new(HashMap::new()),
        buildings: RwLock::new(HashMap::new()),
    });

    let app = Router::new()
        .route("/sunlit", get(sunlit))
        .route("/sunlit/batch", post(sunlit_batch))
        .route("/terraces", get(terraces))
        .route("/sun-hours", get(sun_hours))
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

/// Pixel monde Web Mercator au zoom de travail.
fn world_px(lat: f64, lng: f64) -> (f64, f64) {
    let n = (TILE_SIZE as f64) * f64::powi(2.0, ZOOM as i32);
    let wx = (lng + 180.0) / 360.0 * n;
    let wy = (1.0 - lat.to_radians().tan().asinh() / std::f64::consts::PI) / 2.0 * n;
    (wx, wy)
}

/// Inverse de `world_px` : pixel monde → coordonnée géographique.
fn latlon_of_world_px(wx: f64, wy: f64) -> (f64, f64) {
    let n = (TILE_SIZE as f64) * f64::powi(2.0, ZOOM as i32);
    let lon = wx / n * 360.0 - 180.0;
    let lat = (std::f64::consts::PI * (1.0 - 2.0 * wy / n))
        .sinh()
        .atan()
        .to_degrees();
    (lat, lon)
}

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
fn stamp_buildings(dsm: &mut Dsm, origin_x: f64, origin_y: f64, buildings: &[Building]) {
    for b in buildings {
        if b.ring.len() < 3 {
            continue;
        }
        let pixels: Vec<(f64, f64)> = b
            .ring
            .iter()
            .map(|&(lat, lon)| {
                let (wx, wy) = world_px(lat, lon);
                (wx - origin_x, wy - origin_y)
            })
            .collect();

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
        let cx = ((min_x + max_x) / 2.0).clamp(0.0, dsm.width as f64 - 1.0);
        let cy = ((min_y + max_y) / 2.0).clamp(0.0, dsm.height as f64 - 1.0);
        let target = dsm.sample(cx, cy).unwrap_or(0.0) + b.height_m;

        let y0 = min_y.max(0.0).floor() as usize;
        let y1 = max_y.min(dsm.height as f64 - 1.0).ceil() as usize;
        for y in y0..=y1.min(dsm.height - 1) {
            let scan_y = y as f64 + 0.5;
            let mut xs: Vec<f64> = Vec::new();
            for i in 0..pixels.len() {
                let (x1, y1p) = pixels[i];
                let (x2, y2p) = pixels[(i + 1) % pixels.len()];
                if (y1p <= scan_y && y2p > scan_y) || (y2p <= scan_y && y1p > scan_y) {
                    let t = (scan_y - y1p) / (y2p - y1p);
                    xs.push(x1 + t * (x2 - x1));
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
) -> Result<(), (StatusCode, String)> {
    let (north, west) = latlon_of_world_px(origin_x, origin_y);
    let (south, east) = latlon_of_world_px(origin_x + dsm.width as f64, origin_y + dsm.height as f64);
    let buildings = overpass_buildings(state, south, west, north, east).await?;
    stamp_buildings(dsm, origin_x, origin_y, &buildings);
    Ok(())
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
) -> Result<(Dsm, f64, f64, f32), (StatusCode, String)> {
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
    add_buildings(state, &mut dsm, origin_x, origin_y).await?;

    Ok((dsm, px, py, ground))
}

async fn classify(
    state: &AppState,
    lat: f64,
    lng: f64,
    t_unix: f64,
    observer_height_m: f64,
) -> Result<SunlitResponse, (StatusCode, String)> {
    let sun = sun_position(t_unix, lat, lng);
    let (dsm, px, py, elevation_m) = assemble_point(state, lat, lng).await?;

    let params = ShadowParams {
        max_distance_m: 5_000.0, // relief : ombres longues possibles
        observer_height_m,
        step_px: 1.0,
    };
    let shadowed = is_shadowed_from_ground(&dsm, &sun, px, py, elevation_m, &params);

    Ok(SunlitResponse {
        sunlit: !shadowed,
        elevation_m,
        sun_azimuth_deg: sun.azimuth_deg,
        sun_elevation_deg: sun.elevation_deg,
        t_unix,
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
}

#[derive(Serialize)]
struct SunHoursResponse {
    lat: f64,
    lng: f64,
    elevation_m: f32,
    /// Instant demandé et sa classification, pour un statut "maintenant"
    /// immédiat sans avoir à chercher dans `intervals`.
    t_unix: f64,
    sunlit_now: bool,
    day_start_unix: f64,
    day_end_unix: f64,
    total_sunlit_minutes: u32,
    total_shadow_minutes: u32,
    intervals: Vec<SunInterval>,
}

#[derive(Serialize)]
struct SunInterval {
    start_unix: f64,
    end_unix: f64,
    sunlit: bool,
}

/// Journée calendaire UTC (00:00 → 24:00) contenant l'instant donné.
fn day_bounds_utc(t_unix: f64) -> (f64, f64) {
    let dt = chrono::DateTime::from_timestamp(t_unix as i64, 0).unwrap_or_default();
    let start = dt.date_naive().and_hms_opt(0, 0, 0).unwrap().and_utc();
    let start_unix = start.timestamp() as f64;
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
    let (day_start, day_end) = day_bounds_utc(t);

    let (dsm, px, py, elevation_m) = assemble_point(&state, q.lat, q.lng).await?;
    let params = ShadowParams {
        max_distance_m: 5_000.0,
        observer_height_m: h,
        step_px: 1.0,
    };

    const STEP_S: f64 = 300.0; // 5 min
    let steps = (86_400.0 / STEP_S) as usize;

    let mut intervals: Vec<SunInterval> = Vec::new();
    let mut sunlit_now = false;
    let mut total_sunlit_steps: u32 = 0;

    for i in 0..steps {
        let step_t = day_start + i as f64 * STEP_S;
        let sun = sun_position(step_t, q.lat, q.lng);
        let sunlit = sun.is_up() && !is_shadowed_from_ground(&dsm, &sun, px, py, elevation_m, &params);
        if sunlit {
            total_sunlit_steps += 1;
        }
        if step_t <= t && t < step_t + STEP_S {
            sunlit_now = sunlit;
        }

        match intervals.last_mut() {
            Some(last) if last.sunlit == sunlit => last.end_unix = step_t + STEP_S,
            _ => intervals.push(SunInterval {
                start_unix: step_t,
                end_unix: step_t + STEP_S,
                sunlit,
            }),
        }
    }

    let total_sunlit_minutes = total_sunlit_steps * 5;
    let total_shadow_minutes = (steps as u32 - total_sunlit_steps) * 5;

    Ok(Json(SunHoursResponse {
        lat: q.lat,
        lng: q.lng,
        elevation_m,
        t_unix: t,
        sunlit_now,
        day_start_unix: day_start,
        day_end_unix: day_end,
        total_sunlit_minutes,
        total_shadow_minutes,
        intervals,
    }))
}

/// Tuile Mapterhorn décodée en altitudes (cache mémoire).
async fn fetch_tile(
    state: &AppState,
    z: u32,
    x: u32,
    y: u32,
) -> Result<Arc<Vec<f32>>, (StatusCode, String)> {
    if let Some(hit) = state.tiles.read().await.get(&(z, x, y)) {
        return Ok(hit.clone());
    }

    let url = format!("{TILE_URL}/{z}/{x}/{y}.webp");
    let bytes = state
        .http
        .get(&url)
        .send()
        .await
        .and_then(|r| r.error_for_status())
        .map_err(|e| (StatusCode::BAD_GATEWAY, format!("tuile {url} : {e}")))?
        .bytes()
        .await
        .map_err(|e| (StatusCode::BAD_GATEWAY, format!("tuile {url} : {e}")))?;

    let img = image::load_from_memory(&bytes)
        .map_err(|e| (StatusCode::BAD_GATEWAY, format!("décodage {url} : {e}")))?
        .to_rgb8();
    if img.width() as usize != TILE_SIZE || img.height() as usize != TILE_SIZE {
        return Err((
            StatusCode::BAD_GATEWAY,
            format!("tuile {url} : taille inattendue {}×{}", img.width(), img.height()),
        ));
    }

    // Même décodage que Dsm::from_terrarium_rgb (on ne garde que les floats).
    let floats = Dsm::from_terrarium_rgb(img.as_raw(), TILE_SIZE, TILE_SIZE, 1.0).data;
    let arc = Arc::new(floats);
    state.tiles.write().await.insert((z, x, y), arc.clone());
    Ok(arc)
}

// ------------------------------------------------------------- terrasses

#[derive(Deserialize)]
struct TerracesQuery {
    /// `min_lon,min_lat,max_lon,max_lat`
    bbox: String,
    t: Option<String>,
    /// Défaut 1,5 m : personne attablée en terrasse.
    observer_height: Option<f64>,
}

#[derive(Serialize)]
struct TerracesResponse {
    t_unix: f64,
    sun_azimuth_deg: f64,
    sun_elevation_deg: f64,
    count: usize,
    terraces: Vec<Terrace>,
}

#[derive(Serialize)]
struct Terrace {
    /// Identifiant OSM, ex. "node/123456" ou "way/789".
    id: String,
    name: Option<String>,
    amenity: Option<String>,
    lat: f64,
    lng: f64,
    sunlit: bool,
    elevation_m: f32,
    /// Champs OSM optionnels — couverture très inégale selon les POI.
    website: Option<String>,
    phone: Option<String>,
    opening_hours: Option<String>,
    cuisine: Option<String>,
    /// Identifiant Wikidata (ex. "Q123456") : présent sur ~15% des POI,
    /// utilisable côté client pour aller chercher une photo (propriété P18)
    /// via l'API Wikidata/Commons — OSM ne stocke pas de photos lui-même.
    wikidata: Option<String>,
}

/// POI brut issu d'Overpass (centroïde pour les ways/relations).
#[derive(Clone)]
struct OverpassPoi {
    id: String,
    name: Option<String>,
    amenity: Option<String>,
    lat: f64,
    lng: f64,
    website: Option<String>,
    phone: Option<String>,
    opening_hours: Option<String>,
    cuisine: Option<String>,
    wikidata: Option<String>,
}

/// Bars/restaurants/cafés avec terrasse dans la bbox, classés soleil/ombre.
///
/// Limite assumée (POC) : classification binaire au centroïde — une terrasse
/// est un polygone, potentiellement mi-ombre mi-soleil. Prochaine étape :
/// échantillonner 3-5 points dans un buffer côté rue et renvoyer un %.
async fn terraces(
    State(state): State<Arc<AppState>>,
    Query(q): Query<TerracesQuery>,
) -> Result<Json<TerracesResponse>, (StatusCode, String)> {
    let t = parse_time(q.t.as_deref())?;
    let h = q.observer_height.unwrap_or(1.5);

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

    let pois = overpass_terraces(&state, s, w, n, e).await?;
    let (mut dsm, origin_x, origin_y) = assemble_grid(&state, tx0, ty0, tx1, ty1, (s + n) / 2.0).await?;
    // Relief seul, avant stamping des bâtiments : sert de sol pour chaque
    // terrasse (cf. commentaire dans classify() — un POI mal placé dans un
    // bâtiment côté OSM ne doit pas hériter de l'altitude du toit).
    let terrain_only = dsm.clone();
    add_buildings(&state, &mut dsm, origin_x, origin_y).await?;

    let mid_lat = (s + n) / 2.0;
    let mid_lng = (w + e) / 2.0;
    let sun = sun_position(t, mid_lat, mid_lng);
    let params = ShadowParams {
        max_distance_m: 5_000.0,
        observer_height_m: h,
        step_px: 1.0,
    };

    let terraces: Vec<Terrace> = pois
        .iter()
        .map(|p| {
            let (wx, wy) = world_px(p.lat, p.lng);
            let px = wx - origin_x;
            let py = wy - origin_y;
            let ground = terrain_only.sample(px, py).unwrap_or(0.0);
            Terrace {
                id: p.id.clone(),
                name: p.name.clone(),
                amenity: p.amenity.clone(),
                lat: p.lat,
                lng: p.lng,
                sunlit: sun.is_up() && !is_shadowed_from_ground(&dsm, &sun, px, py, ground, &params),
                elevation_m: ground,
                website: p.website.clone(),
                phone: p.phone.clone(),
                opening_hours: p.opening_hours.clone(),
                cuisine: p.cuisine.clone(),
                wikidata: p.wikidata.clone(),
            }
        })
        .collect();

    Ok(Json(TerracesResponse {
        t_unix: t,
        sun_azimuth_deg: sun.azimuth_deg,
        sun_elevation_deg: sun.elevation_deg,
        count: terraces.len(),
        terraces,
    }))
}

/// Miroirs Overpass, essayés dans l'ordre. L'instance officielle
/// (overpass-api.de) sature souvent (504) aux heures de pointe.
const OVERPASS_MIRRORS: &[&str] = &[
    "https://overpass-api.de/api/interpreter",
    "https://overpass.kumi.systems/api/interpreter",
    "https://overpass.private.coffee/api/interpreter",
];

/// Requête Overpass générique : essaie chaque miroir dans l'ordre, renvoie
/// le premier succès. Partagé par POI terrasses et emprises bâtiments.
async fn overpass_query<T: serde::de::DeserializeOwned>(
    state: &AppState,
    query: &str,
) -> Result<T, (StatusCode, String)> {
    let mut last_err = String::new();
    for mirror in OVERPASS_MIRRORS {
        match state
            .http
            .post(*mirror)
            .form(&[("data", query)])
            .send()
            .await
            .and_then(|r| r.error_for_status())
        {
            Ok(resp) => match resp.json::<T>().await {
                Ok(parsed) => return Ok(parsed),
                Err(e) => last_err = format!("{mirror} (JSON) : {e}"),
            },
            Err(e) => last_err = format!("{mirror} : {e}"),
        }
    }
    Err((StatusCode::BAD_GATEWAY, format!("Overpass : {last_err}")))
}

/// Fetch Overpass des POI terrasse (cache mémoire par bbox arrondie —
/// les POI bougent rarement, l'API publique est lente/instable : on
/// essaie plusieurs miroirs avant d'abandonner).
async fn overpass_terraces(
    state: &AppState,
    s: f64,
    w: f64,
    n: f64,
    e: f64,
) -> Result<Arc<Vec<OverpassPoi>>, (StatusCode, String)> {
    // DEBUG : clé fixe, un seul fetch Overpass pour toute la durée de vie du
    // process, peu importe la bbox demandée — évite de re-solliciter
    // Overpass à chaque test iOS sur la même zone. Le premier appel fait
    // foi pour la zone couverte ; à retirer avant tout usage au-delà du dev
    // local (des points hors de cette zone n'auront jamais leurs propres
    // terrasses tant que le process tourne).
    let key = "DEBUG_ALL".to_string();
    if let Some(hit) = state.pois.read().await.get(&key) {
        return Ok(hit.clone());
    }

    let query = format!(
        r#"[out:json][timeout:25];
nwr["amenity"~"^(bar|restaurant|cafe)$"]["outdoor_seating"="yes"]({s},{w},{n},{e});
out center 500;"#
    );
    let raw: OverpassResponse = overpass_query(state, &query).await?;

    let pois: Vec<OverpassPoi> = raw
        .elements
        .into_iter()
        .filter_map(|el| {
            let (lat, lng) = match (el.lat, el.lon, &el.center) {
                (Some(la), Some(lo), _) => (la, lo),
                (_, _, Some(c)) => (c.lat, c.lon),
                _ => return None,
            };
            let tags = el.tags.unwrap_or_default();
            // Website/téléphone : "contact:*" en repli si le tag simple est absent.
            let website = tags
                .get("website")
                .or_else(|| tags.get("contact:website"))
                .cloned();
            let phone = tags
                .get("phone")
                .or_else(|| tags.get("contact:phone"))
                .cloned();
            Some(OverpassPoi {
                id: format!("{}/{}", el.element_type, el.id),
                name: tags.get("name").cloned(),
                amenity: tags.get("amenity").cloned(),
                lat,
                lng,
                website,
                phone,
                opening_hours: tags.get("opening_hours").cloned(),
                cuisine: tags.get("cuisine").cloned(),
                wikidata: tags.get("wikidata").cloned(),
            })
        })
        .collect();

    let arc = Arc::new(pois);
    state.pois.write().await.insert(key, arc.clone());
    Ok(arc)
}

/// Emprise rectangulaire d'un bâtiment (bbox de son empreinte OSM) + hauteur.
#[derive(Clone)]
struct Building {
    /// Anneau extérieur du polygone (lat, lon), tel que renvoyé par Overpass.
    ring: Vec<(f64, f64)>,
    height_m: f32,
}

/// Fetch Overpass des bâtiments (`building=*`) de la zone, cache mémoire par
/// bbox arrondie. `out geom` renvoie directement la géométrie (liste de
/// nœuds) sans second aller-retour pour résoudre les ways.
async fn overpass_buildings(
    state: &AppState,
    s: f64,
    w: f64,
    n: f64,
    e: f64,
) -> Result<Arc<Vec<Building>>, (StatusCode, String)> {
    // DEBUG : même principe que overpass_terraces — clé fixe, un seul fetch
    // pour toute la durée de vie du process. Seul le calcul soleil/ombre
    // (dépendant de `t`) doit varier entre requêtes ; la géométrie
    // (bâtiments, relief) est figée sur la zone du premier appel.
    let key = "DEBUG_ALL".to_string();
    if let Some(hit) = state.buildings.read().await.get(&key) {
        return Ok(hit.clone());
    }

    let query = format!(
        r#"[out:json][timeout:25];
way["building"]({s},{w},{n},{e});
out geom 5000;"#
    );
    let raw: BuildingsResponse = overpass_query(state, &query).await?;

    let buildings: Vec<Building> = raw
        .elements
        .into_iter()
        .filter_map(|el| {
            let geometry = el.geometry?;
            if geometry.len() < 3 {
                return None;
            }
            let ring: Vec<(f64, f64)> = geometry.iter().map(|n| (n.lat, n.lon)).collect();
            let tags = el.tags.unwrap_or_default();
            let height_m = tags
                .get("height")
                .and_then(|h| h.trim_end_matches(" m").parse::<f32>().ok())
                .or_else(|| {
                    tags.get("building:levels")
                        .and_then(|l| l.parse::<f32>().ok())
                        .map(|levels| levels * 3.0)
                })
                .unwrap_or(DEFAULT_BUILDING_HEIGHT_M);
            Some(Building { ring, height_m })
        })
        .collect();

    let arc = Arc::new(buildings);
    state.buildings.write().await.insert(key, arc.clone());
    Ok(arc)
}

#[derive(Deserialize)]
struct BuildingsResponse {
    elements: Vec<BuildingElement>,
}

#[derive(Deserialize)]
struct BuildingElement {
    tags: Option<HashMap<String, String>>,
    geometry: Option<Vec<GeomNode>>,
}

#[derive(Deserialize)]
struct GeomNode {
    lat: f64,
    lon: f64,
}

#[derive(Deserialize)]
struct OverpassResponse {
    elements: Vec<OverpassElement>,
}

#[derive(Deserialize)]
struct OverpassElement {
    #[serde(rename = "type")]
    element_type: String,
    id: u64,
    lat: Option<f64>,
    lon: Option<f64>,
    center: Option<OverpassCenter>,
    tags: Option<HashMap<String, String>>,
}

#[derive(Deserialize)]
struct OverpassCenter {
    lat: f64,
    lon: f64,
}

/// "1753455600", "1753455600.5" ou RFC3339. Absent → maintenant.
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
