//! Routes d'exposition annuelle — module du **binaire** (handlers axum), qui
//! branche les modules purs `facade` et `exposure` sur le moteur du serveur :
//! même DSM (`assemble_grid` + `add_buildings`), même ray marching
//! (`shadow_hit_from_ground`), mêmes paramètres que `/places`.
//!
//! - `GET /buildings/at` : le bâtiment sous le doigt et ses façades (léger,
//!   sans DSM ni rayon).
//! - `GET /exposure/facade` : grille étages × tronçons d'une façade, profil
//!   annuel complet par case.
//! - `GET /exposure/point` : profil annuel d'un point au sol, ou d'un lieu
//!   (`place=`) à la position exacte de `/places`.
//!
//! Spec : `docs/superpowers/specs/2026-11-01-exposition-immobiliere-design.md`.
//! Trois routes **nouvelles** : aucune version d'app en ligne ne les appelle,
//! leurs codes d'erreur ne changent aucun contrat existant.

use std::collections::VecDeque;
use std::sync::Mutex;

use axum::http::header;
use axum::response::{IntoResponse, Response};
use chrono_tz::Tz;
use helios_core::sun::SunPosition;
use helios_server::exposure::{
    self, build_profile, sampled_days, ExposureProfile, Sample, SampledDay, DEFAULT_DAY_STRIDE,
    REFERENCE_YEAR,
};
use helios_server::facade::{self, Facade};
use helios_server::vtiles::VectorStore;

use super::*;

type ApiError = (StatusCode, String);

/// Le résultat ne dépend que des entrées (année de référence fixe) : un jour
/// de cache côté CDN ne risque rien, un import redémarre de toute façon le
/// serveur.
const CACHE_CONTROL: &str = "public, max-age=86400";
/// Hauteur d'observation d'un lieu : celle de `/places` par défaut.
const PLACE_OBSERVER_HEIGHT_M: f64 = 1.5;
const MAX_POINT_HEIGHT_M: f64 = 200.0;
/// Rayon de recherche du bâtiment autour du point tapé (~30 m) : l'archive
/// renvoie les objets entiers, il suffit d'atteindre les tuiles qui le portent.
const PICK_RADIUS_DEG: f64 = 0.0003;
/// Marge autour du bâtiment pour lire ses voisins (murs mitoyens).
const NEIGHBOUR_MARGIN_DEG: f64 = 0.0001;
const CACHE_CAPACITY: usize = 2_000;

// ------------------------------------------------------------------ cache

/// Réponses déjà calculées, sérialisées. Borné, éviction FIFO : un profil de
/// façade pèse ~60 Ko, 2 000 entrées tiennent dans ~120 Mo au pire.
#[derive(Default)]
pub(crate) struct ExposureCache {
    inner: Mutex<(HashMap<String, Arc<String>>, VecDeque<String>)>,
}

impl ExposureCache {
    fn get(&self, key: &str) -> Option<Arc<String>> {
        self.inner.lock().ok()?.0.get(key).cloned()
    }

    fn put(&self, key: String, body: Arc<String>) {
        let Ok(mut guard) = self.inner.lock() else { return };
        let (map, order) = &mut *guard;
        if map.insert(key.clone(), body).is_none() {
            order.push_back(key);
        }
        while order.len() > CACHE_CAPACITY {
            if let Some(old) = order.pop_front() {
                map.remove(&old);
            }
        }
    }
}

fn json_response(body: Arc<String>) -> Response {
    (
        [(header::CONTENT_TYPE, "application/json"), (header::CACHE_CONTROL, CACHE_CONTROL)],
        body.as_str().to_owned(),
    )
        .into_response()
}

// -------------------------------------------------------------- validation

