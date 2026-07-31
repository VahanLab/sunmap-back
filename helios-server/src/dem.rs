//! Tuiles d'élévation Mapterhorn : géométrie Web Mercator et décodage.
//!
//! Encodage Terrarium (`alt = r·256 + g + b/256 − 32768`), webp 512 px, même
//! source que le rendu iOS — relief affiché = relief calculé.
//!
//! Attention à ce que ces tuiles contiennent réellement : Mapterhorn utilise
//! Copernicus GLO-30 comme socle mondial, raffiné par des modèles LiDAR
//! nationaux là où ils existent. Or GLO-30 est un **DSM** (bâtiments et
//! végétation inclus), pas un MNT. Hors zones raffinées, y stamper les
//! emprises OSM revient donc à compter les bâtiments deux fois.
//! Cf. `docs/recherche-donnees-elevation.md` et le binaire `dem_probe`.

use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::RwLock;

use helios_core::dsm::Dsm;

pub const TILE_SIZE: usize = 512;
/// z15 ≈ 1,57 m/pixel à Paris : suffisant pour du relief et des terrasses,
/// limite pour les cours intérieures. (z16 dispo, 4× plus de données.)
pub const ZOOM: u32 = 15;
pub const TILE_URL: &str = "https://tiles.mapterhorn.com";

pub type TileCache = RwLock<HashMap<(u32, u32, u32), Arc<Vec<f32>>>>;

/// Pixel monde Web Mercator au zoom de travail.
pub fn world_px(lat: f64, lng: f64) -> (f64, f64) {
    let n = (TILE_SIZE as f64) * f64::powi(2.0, ZOOM as i32);
    let wx = (lng + 180.0) / 360.0 * n;
    let wy = (1.0 - lat.to_radians().tan().asinh() / std::f64::consts::PI) / 2.0 * n;
    (wx, wy)
}

/// Inverse de [`world_px`] : pixel monde → coordonnée géographique.
pub fn latlon_of_world_px(wx: f64, wy: f64) -> (f64, f64) {
    let n = (TILE_SIZE as f64) * f64::powi(2.0, ZOOM as i32);
    let lon = wx / n * 360.0 - 180.0;
    let lat = (std::f64::consts::PI * (1.0 - 2.0 * wy / n))
        .sinh()
        .atan()
        .to_degrees();
    (lat, lon)
}

/// Taille d'une cellule au sol, en mètres, à cette latitude.
pub fn meters_per_pixel(lat: f64) -> f64 {
    40_075_016.686 * lat.to_radians().cos() / ((TILE_SIZE as f64) * f64::powi(2.0, ZOOM as i32))
}

/// Tuile Mapterhorn décodée en altitudes (cache mémoire).
pub async fn fetch_tile(
    http: &reqwest::Client,
    cache: &TileCache,
    z: u32,
    x: u32,
    y: u32,
) -> Result<Arc<Vec<f32>>, String> {
    if let Some(hit) = cache.read().await.get(&(z, x, y)) {
        return Ok(hit.clone());
    }

    let url = format!("{TILE_URL}/{z}/{x}/{y}.webp");
    let bytes = http
        .get(&url)
        .send()
        .await
        .and_then(|r| r.error_for_status())
        .map_err(|e| format!("tuile {url} : {e}"))?
        .bytes()
        .await
        .map_err(|e| format!("tuile {url} : {e}"))?;

    let img = image::load_from_memory(&bytes)
        .map_err(|e| format!("décodage {url} : {e}"))?
        .to_rgb8();
    if img.width() as usize != TILE_SIZE || img.height() as usize != TILE_SIZE {
        return Err(format!(
            "tuile {url} : taille inattendue {}×{}",
            img.width(),
            img.height()
        ));
    }

    // Même décodage que Dsm::from_terrarium_rgb (on ne garde que les floats).
    let floats = Dsm::from_terrarium_rgb(img.as_raw(), TILE_SIZE, TILE_SIZE, 1.0).data;
    let arc = Arc::new(floats);
    cache.write().await.insert((z, x, y), arc.clone());
    Ok(arc)
}

/// Pixel monde à un zoom quelconque (généralise [`world_px`], qui est figé sur
/// [`ZOOM`]). Utile pour sonder la couverture réelle d'une zone.
pub fn world_px_at(lat: f64, lng: f64, z: u32) -> (f64, f64) {
    let n = (TILE_SIZE as f64) * f64::powi(2.0, z as i32);
    let wx = (lng + 180.0) / 360.0 * n;
    let wy = (1.0 - lat.to_radians().tan().asinh() / std::f64::consts::PI) / 2.0 * n;
    (wx, wy)
}

/// Altitude Mapterhorn en un point, sans assembler de grille — une seule
/// tuile suffit tant qu'on ne fait qu'échantillonner.
pub async fn elevation_at(
    http: &reqwest::Client,
    cache: &TileCache,
    lat: f64,
    lng: f64,
) -> Result<f32, String> {
    elevation_at_zoom(http, cache, lat, lng, ZOOM).await
}

/// Variante à zoom explicite : la couverture Mapterhorn n'est pas uniforme,
/// z15 n'existe que dans les régions raffinées par un LiDAR national.
pub async fn elevation_at_zoom(
    http: &reqwest::Client,
    cache: &TileCache,
    lat: f64,
    lng: f64,
    z: u32,
) -> Result<f32, String> {
    let (wx, wy) = world_px_at(lat, lng, z);
    let (tx, ty) = (
        (wx / TILE_SIZE as f64) as u32,
        (wy / TILE_SIZE as f64) as u32,
    );
    let tile = fetch_tile(http, cache, z, tx, ty).await?;

    let px = (wx - (tx as f64) * TILE_SIZE as f64).clamp(0.0, TILE_SIZE as f64 - 1.0);
    let py = (wy - (ty as f64) * TILE_SIZE as f64).clamp(0.0, TILE_SIZE as f64 - 1.0);
    let dsm = Dsm {
        width: TILE_SIZE,
        height: TILE_SIZE,
        meters_per_pixel: meters_per_pixel(lat),
        data: (*tile).clone(),
        canopy_top: None,
        canopy_base: None,
    };
    dsm.sample(px, py).ok_or_else(|| "hors tuile".to_string())
}

/// Zoom maximal réellement servi en un point (sonde descendante).
pub async fn max_zoom_at(
    http: &reqwest::Client,
    cache: &TileCache,
    lat: f64,
    lng: f64,
) -> Option<u32> {
    for z in (6..=16).rev() {
        if elevation_at_zoom(http, cache, lat, lng, z).await.is_ok() {
            return Some(z);
        }
    }
    None
}
