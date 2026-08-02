//! Tuiles de canopée servies au client (`GET /canopy/{z}/{x}/{y}`) : de quoi
//! faire porter l'ombre des forêts au masque Metal.
//!
//! La DSM du client est terrain seul (tuiles DEM Mapterhorn, un MNT — ni
//! bâtiments ni végétation). Ces tuiles apportent la couche canopée dans la
//! même grille slippy 512 px : le shader (`Shaders.metal`) applique alors la
//! même transmittance que `shadow.rs` côté serveur — mêmes données (tables
//! `woods` et `trees`), même algorithme, écart rendu/calcul nul sur les
//! forêts.
//!
//! Format : PNG RGB 512×512. R = sommet de couronne, G = base de couronne,
//! en **mètres au-dessus du sol × 2** (pas de 0,5 m, plafond 127,5 m),
//! B = 0. Relatif au sol et non absolu : le client additionne sur SON
//! altitude de terrain — même source Mapterhorn des deux côtés, et la tuile
//! reste indépendante du DEM. Le PNG compresse très bien ces aplats (une
//! tuile sans végétation fait ~1 Ko).
//!
//! Rasterisation propre au module (scanline pair-impair + disques), distincte
//! de `stamp_canopy` : celle-ci travaille en pixels monde z15 absolus sur la
//! grille de requête, ici il faut n'importe quel zoom slippy (le client
//! choisit z12-15 selon l'étendue visible) et des hauteurs relatives.

use image::ImageEncoder;

use crate::dem::world_px;
use crate::osm::{Building, Tree};

pub const TILE_SIZE: usize = 512;
/// Le client ne descend jamais sous z12 pour son masque ; en deçà, une tuile
/// couvre >10 km et la rasterisation fine n'a plus de sens.
pub const MIN_Z: u32 = 12;
pub const MAX_Z: u32 = 15;

/// Bornes géographiques (s, w, n, e) d'une tuile slippy.
pub fn tile_bounds(z: u32, x: u32, y: u32) -> (f64, f64, f64, f64) {
    let n = f64::powi(2.0, z as i32);
    let lon_w = x as f64 / n * 360.0 - 180.0;
    let lon_e = (x + 1) as f64 / n * 360.0 - 180.0;
    let lat_n = (std::f64::consts::PI * (1.0 - 2.0 * y as f64 / n)).sinh().atan().to_degrees();
    let lat_s = (std::f64::consts::PI * (1.0 - 2.0 * (y + 1) as f64 / n)).sinh().atan().to_degrees();
    (lat_s, lon_w, lat_n, lon_e)
}

/// Grilles sommet/base de la tuile, en mètres au-dessus du sol.
pub struct CanopyTile {
    pub top: Vec<f32>,
    pub base: Vec<f32>,
    /// Pixel couvert par une **emprise boisée** (`woods`), par opposition à
    /// un arbre isolé (`natural=tree`). Décide du modèle 3D posé côté client :
    /// bosquet sur une emprise, arbre seul sinon.
    pub wood: Vec<bool>,
    /// Silhouette dominante du pixel (0 feuillu, 1 conifère, 2 palmier) —
    /// celle de la végétation la plus haute qui l'occupe.
    pub leaf: Vec<u8>,
}

/// Code du canal B : `0` = pas de canopée, sinon
/// `40 × (1 + type de feuillage + 3 × emprise boisée)`.
///
/// Valeurs espacées de 40 et décodées au multiple le plus proche : le PNG est
/// sans perte, mais il traverse un contexte CoreGraphics côté client — un
/// champ de bits serait corrompu par le moindre décalage d'un LSB, un
/// intervalle de 40 ne l'est pas.
pub const CLASS_STEP: u8 = 40;

fn class_code(wood: bool, leaf: u8) -> u8 {
    CLASS_STEP * (1 + leaf + if wood { 3 } else { 0 })
}

