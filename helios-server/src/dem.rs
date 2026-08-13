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

/// Erreur de tuile. Le 404 — zone hors couverture À CE zoom — se distingue
/// des autres échecs : lui seul autorise le repli vers un zoom parent. Une
/// panne réseau ou un décodage raté doivent remonter tels quels — se replier
/// dessus dégraderait silencieusement la résolution, et la tuile synthétisée
/// resterait au cache bien après la fin de l'incident.
enum TileError {
    NotFound,
    Other(String),
}

/// Tuile Mapterhorn décodée en altitudes (cache mémoire).
pub async fn fetch_tile(
    http: &reqwest::Client,
    cache: &TileCache,
    z: u32,
    x: u32,
    y: u32,
) -> Result<Arc<Vec<f32>>, String> {
    fetch_tile_inner(http, cache, z, x, y).await.map_err(|e| match e {
        TileError::NotFound => format!("tuile {TILE_URL}/{z}/{x}/{y}.webp : 404 Not Found"),
        TileError::Other(msg) => msg,
    })
}

async fn fetch_tile_inner(
    http: &reqwest::Client,
    cache: &TileCache,
    z: u32,
    x: u32,
    y: u32,
) -> Result<Arc<Vec<f32>>, TileError> {
    if let Some(hit) = cache.read().await.get(&(z, x, y)) {
        return Ok(hit.clone());
    }

    let url = format!("{TILE_URL}/{z}/{x}/{y}.webp");
    let resp = http
        .get(&url)
        .send()
        .await
        .map_err(|e| TileError::Other(format!("tuile {url} : {e}")))?;
    if resp.status() == reqwest::StatusCode::NOT_FOUND {
        return Err(TileError::NotFound);
    }
    let bytes = resp
        .error_for_status()
        .map_err(|e| TileError::Other(format!("tuile {url} : {e}")))?
        .bytes()
        .await
        .map_err(|e| TileError::Other(format!("tuile {url} : {e}")))?;

    let img = image::load_from_memory(&bytes)
        .map_err(|e| TileError::Other(format!("décodage {url} : {e}")))?
        .to_rgb8();
    if img.width() as usize != TILE_SIZE || img.height() as usize != TILE_SIZE {
        return Err(TileError::Other(format!(
            "tuile {url} : taille inattendue {}×{}",
            img.width(),
            img.height()
        )));
    }

    // Même décodage que Dsm::from_terrarium_rgb (on ne garde que les floats).
    let floats = Dsm::from_terrarium_rgb(img.as_raw(), TILE_SIZE, TILE_SIZE, 1.0).data;
    let arc = Arc::new(floats);
    cache.write().await.insert((z, x, y), arc.clone());
    Ok(arc)
}

/// Zoom plancher du repli : la base mondiale de Mapterhorn (Copernicus
/// GLO-30) est servie partout à z12 — descendre encore ne rattrape que des
/// trous d'océan, pas de la couverture. Deux crans sous z12 par prudence.
pub const FALLBACK_MIN_ZOOM: u32 = 10;

/// Tuile au zoom demandé, ou synthétisée depuis le premier ancêtre couvert.
///
/// Mapterhorn ne raffine (z13-z16) que les pays à LiDAR ouvert : l'Italie
/// s'arrête à z13, d'autres à z12. Plutôt que de répondre 502 sur ces zones
/// (comportement d'avant ce repli), on agrandit en bilinéaire le quadrant de
/// la première tuile parente servie — la grille garde partout la géométrie
/// du zoom de travail, seule la finesse du relief se dégrade (z13 ≈ 6 m/px
/// là où z15 fait 1,57 m/px). La tuile synthétisée entre au cache sous la
/// clé demandée : le coût du repli se paie une fois.
///
/// Limite assumée (cf. commentaire de module) : hors zones raffinées, la base
/// GLO-30 est un DSM — bâtiments et canopée y sont fondus dans le relief à
/// ~30 m, et nos emprises OSM se stampent par-dessus. C'est l'arbitrage
/// documenté dans `docs/recherche-donnees-elevation.md` : un relief un peu
/// gonflé vaut mieux qu'aucune réponse.
pub async fn fetch_tile_or_ancestor(
    http: &reqwest::Client,
    cache: &TileCache,
    z: u32,
    x: u32,
    y: u32,
) -> Result<Arc<Vec<f32>>, String> {
    match fetch_tile_inner(http, cache, z, x, y).await {
        Ok(tile) => return Ok(tile),
        Err(TileError::Other(msg)) => return Err(msg),
        Err(TileError::NotFound) => {}
    }
    for parent_z in (FALLBACK_MIN_ZOOM..z).rev() {
        let dz = z - parent_z;
        match fetch_tile_inner(http, cache, parent_z, x >> dz, y >> dz).await {
            Ok(parent) => {
                let tile = Arc::new(upsample_quadrant(&parent, x, y, dz));
                cache.write().await.insert((z, x, y), tile.clone());
                return Ok(tile);
            }
            Err(TileError::NotFound) => continue,
            Err(TileError::Other(msg)) => return Err(msg),
        }
    }
    Err(format!(
        "tuile {z}/{x}/{y} : aucune couverture Mapterhorn jusqu'à z{FALLBACK_MIN_ZOOM}"
    ))
}