fn check_coords(lat: f64, lng: f64) -> Result<(), ApiError> {
    if lat.is_finite() && lng.is_finite() && (-85.0..=85.0).contains(&lat) && (-180.0..=180.0).contains(&lng) {
        Ok(())
    } else {
        Err((StatusCode::BAD_REQUEST, "lat/lng hors bornes".into()))
    }
}

fn check_levels(levels: Option<u32>) -> Result<Option<u32>, ApiError> {
    match levels {
        Some(l) if !(1..=facade::MAX_LEVELS).contains(&l) => Err((
            StatusCode::BAD_REQUEST,
            format!("levels attendu entre 1 et {}", facade::MAX_LEVELS),
        )),
        other => Ok(other),
    }
}

fn check_height(height: Option<f64>) -> Result<f64, ApiError> {
    let h = height.unwrap_or(PLACE_OBSERVER_HEIGHT_M);
    if h.is_finite() && (0.0..=MAX_POINT_HEIGHT_M).contains(&h) {
        Ok(h)
    } else {
        Err((StatusCode::BAD_REQUEST, format!("height attendu entre 0 et {MAX_POINT_HEIGHT_M} m")))
    }
}

fn check_tz(raw: Option<&str>) -> Result<Tz, ApiError> {
    exposure::parse_tz(raw).map_err(|e| (StatusCode::BAD_REQUEST, e))
}

// --------------------------------------------------------- bâtiment ciblé

/// Le bâtiment sous un point, ses façades proposables et ses murs mitoyens.
/// L'index d'une façade est sa position dans `facades` : les deux routes le
/// recalculent à l'identique depuis la même archive. Les murs mitoyens n'y
/// entrent pas — les y mêler décalerait les index.
struct Target {
    building: Building,
    facades: Vec<Facade>,
    party_walls: Vec<Facade>,
}

fn find_target(vstore: &VectorStore, lat: f64, lng: f64) -> Result<Target, ApiError> {
    let vtiles_err = |e: std::io::Error| (StatusCode::INTERNAL_SERVER_ERROR, format!("vtiles : {e}"));
    let r = PICK_RADIUS_DEG;
    let around = vstore.buildings(lat - r, lng - r, lat + r, lng + r).map_err(vtiles_err)?;
    let index = facade::building_at(&around, lat, lng)
        .ok_or((StatusCode::NOT_FOUND, "aucun bâtiment à ce point".to_string()))?;
    let building = around[index].clone();

    // Voisins : tout ce qui touche l'emprise, sauf le bâtiment lui-même.
    let points = building.rings.iter().flatten();
    let (s, n) = points.clone().fold((f64::MAX, f64::MIN), |(s, n), p| (s.min(p.0), n.max(p.0)));
    let (w, e) = points.fold((f64::MAX, f64::MIN), |(w, e), p| (w.min(p.1), e.max(p.1)));
    let m = NEIGHBOUR_MARGIN_DEG;
    let neighbours: Vec<Vec<Vec<(f64, f64)>>> = vstore
        .buildings(s - m, w - m, n + m, e + m)
        .map_err(vtiles_err)?
        .iter()
        .filter(|b| b.osm_id != building.osm_id)
        .map(facade::rings_px)
        .collect();
    let occupied = |x: f64, y: f64| neighbours.iter().any(|rings| facade::point_in_rings(rings, x, y));

    let mpp = facade::meters_per_px(lat);
    let (party_walls, facades) = facade::facades(&building)
        .into_iter()
        .partition(|f| facade::is_party_wall(f, mpp, occupied));
    Ok(Target { building, facades, party_walls })
}

// ---------------------------------------------------------- /buildings/at

#[derive(Deserialize)]
pub(crate) struct BuildingAtQuery {
    lat: f64,
    lng: f64,
}

#[derive(Serialize)]
struct FacadeDto {
    index: usize,
    azimuth_deg: f64,
    /// N, NE, E, SE, S, SO, O, NO.
    orientation: &'static str,
    length_m: f64,
    courtyard: bool,
    /// `[lat, lng]`, pour surligner la façade sur la carte.
    line: Vec<[f64; 2]>,
}