pub fn rasterize(z: u32, x: u32, y: u32, woods: &[Building], trees: &[Tree]) -> CanopyTile {
    // Le repère de travail est le pixel de tuile au zoom demandé : pixels
    // monde z15 (la seule échelle que `world_px` parle) divisés par 2^(15−z).
    let scale = f64::powi(2.0, 15 - z as i32);
    let origin_x = x as f64 * TILE_SIZE as f64;
    let origin_y = y as f64 * TILE_SIZE as f64;
    let to_px = |lat: f64, lon: f64| -> (f64, f64) {
        let (wx, wy) = world_px(lat, lon);
        (wx / scale - origin_x, wy / scale - origin_y)
    };

    let (lat_s, _, lat_n, _) = tile_bounds(z, x, y);
    let mid_lat = (lat_s + lat_n) / 2.0;
    let meters_per_px = 40_075_016.686 * mid_lat.to_radians().cos()
        / (TILE_SIZE as f64 * f64::powi(2.0, z as i32));

    let mut top = vec![0.0f32; TILE_SIZE * TILE_SIZE];
    let mut base = vec![0.0f32; TILE_SIZE * TILE_SIZE];
    let mut wood_mask = vec![false; TILE_SIZE * TILE_SIZE];
    let mut leaf = vec![0u8; TILE_SIZE * TILE_SIZE];

    // Bois : remplissage scanline pair-impair, tous anneaux ensemble — une
    // clairière (anneau intérieur) rebascule en « dehors » et reste creuse.
    // Base au sol : un sous-bois n'a pas de tronc dégagé (cf. stamp_canopy).
    for wood in woods {
        let rings: Vec<Vec<(f64, f64)>> = wood
            .rings
            .iter()
            .filter(|r| r.len() >= 3)
            .map(|r| r.iter().map(|&(lat, lon)| to_px(lat, lon)).collect())
            .collect();
        if rings.is_empty() {
            continue;
        }
        let (mut y0, mut y1) = (f64::MAX, f64::MIN);
        for ring in &rings {
            for &(_, py) in ring {
                y0 = y0.min(py);
                y1 = y1.max(py);
            }
        }
        let row0 = y0.floor().max(0.0) as usize;
        let row1 = y1.ceil().min(TILE_SIZE as f64 - 1.0) as usize;
        for row in row0..=row1 {
            let scan_y = row as f64 + 0.5;
            let mut crossings: Vec<f64> = Vec::new();
            for ring in &rings {
                for i in 0..ring.len() {
                    let (x1, y1p) = ring[i];
                    let (x2, y2p) = ring[(i + 1) % ring.len()];
                    if (y1p <= scan_y) != (y2p <= scan_y) {
                        crossings.push(x1 + (scan_y - y1p) / (y2p - y1p) * (x2 - x1));
                    }
                }
            }
            crossings.sort_by(|a, b| a.partial_cmp(b).unwrap());
            for pair in crossings.chunks_exact(2) {
                let cx0 = pair[0].max(0.0).round() as usize;
                let cx1 = pair[1].min(TILE_SIZE as f64 - 1.0).round() as usize;
                for cx in cx0..=cx1.min(TILE_SIZE - 1) {
                    let i = row * TILE_SIZE + cx;
                    wood_mask[i] = true;
                    if wood.height_m > top[i] {
                        top[i] = wood.height_m;
                        base[i] = 0.0;
                        leaf[i] = wood.leaf_type.map(leaf_code).unwrap_or(0);
                    }
                }
            }
        }
    }

    // Arbres isolés : disque de couronne, base = sommet − diamètre (le tronc
    // laisse passer dessous), bornée comme dans stamp_canopy.
    for t in trees {
        let (cx, cy) = to_px(t.lat, t.lng);
        let radius_px = (t.crown_radius_m / meters_per_px).max(0.5);
        let t_top = t.height_m as f32;
        let t_base = ((t_top - 2.0 * t.crown_radius_m as f32).min(t_top - 1.0)).max(0.0);

        let x0 = (cx - radius_px).floor().max(0.0) as usize;
        let x1 = (cx + radius_px).ceil().min(TILE_SIZE as f64 - 1.0) as usize;
        let y0 = (cy - radius_px).floor().max(0.0) as usize;
        let y1 = (cy + radius_px).ceil().min(TILE_SIZE as f64 - 1.0) as usize;
        if x1 < x0 || y1 < y0 {
            continue;
        }
        for py in y0..=y1 {
            for px in x0..=x1 {
                let dx = px as f64 + 0.5 - cx;
                let dy = py as f64 + 0.5 - cy;
                if dx * dx + dy * dy > radius_px * radius_px {
                    continue;
                }
                let i = py * TILE_SIZE + px;
                // Mémorisé AVANT d'écraser `top[i]` : après affectation le
                // test dirait toujours « il y avait de la canopée ».
                let had_canopy = top[i] > 0.0;
                if t_top > top[i] {
                    top[i] = t_top;
                    base[i] = if had_canopy { base[i].min(t_base) } else { t_base };
                    leaf[i] = leaf_code(t.leaf_type);
                }
            }
        }
    }

    CanopyTile { top, base, wood: wood_mask, leaf }
}