/// Agrandit en bilinéaire le sous-bloc de la tuile parente qui couvre la
/// tuile `(z, x, y)`, `dz` crans plus bas. Le bloc source fait
/// `TILE_SIZE / 2^dz` pixels de côté, à l'offset donné par les bits de poids
/// faible de `x`/`y`. Échantillonnage au centre des pixels, bords clampés à
/// la tuile parente — la couture entre deux parents reste possible mais
/// imperceptible sur du relief.
fn upsample_quadrant(parent: &[f32], x: u32, y: u32, dz: u32) -> Vec<f32> {
    let scale = 1u64 << dz;
    let block = TILE_SIZE as f64 / scale as f64;
    let ox = (x as u64 % scale) as f64 * block;
    let oy = (y as u64 % scale) as f64 * block;
    let max = (TILE_SIZE - 1) as f64;

    let mut out = vec![0f32; TILE_SIZE * TILE_SIZE];
    for j in 0..TILE_SIZE {
        let sy = (oy + (j as f64 + 0.5) / TILE_SIZE as f64 * block - 0.5).clamp(0.0, max);
        let y0 = sy.floor() as usize;
        let y1 = (y0 + 1).min(TILE_SIZE - 1);
        let fy = sy - y0 as f64;
        for i in 0..TILE_SIZE {
            let sx = (ox + (i as f64 + 0.5) / TILE_SIZE as f64 * block - 0.5).clamp(0.0, max);
            let x0 = sx.floor() as usize;
            let x1 = (x0 + 1).min(TILE_SIZE - 1);
            let fx = sx - x0 as f64;
            let top = parent[y0 * TILE_SIZE + x0] as f64 * (1.0 - fx)
                + parent[y0 * TILE_SIZE + x1] as f64 * fx;
            let bottom = parent[y1 * TILE_SIZE + x0] as f64 * (1.0 - fx)
                + parent[y1 * TILE_SIZE + x1] as f64 * fx;
            out[j * TILE_SIZE + i] = (top * (1.0 - fy) + bottom * fy) as f32;
        }
    }
    out
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Parent en dégradé linéaire est-ouest : l'agrandissement bilinéaire
    /// d'un dégradé doit rendre exactement le même dégradé, rééchelonné au
    /// quadrant — toute marche d'escalier trahirait un offset de quadrant ou
    /// un échantillonnage hors centre de pixel.
    #[test]
    fn upsample_preserve_un_degrade() {
        let parent: Vec<f32> = (0..TILE_SIZE * TILE_SIZE)
            .map(|i| (i % TILE_SIZE) as f32)
            .collect();

        // Quadrant nord-ouest (x pair, y pair) à dz=1 : bloc source
        // [0, 256), donc valeurs attendues ≈ sx = ox + (i+0.5)/512*256 − 0.5.
        let out = upsample_quadrant(&parent, 8758, 6086, 1);
        for &i in &[0usize, 100, 300, 511] {
            let expected = ((i as f64 + 0.5) / 512.0 * 256.0 - 0.5).clamp(0.0, 511.0);
            let got = out[256 * TILE_SIZE + i] as f64;
            assert!(
                (got - expected).abs() < 1e-3,
                "px {i} : attendu {expected}, obtenu {got}"
            );
        }

        // Quadrant sud-est (x impair, y impair) : bloc source [256, 512).
        let out = upsample_quadrant(&parent, 8759, 6087, 1);
        let expected: f64 = (256.0 + 0.5 / 512.0 * 256.0 - 0.5f64).clamp(0.0, 511.0);
        assert!((out[0] as f64 - expected).abs() < 1e-3);
    }

    /// dz=3 (z15 depuis z12) : le bloc source ne fait plus que 64 px, la
    /// valeur du coin doit venir du bon soixante-quatrième de la tuile.
    #[test]
    fn upsample_dz3_vise_le_bon_bloc() {
        let parent: Vec<f32> = (0..TILE_SIZE * TILE_SIZE)
            .map(|i| (i / TILE_SIZE) as f32)
            .collect();
        // y % 8 == 7 → bloc source vertical [448, 512).
        let out = upsample_quadrant(&parent, 0, 7, 3);
        let center = out[(TILE_SIZE / 2) * TILE_SIZE];
        assert!(
            (448.0..512.0).contains(&(center as f64)),
            "centre {center} hors du bloc attendu [448, 512)"
        );
    }
}