/// Mur collé à un voisin : pas de soleil à mesurer, mais il fait partie du
/// volume que le client dessine.
#[derive(Serialize)]
struct PartyWallDto {
    azimuth_deg: f64,
    length_m: f64,
    courtyard: bool,
    line: Vec<[f64; 2]>,
}

#[derive(Serialize)]
struct BuildingAtResponse {
    id: String,
    name: Option<String>,
    height_m: f32,
    height_from_osm: bool,
    levels: u32,
    /// Toujours vrai aujourd'hui : l'archive ne garde que la hauteur, le
    /// nombre d'étages en est déduit. Le client le corrige via `levels`.
    levels_estimated: bool,
    facades: Vec<FacadeDto>,
    /// Murs mitoyens, écartés de `facades`. Hors numérotation des façades.
    party_walls: Vec<PartyWallDto>,
    /// Emprise complète, anneaux `[lat, lng]` (extérieurs et cours, règle
    /// pair-impair), murs mitoyens compris : le client en tire le volume.
    footprint: Vec<Vec<[f64; 2]>>,
}

fn round1(v: f64) -> f64 {
    (v * 10.0).round() / 10.0
}

pub(crate) async fn building_at(
    State(state): State<Arc<AppState>>,
    Query(q): Query<BuildingAtQuery>,
) -> Result<Response, ApiError> {
    check_coords(q.lat, q.lng)?;
    let target = find_target(&state.vstore, q.lat, q.lng)?;
    let b = &target.building;
    let body = BuildingAtResponse {
        id: b.osm_id.clone(),
        name: b.name.clone(),
        height_m: b.height_m,
        height_from_osm: b.height_from_osm,
        levels: facade::estimated_levels(b.height_m),
        levels_estimated: true,
        facades: target
            .facades
            .iter()
            .enumerate()
            .map(|(index, f)| FacadeDto {
                index,
                azimuth_deg: round1(f.azimuth_deg),
                orientation: facade::orientation_label(f.azimuth_deg),
                length_m: round1(f.length_m),
                courtyard: f.courtyard,
                line: f.line_latlng(),
            })
            .collect(),
        party_walls: target
            .party_walls
            .iter()
            .map(|f| PartyWallDto {
                azimuth_deg: round1(f.azimuth_deg),
                length_m: round1(f.length_m),
                courtyard: f.courtyard,
                line: f.line_latlng(),
            })
            .collect(),
        footprint: b.rings.iter().map(|r| r.iter().map(|p| [p.0, p.1]).collect()).collect(),
    };
    let json = serde_json::to_string(&body).map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(json_response(Arc::new(json)))
}

// ------------------------------------------------------------------ scène

/// DSM 3×3 tuiles autour d'un point, bâtiments et canopée tamponnés, plus le
/// relief nu (le sol des points d'observation) — comme `/places`.
struct Scene {
    dsm: Dsm,
    terrain: Dsm,
    owner: Vec<u32>,
    buildings: Arc<Vec<Building>>,
    origin_x: f64,
    origin_y: f64,
    max_elevation: f32,
}

impl Scene {
    fn local(&self, wx: f64, wy: f64) -> (f64, f64) {
        (wx - self.origin_x, wy - self.origin_y)
    }

    /// Le bâtiment (indice dans `buildings`) qui occupe la cellule, s'il y en a.
    fn owner_at(&self, px: f64, py: f64) -> Option<&Building> {
        let (x, y) = (px.round(), py.round());
        if x < 0.0 || y < 0.0 || x >= self.dsm.width as f64 || y >= self.dsm.height as f64 {
            return None;
        }
        let o = self.owner[y as usize * self.dsm.width + x as usize];
        (o < OWNER_FIRST_SENTINEL).then(|| self.buildings.get(o as usize)).flatten()
    }
}