fn leaf_code(l: crate::osm::LeafType) -> u8 {
    match l {
        crate::osm::LeafType::Broadleaved => 0,
        crate::osm::LeafType::Needleleaved => 1,
        crate::osm::LeafType::Palm => 2,
    }
}

/// Encode la tuile en PNG RGB : R = sommet ×2, G = base ×2, B = classe de
/// végétation (cf. `class_code`) — emprise ou arbre isolé, et silhouette.
pub fn encode_png(tile: &CanopyTile) -> Result<Vec<u8>, image::ImageError> {
    let mut rgb = vec![0u8; TILE_SIZE * TILE_SIZE * 3];
    for i in 0..TILE_SIZE * TILE_SIZE {
        rgb[i * 3] = (tile.top[i] * 2.0).round().clamp(0.0, 255.0) as u8;
        rgb[i * 3 + 1] = (tile.base[i] * 2.0).round().clamp(0.0, 255.0) as u8;
        rgb[i * 3 + 2] = if tile.top[i] > 0.0 {
            class_code(tile.wood[i], tile.leaf[i])
        } else {
            0
        };
    }
    let mut out = Vec::new();
    image::codecs::png::PngEncoder::new(&mut out).write_image(
        &rgb,
        TILE_SIZE as u32,
        TILE_SIZE as u32,
        image::ExtendedColorType::Rgb8,
    )?;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bounds_roundtrip() {
        // Paris z15 : la tuile contenant Notre-Dame doit contenir son centre.
        let (lat, lon): (f64, f64) = (48.853, 2.3499);
        let n = f64::powi(2.0, 15);
        let x = ((lon + 180.0) / 360.0 * n) as u32;
        let lat_rad = lat.to_radians();
        let y = ((1.0 - lat_rad.tan().asinh() / std::f64::consts::PI) / 2.0 * n) as u32;
        let (s, w, nn, e) = tile_bounds(15, x, y);
        assert!(s < lat && lat < nn, "{s} < {lat} < {nn}");
        assert!(w < lon && lon < e, "{w} < {lon} < {e}");
    }

    #[test]
    fn tree_disc_rasterizes_with_trunk_clearance() {
        let (s, w, n, e) = tile_bounds(15, 16596, 11273); // tuile z15 de Paris
        let lat = (s + n) / 2.0;
        let lon = (w + e) / 2.0;
        let tree = Tree {
            osm_id: "node/1".into(),
            lat,
            lng: lon,
            height_m: 10.0,
            crown_radius_m: 3.0,
            leaf_type: crate::osm::LeafType::Broadleaved,
        };
        let tile = rasterize(15, 16596, 11273, &[], &[tree]);
        let lit: Vec<usize> = (0..tile.top.len()).filter(|&i| tile.top[i] > 0.0).collect();
        assert!(!lit.is_empty(), "la couronne doit couvrir des pixels");
        let i = lit[0];
        assert_eq!(tile.top[i], 10.0);
        assert_eq!(tile.base[i], 4.0, "base = sommet − 2×rayon de couronne");
    }

    #[test]
    fn png_roundtrip_values() {
        let mut tile = CanopyTile {
            top: vec![0.0; TILE_SIZE * TILE_SIZE],
            base: vec![0.0; TILE_SIZE * TILE_SIZE],
            wood: vec![false; TILE_SIZE * TILE_SIZE],
            leaf: vec![0; TILE_SIZE * TILE_SIZE],
        };
        tile.top[42] = 18.0;
        tile.base[42] = 2.5;
        tile.wood[42] = true;
        let png = encode_png(&tile).unwrap();
        let img = image::load_from_memory(&png).unwrap().to_rgb8();
        let px = img.get_pixel(42, 0);
        assert_eq!(px[0], 36, "sommet ×2");
        assert_eq!(px[1], 5, "base ×2");
        assert_eq!(px[2], class_code(true, 0), "emprise boisée feuillue");
        // Une tuile quasi vide doit rester minuscule une fois compressée.
        assert!(png.len() < 10_000, "PNG creux : {} octets", png.len());
    }
}
