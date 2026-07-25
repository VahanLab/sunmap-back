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
use helios_core::shadow::{is_shadowed, ShadowParams};
use helios_core::sun::sun_position;

const TILE_SIZE: usize = 512;
/// z15 ≈ 2,4 m/pixel à 45° de latitude : suffisant pour du relief et des
/// terrasses. (z16 dispo si besoin de plus fin, 4× plus de données.)
const ZOOM: u32 = 15;
const TILE_URL: &str = "https://tiles.mapterhorn.com";

type TileCache = RwLock<HashMap<(u32, u32, u32), Arc<Vec<f32>>>>;
type PoiCache = RwLock<HashMap<String, Arc<Vec<OverpassPoi>>>>;

struct AppState {
    http: reqwest::Client,
    tiles: TileCache,
    pois: PoiCache,
}

#[tokio::main]
async fn main() {
    let state = Arc::new(AppState {
        // User-Agent identifiable : requis par Overpass (406 sinon).
        http: reqwest::Client::builder()
            .user_agent("sunmap-helios/0.1 (+https://github.com/VahanLab/sunmap-back)")
            .build()
            .expect("client HTTP"),
        tiles: RwLock::new(HashMap::new()),
        pois: RwLock::new(HashMap::new()),
    });

    let app = Router::new()
        .route("/sunlit", get(sunlit))
        .route("/sunlit/batch", post(sunlit_batch))
        .route("/terraces", get(terraces))
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

/// Assemble la DSM 3×3 tuiles autour du point et interroge helios-core.
async fn classify(
    state: &AppState,
    lat: f64,
    lng: f64,
    t_unix: f64,
    observer_height_m: f64,
) -> Result<SunlitResponse, (StatusCode, String)> {
    if !(-85.0..=85.0).contains(&lat) || !(-180.0..=180.0).contains(&lng) {
        return Err((StatusCode::BAD_REQUEST, "lat/lng hors bornes".into()));
    }

    let sun = sun_position(t_unix, lat, lng);

    // Tuile centrale + marge d'une tuile ≈ 1,2 km de casters à z15.
    let (wx, wy) = world_px(lat, lng);
    let tx = (wx / TILE_SIZE as f64) as u32;
    let ty = (wy / TILE_SIZE as f64) as u32;
    let max_tile = (1u32 << ZOOM) - 1;
    let (dsm, origin_x, origin_y) = assemble_grid(
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
    let elevation_m = dsm.sample(px, py).unwrap_or(0.0);

    let params = ShadowParams {
        max_distance_m: 5_000.0, // relief : ombres longues possibles
        observer_height_m,
        step_px: 1.0,
    };
    let shadowed = is_shadowed(&dsm, &sun, px, py, &params);

    Ok(SunlitResponse {
        sunlit: !shadowed,
        elevation_m,
        sun_azimuth_deg: sun.azimuth_deg,
        sun_elevation_deg: sun.elevation_deg,
        t_unix,
    })
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
}

/// POI brut issu d'Overpass (centroïde pour les ways/relations).
#[derive(Clone)]
struct OverpassPoi {
    id: String,
    name: Option<String>,
    amenity: Option<String>,
    lat: f64,
    lng: f64,
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
    let (dsm, origin_x, origin_y) = assemble_grid(&state, tx0, ty0, tx1, ty1, (s + n) / 2.0).await?;

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
            Terrace {
                id: p.id.clone(),
                name: p.name.clone(),
                amenity: p.amenity.clone(),
                lat: p.lat,
                lng: p.lng,
                sunlit: sun.is_up() && !is_shadowed(&dsm, &sun, px, py, &params),
                elevation_m: dsm.sample(px, py).unwrap_or(0.0),
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

/// Fetch Overpass des POI terrasse (cache mémoire par bbox arrondie —
/// les POI bougent rarement, l'API publique est lente).
async fn overpass_terraces(
    state: &AppState,
    s: f64,
    w: f64,
    n: f64,
    e: f64,
) -> Result<Arc<Vec<OverpassPoi>>, (StatusCode, String)> {
    let key = format!("{s:.4},{w:.4},{n:.4},{e:.4}");
    if let Some(hit) = state.pois.read().await.get(&key) {
        return Ok(hit.clone());
    }

    let query = format!(
        r#"[out:json][timeout:25];
nwr["amenity"~"^(bar|restaurant|cafe)$"]["outdoor_seating"="yes"]({s},{w},{n},{e});
out center 500;"#
    );
    let raw: OverpassResponse = state
        .http
        .post("https://overpass-api.de/api/interpreter")
        .form(&[("data", query.as_str())])
        .send()
        .await
        .and_then(|r| r.error_for_status())
        .map_err(|e| (StatusCode::BAD_GATEWAY, format!("Overpass : {e}")))?
        .json()
        .await
        .map_err(|e| (StatusCode::BAD_GATEWAY, format!("Overpass JSON : {e}")))?;

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
            Some(OverpassPoi {
                id: format!("{}/{}", el.element_type, el.id),
                name: tags.get("name").cloned(),
                amenity: tags.get("amenity").cloned(),
                lat,
                lng,
            })
        })
        .collect();

    let arc = Arc::new(pois);
    state.pois.write().await.insert(key, arc.clone());
    Ok(arc)
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