async fn assemble_scene(state: &AppState, lat: f64, lng: f64) -> Result<Scene, ApiError> {
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
    let terrain = dsm.clone();
    let (buildings, owner) = add_buildings(state, &mut dsm, origin_x, origin_y).await?;
    let max_elevation = dsm.max_elevation();
    Ok(Scene { dsm, terrain, owner, buildings, origin_x, origin_y, max_elevation })
}

/// Positions solaires de chaque instant échantillonné, calculées une fois pour
/// toutes les cases d'une même requête.
fn sun_table(days: &[SampledDay], lat: f64, lng: f64) -> Vec<Vec<SunPosition>> {
    days.iter()
        .map(|d| d.instants.iter().map(|i| sun_position(i.unix, lat, lng)).collect())
        .collect()
}

fn shadow_params(observer_height_m: f64) -> ShadowParams {
    ShadowParams {
        max_distance_m: 5_000.0,
        observer_height_m,
        step_px: 1.0,
        ..ShadowParams::default()
    }
}

/// Profil d'un point d'observation de la scène. `faces` filtre les instants où
/// le soleil est derrière une façade (ombre sans rayon) ; `None` pour un point
/// au sol, qui voit tout le ciel.
fn point_profile(
    scene: &Scene,
    days: &[SampledDay],
    suns: &[Vec<SunPosition>],
    (px, py): (f64, f64),
    ground: f32,
    observer_height_m: f64,
    faces: Option<&Facade>,
) -> ExposureProfile {
    let params = shadow_params(observer_height_m);
    build_profile(days, |d, i| {
        let sun = &suns[d][i];
        if !sun.is_up() {
            return Sample::Night;
        }
        if faces.is_some_and(|f| !f.faces_sun(sun.azimuth_deg)) {
            return Sample::Shadow;
        }
        match shadow_hit_from_ground(&scene.dsm, sun, px, py, ground, &params, scene.max_elevation) {
            None => Sample::Sunlit,
            Some(_) => Sample::Shadow,
        }
    })
}

// -------------------------------------------------------- /exposure/facade

#[derive(Deserialize)]
pub(crate) struct FacadeExposureQuery {
    lat: f64,
    lng: f64,
    building: String,
    facade: usize,
    levels: Option<u32>,
    tz: Option<String>,
}

#[derive(Serialize)]
struct CellDto {
    level: u32,
    column: usize,
    /// Point d'observation sous le toit d'un voisin accolé : pas de fenêtre
    /// possible, pas de profil.
    blind: bool,
    profile: Option<ExposureProfile>,
}

#[derive(Serialize)]
struct FacadeExposureResponse {
    building: String,
    facade: usize,
    levels: u32,
    levels_estimated: bool,
    columns: usize,
    tz: String,
    /// `false` : la végétation est considérée feuillue toute l'année, le
    /// soleil d'hiver derrière un feuillu est donc sous-estimé.
    vegetation_seasonal: bool,
    cells: Vec<CellDto>,
}

pub(crate) async fn facade_exposure(
    State(state): State<Arc<AppState>>,
    Query(q): Query<FacadeExposureQuery>,
) -> Result<Response, ApiError> {
    check_coords(q.lat, q.lng)?;
    let levels_override = check_levels(q.levels)?;
    let tz = check_tz(q.tz.as_deref())?;

    let target = find_target(&state.vstore, q.lat, q.lng)?;
    if target.building.osm_id != q.building {
        return Err((
            StatusCode::CONFLICT,
            format!("le bâtiment à ce point est {}, pas {}", target.building.osm_id, q.building),
        ));
    }
    let Some(chosen) = target.facades.get(q.facade).cloned() else {
        return Err((StatusCode::NOT_FOUND, format!("façade {} inexistante", q.facade)));
    };
    let levels = levels_override.unwrap_or_else(|| facade::estimated_levels(target.building.height_m));

    let key = format!("facade|{}|{}|{levels}|{tz}", q.building, q.facade);
    if let Some(hit) = state.exposure_cache.get(&key) {
        return Ok(json_response(hit));
    }

    let started = std::time::Instant::now();
    let scene = assemble_scene(&state, q.lat, q.lng).await?;
    let building = target.building;
    let building_id = building.osm_id.clone();
    let columns = chosen.columns();
    let body = tokio::task::spawn_blocking(move || {
        compute_facade(&scene, &building, &chosen, levels, tz)
    })
    .await
    .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("calcul : {e}")))?;

    let response = FacadeExposureResponse {
        building: building_id,
        facade: q.facade,
        levels,
        levels_estimated: levels_override.is_none(),
        columns,
        tz: tz.name().to_string(),
        vegetation_seasonal: false,
        cells: body,
    };
    let json = Arc::new(
        serde_json::to_string(&response).map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?,
    );
    println!(
        "[exposure/facade] {key} → {} cases en {:.0} ms",
        response.cells.len(),
        started.elapsed().as_secs_f64() * 1000.0
    );
    state.exposure_cache.put(key, json.clone());
    Ok(json_response(json))
}

/// Toutes les cases d'une façade, réparties sur les cœurs disponibles :
/// ~0,2 s de CPU pour 36 cases × 365 jours, autant ne pas les enchaîner.
fn compute_facade(scene: &Scene, building: &Building, chosen: &Facade, levels: u32, tz: Tz) -> Vec<CellDto> {
    let days = sampled_days(tz, REFERENCE_YEAR, DEFAULT_DAY_STRIDE);
    let (wx, wy) = chosen.line_px[chosen.line_px.len() / 2];
    let (lat, lng) = latlon_of_world_px(wx, wy);
    let suns = sun_table(&days, lat, lng);

    // Sol du bâtiment : le relief au centre de son emprise, comme le tamponnage
    // (`GroundRef::BboxCenter`) — les étages se comptent depuis là.
    let rings = facade::rings_px(building);
    let all = rings.iter().flatten();
    let (min_x, max_x) = all.clone().fold((f64::MAX, f64::MIN), |(a, b), p| (a.min(p.0), b.max(p.0)));
    let (min_y, max_y) = all.fold((f64::MAX, f64::MIN), |(a, b), p| (a.min(p.1), b.max(p.1)));
    let (cx, cy) = scene.local((min_x + max_x) / 2.0, (min_y + max_y) / 2.0);
    let ground = scene
        .terrain
        .sample(
            cx.clamp(0.0, scene.terrain.width as f64 - 1.0),
            cy.clamp(0.0, scene.terrain.height as f64 - 1.0),
        )
        .unwrap_or(0.0);

    // Points d'observation dans la DSM. Si la rasterisation déborde du mur
    // (cellule de ~1,6 m), on s'écarte encore d'une cellule à la fois : un
    // rayon parti du dedans de son propre immeuble ne verrait jamais le jour.
    let columns: Vec<(f64, f64)> = chosen
        .observation_points(facade::meters_per_px(lat))
        .into_iter()
        .map(|(x, y)| {
            let (mut px, mut py) = scene.local(x, y);
            for _ in 0..3 {
                if scene.owner_at(px, py).is_none_or(|b| b.osm_id != building.osm_id) {
                    break;
                }
                px += chosen.normal.0;
                py += chosen.normal.1;
            }
            (px, py)
        })
        .collect();

    let cells: Vec<(u32, usize)> = (0..levels).flat_map(|l| (0..columns.len()).map(move |c| (l, c))).collect();
    let threads = std::thread::available_parallelism().map_or(1, |n| n.get()).min(cells.len().max(1));
    let chunk = cells.len().div_ceil(threads.max(1)).max(1);

    std::thread::scope(|s| {
        let handles: Vec<_> = cells
            .chunks(chunk)
            .map(|part| {
                let (days, suns, columns) = (&days, &suns, &columns);
                s.spawn(move || {
                    part.iter()
                        .map(|&(level, column)| {
                            let (px, py) = columns[column];
                            let height = facade::level_height_m(level);
                            // Fenêtre impossible : sous le toit d'un voisin
                            // accolé, plus haut que cet étage.
                            let blind = scene.owner_at(px, py).is_some_and(|b| b.osm_id != building.osm_id)
                                && scene.dsm.sample(px, py).is_some_and(|top| top as f64 >= ground as f64 + height);
                            let profile = (!blind).then(|| {
                                point_profile(scene, days, suns, (px, py), ground, height, Some(chosen))
                            });
                            CellDto { level, column, blind, profile }
                        })
                        .collect::<Vec<_>>()
                })
            })
            .collect();
        handles.into_iter().flat_map(|h| h.join().expect("thread de calcul")).collect()
    })
}

// --------------------------------------------------------- /exposure/point

#[derive(Deserialize)]
pub(crate) struct PointExposureQuery {
    lat: Option<f64>,
    lng: Option<f64>,
    height: Option<f64>,
    /// Identifiant d'un lieu de `/places` (`node/123`) : prime sur lat/lng.
    place: Option<String>,
    tz: Option<String>,
}

#[derive(Serialize)]
struct PointExposureResponse {
    /// Point effectivement mesuré (pour un lieu : après recalage éventuel).
    lat: f64,
    lng: f64,
    height: f64,
    place: Option<String>,
    tz: String,
    vegetation_seasonal: bool,
    profile: ExposureProfile,
}

pub(crate) async fn point_exposure(
    State(state): State<Arc<AppState>>,
    Query(q): Query<PointExposureQuery>,
) -> Result<Response, ApiError> {
    let tz = check_tz(q.tz.as_deref())?;
    let place_id = q.place.as_deref().map(str::trim).filter(|p| !p.is_empty());

    // Le lieu et sa position éventuellement contribuée, ou le point brut.
    let (lat, lng, height, place) = match place_id {
        Some(id) => {
            let (lat, lng) = db::place_coordinate(&state.pool, id)
                .await
                .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("PostGIS : {e}")))?
                .ok_or((StatusCode::NOT_FOUND, format!("lieu inconnu : {id}")))?;
            (lat, lng, PLACE_OBSERVER_HEIGHT_M, Some(id.to_string()))
        }
        None => {
            let (Some(lat), Some(lng)) = (q.lat, q.lng) else {
                return Err((StatusCode::BAD_REQUEST, "lat/lng ou place attendus".into()));
            };
            (lat, lng, check_height(q.height)?, None)
        }
    };
    check_coords(lat, lng)?;

    let key = match &place {
        Some(id) => format!("place|{id}|{tz}"),
        None => format!("point|{lat:.5}|{lng:.5}|{height:.2}|{tz}"),
    };
    if let Some(hit) = state.exposure_cache.get(&key) {
        return Ok(json_response(hit));
    }

    let scene = assemble_scene(&state, lat, lng).await?;
    let (px, py) = match &place {
        Some(id) => {
            // Même position que `/places` : relue avec les mêmes données.
            let e = 1e-6;
            let poi = db::places_in_bbox(&state.pool, lat - e, lng - e, lat + e, lng + e)
                .await
                .map_err(|err| (StatusCode::INTERNAL_SERVER_ERROR, format!("PostGIS : {err}")))?
                .into_iter()
                .find(|p| &p.osm_id == id)
                .ok_or((StatusCode::NOT_FOUND, format!("lieu inconnu : {id}")))?;
            let report = db::terrace_reports_in_bbox(&state.pool, lat - e, lng - e, lat + e, lng + e)
                .await
                .map_err(|err| (StatusCode::INTERNAL_SERVER_ERROR, format!("PostGIS : {err}")))?
                .remove(id);
            let terrace = report.and_then(|r| Some((r.lat?, r.lng?)));
            let (px, py, _) = place_position(&poi, terrace, &scene.dsm, &scene.owner, scene.origin_x, scene.origin_y);
            (px, py)
        }
        None => {
            let (wx, wy) = world_px(lat, lng);
            scene.local(wx, wy)
        }
    };
    let (out_lat, out_lng) = latlon_of_world_px(scene.origin_x + px, scene.origin_y + py);

    let body = tokio::task::spawn_blocking(move || {
        let days = sampled_days(tz, REFERENCE_YEAR, DEFAULT_DAY_STRIDE);
        let suns = sun_table(&days, lat, lng);
        let ground = scene.terrain.sample(px, py).unwrap_or(0.0);
        point_profile(&scene, &days, &suns, (px, py), ground, height, None)
    })
    .await
    .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("calcul : {e}")))?;

    let response = PointExposureResponse {
        lat: out_lat,
        lng: out_lng,
        height,
        place,
        tz: tz.name().to_string(),
        vegetation_seasonal: false,
        profile: body,
    };
    let json = Arc::new(
        serde_json::to_string(&response).map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?,
    );
    state.exposure_cache.put(key, json.clone());
    Ok(json_response(json))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validation_bounds() {
        assert!(check_coords(48.85, 2.35).is_ok());
        assert_eq!(check_coords(89.0, 2.35).unwrap_err().0, StatusCode::BAD_REQUEST);
        assert_eq!(check_coords(f64::NAN, 2.35).unwrap_err().0, StatusCode::BAD_REQUEST);
        assert_eq!(check_levels(None).unwrap(), None);
        assert_eq!(check_levels(Some(7)).unwrap(), Some(7));
        assert!(check_levels(Some(0)).is_err());
        assert!(check_levels(Some(61)).is_err());
        assert_eq!(check_height(None).unwrap(), 1.5);
        assert_eq!(check_height(Some(12.0)).unwrap(), 12.0);
        assert!(check_height(Some(-1.0)).is_err());
        assert!(check_height(Some(500.0)).is_err());
        assert!(check_tz(Some("Europe/Paris")).is_ok());
        assert_eq!(check_tz(Some("Nowhere/Land")).unwrap_err().0, StatusCode::BAD_REQUEST);
    }

    #[test]
    fn cache_is_bounded_fifo() {
        let cache = ExposureCache::default();
        for i in 0..CACHE_CAPACITY + 10 {
            cache.put(format!("k{i}"), Arc::new(i.to_string()));
        }
        assert!(cache.get("k0").is_none());
        assert!(cache.get("k9").is_none());
        assert_eq!(cache.get("k10").as_deref().map(String::as_str), Some("10"));
        assert_eq!(cache.inner.lock().unwrap().0.len(), CACHE_CAPACITY);
    }

    /// L'archive de test (`testdata/mini.pmtiles`) : un carré isolé, way/1.
    #[test]
    fn target_from_the_test_archive() {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/testdata/mini.pmtiles");
        let store = VectorStore::open(path).expect("fixture lisible");
        let b = &store.buildings(48.85, 2.34, 48.86, 2.36).unwrap()[0];
        let n = b.rings[0].len() as f64;
        let lat = b.rings[0].iter().map(|p| p.0).sum::<f64>() / n;
        let lng = b.rings[0].iter().map(|p| p.1).sum::<f64>() / n;

        let target = find_target(&store, lat, lng).expect("bâtiment trouvé");
        assert_eq!(target.building.osm_id, "way/1");
        let mut labels: Vec<_> =
            target.facades.iter().map(|f| facade::orientation_label(f.azimuth_deg)).collect();
        labels.sort();
        assert_eq!(labels, vec!["E", "N", "O", "S"]);

        let miss = find_target(&store, 45.0, 4.0).err().expect("rien ici");
        assert_eq!(miss.0, StatusCode::NOT_FOUND);
    }

    /// Chemin de calcul complet sur une scène synthétique, tamponnée par le
    /// vrai `stamp_buildings` : immeuble de 7 étages, rue de 12 m au sud, en
    /// face un immeuble de 30 m.
    #[test]
    fn facade_grid_on_a_synthetic_street() {
        const LAT: f64 = 48.85;
        const LNG: f64 = 2.35;
        let north = |m: f64| m / 111_320.0;
        let east = |m: f64| m / (111_320.0 * LAT.to_radians().cos());
        let rect = |x0: f64, y0: f64, w: f64, h: f64| {
            vec![
                (LAT + north(y0), LNG + east(x0)),
                (LAT + north(y0), LNG + east(x0 + w)),
                (LAT + north(y0 + h), LNG + east(x0 + w)),
                (LAT + north(y0 + h), LNG + east(x0)),
                (LAT + north(y0), LNG + east(x0)),
            ]
        };
        let building = |id: &str, ring, height| Building {
            osm_id: id.into(),
            name: None,
            rings: vec![ring],
            height_m: height,
            height_from_osm: true,
            leaf_type: None,
        };
        let target = building("way/1", rect(0.0, 0.0, 24.0, 12.0), 24.0);
        let opposite = building("way/2", rect(-20.0, -27.0, 64.0, 15.0), 30.0);
        let buildings = vec![target.clone(), opposite];

        let mpp = facade::meters_per_px(LAT);
        let terrain = Dsm::flat(300, 300, mpp, 35.0);
        let (cx, cy) = world_px(LAT, LNG);
        let (origin_x, origin_y) = (cx.floor() - 150.0, cy.floor() - 150.0);
        let mut dsm = terrain.clone();
        let mut owner = vec![OWNER_TERRAIN; 300 * 300];
        stamp_buildings(&mut dsm, &terrain, &mut owner, origin_x, origin_y, &buildings, GroundRef::BboxCenter);
        let max_elevation = dsm.max_elevation();
        let scene = Scene { dsm, terrain, owner, buildings: Arc::new(buildings), origin_x, origin_y, max_elevation };

        let facades = facade::facades(&target);
        let pick = |label| facades.iter().find(|f| facade::orientation_label(f.azimuth_deg) == label).unwrap();
        let tz = exposure::DEFAULT_TZ;
        let south = compute_facade(&scene, &target, pick("S"), 7, tz);
        let north_cells = compute_facade(&scene, &target, pick("N"), 7, tz);

        assert_eq!(south.len(), 7 * 3);
        assert!(south.iter().all(|c| !c.blind && c.profile.is_some()));
        let annual = |cells: &[CellDto], level: u32| {
            let row: Vec<u32> = cells
                .iter()
                .filter(|c| c.level == level)
                .map(|c| c.profile.as_ref().unwrap().annual_avg_sunlit_minutes)
                .collect();
            row.iter().sum::<u32>() / row.len() as u32
        };
        // L'immeuble d'en face masque les bas étages : le soleil monte avec eux.
        let by_level: Vec<u32> = (0..7).map(|l| annual(&south, l)).collect();
        assert!(by_level.windows(2).all(|w| w[0] <= w[1]), "{by_level:?}");
        assert!(by_level[6] > by_level[0] + 120, "{by_level:?}");
        // Sud dégagé en haut > nord ; le nord ne voit jamais le soleil en hiver.
        assert!(annual(&south, 6) > annual(&north_cells, 6));
        let december = |c: &CellDto| c.profile.as_ref().unwrap().months[11].avg_sunlit_minutes;
        assert!(north_cells.iter().all(|c| december(c) == 0));
        assert!(north_cells.iter().any(|c| c.profile.as_ref().unwrap().months[5].avg_sunlit_minutes > 60));
    }
}
