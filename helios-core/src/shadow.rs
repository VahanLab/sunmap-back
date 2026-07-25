//! Ray marching sur heightmap : le cœur du moteur.
//!
//! Pour savoir si un point est à l'ombre, on « marche » depuis ce point en
//! direction du soleil (projetée au sol). À chaque pas, la hauteur du rayon
//! monte de `distance · tan(élévation)` ; si la DSM dépasse cette hauteur,
//! un obstacle bloque le soleil → ombre.

use crate::dsm::Dsm;
use crate::sun::SunPosition;

/// Paramètres du calcul d'ombre.
#[derive(Debug, Clone, Copy)]
pub struct ShadowParams {
    /// Distance de recherche maximale en mètres. En ville, 500 m suffisent pour
    /// les bâtiments ; monte à 2–5 km si le relief environnant compte
    /// (la borne réelle est de toute façon recoupée avec `max_elevation`).
    pub max_distance_m: f64,
    /// Hauteur de l'observateur au-dessus de la DSM (0 = le sol lui-même ;
    /// 1.5 m ≈ une personne attablée en terrasse).
    pub observer_height_m: f64,
    /// Pas de marche en fraction de cellule (1.0 = une cellule ; descendre à
    /// 0.5 pour plus de précision au prix de 2× le coût).
    pub step_px: f64,
}

impl Default for ShadowParams {
    fn default() -> Self {
        Self {
            max_distance_m: 1_000.0,
            observer_height_m: 0.0,
            step_px: 1.0,
        }
    }
}

/// Le point `(px, py)` (coordonnées pixel de la DSM) est-il à l'ombre ?
///
/// Retourne aussi `true` hors grille si le soleil est couché ; un point de
/// départ hors grille avec soleil levé renvoie `false` (aucune donnée).
pub fn is_shadowed(dsm: &Dsm, sun: &SunPosition, px: f64, py: f64, params: &ShadowParams) -> bool {
    if !sun.is_up() {
        return true;
    }
    let Some(ground) = dsm.sample(px, py) else {
        return false;
    };
    is_shadowed_from_ground(dsm, sun, px, py, ground, params)
}

/// Variante de [`is_shadowed`] avec une altitude de sol fournie explicitement
/// plutôt qu'échantillonnée dans `dsm`. Utile quand la DSM d'obstacles
/// (relief + bâtiments) ne doit pas servir à définir la position de
/// l'observateur lui-même — cas d'un POI dont les coordonnées tombent par
/// erreur à l'intérieur d'un bâtiment dans la donnée source (OSM) : sans
/// cette séparation, le point hérite à tort de l'altitude du toit et
/// « voit par-dessus » des obstacles qui devraient le masquer.
pub fn is_shadowed_from_ground(
    dsm: &Dsm,
    sun: &SunPosition,
    px: f64,
    py: f64,
    ground: f32,
    params: &ShadowParams,
) -> bool {
    if !sun.is_up() {
        return true;
    }
    let z0 = ground as f64 + params.observer_height_m;

    let rad = std::f64::consts::PI / 180.0;
    let az = sun.azimuth_deg * rad;
    let tan_elev = (sun.elevation_deg * rad).tan();

    // Direction AU SOL vers le soleil, dans le repère raster (x est, y sud) :
    // est = sin(az), nord = cos(az) → dy = −cos(az) car y croît vers le sud.
    let dx = az.sin() * params.step_px;
    let dy = -az.cos() * params.step_px;
    let step_m = params.step_px * dsm.meters_per_pixel;

    // Au-delà de cette distance, même le point le plus haut de la grille
    // passe sous le rayon : inutile de marcher plus loin.
    let max_useful_m = ((dsm.max_elevation() as f64 - z0).max(0.0)) / tan_elev.max(1e-9);
    let max_m = params.max_distance_m.min(max_useful_m);
    let max_steps = (max_m / step_m).ceil() as usize;

    let mut x = px;
    let mut y = py;
    for i in 1..=max_steps {
        x += dx;
        y += dy;
        let Some(h) = dsm.sample(x, y) else {
            // Sorti de la grille sans obstacle : réputé au soleil.
            // (En production : charger la DSM avec une marge — cf. serveur.)
            return false;
        };
        let ray_z = z0 + (i as f64) * step_m * tan_elev;
        if (h as f64) > ray_z {
            return true;
        }
    }
    false
}

/// Rend le masque d'ombre de toute la grille : `255` = ombre, `0` = soleil.
///
/// Boucle naïvement sur les pixels — chaque ligne est indépendante, donc la
/// parallélisation avec `rayon` est un one-liner (`par_chunks_mut` sur les
/// lignes) quand on branchera le crate côté serveur.
pub fn render_mask(dsm: &Dsm, sun: &SunPosition, params: &ShadowParams) -> Vec<u8> {
    let mut mask = vec![0u8; dsm.width * dsm.height];
    if !sun.is_up() {
        mask.fill(255);
        return mask;
    }
    for y in 0..dsm.height {
        for x in 0..dsm.width {
            if is_shadowed(dsm, sun, x as f64, y as f64, params) {
                mask[y * dsm.width + x] = 255;
            }
        }
    }
    mask
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sun::SunPosition;

    /// Mur nord-sud de 10 m de haut à x=50, soleil plein ouest à 45°.
    /// L'ombre s'étend exactement sur 10 m à l'est du mur.
    #[test]
    fn wall_casts_expected_shadow() {
        let mut dsm = Dsm::flat(100, 100, 1.0, 0.0);
        dsm.stamp_max(50, 0, 50, 99, 10.0);
        let sun = SunPosition {
            azimuth_deg: 270.0, // soleil à l'ouest → ombre vers l'est
            elevation_deg: 45.0,
        };
        let p = ShadowParams::default();

        // 5 m à l'est du mur : rayon à 5 m de haut face à un mur de 10 m → ombre.
        assert!(is_shadowed(&dsm, &sun, 55.0, 50.0, &p));
        // 15 m à l'est : le rayon passe à 15 m > 10 m → soleil.
        assert!(!is_shadowed(&dsm, &sun, 65.0, 50.0, &p));
        // À l'ouest du mur, rien entre le point et le soleil → soleil.
        assert!(!is_shadowed(&dsm, &sun, 40.0, 50.0, &p));
    }

    /// Soleil plus bas (26.57° → tan ≈ 0.5) : l'ombre du mur double (20 m).
    #[test]
    fn lower_sun_longer_shadow() {
        let mut dsm = Dsm::flat(100, 100, 1.0, 0.0);
        dsm.stamp_max(50, 0, 50, 99, 10.0);
        let sun = SunPosition {
            azimuth_deg: 270.0,
            elevation_deg: 26.565, // tan = 0.5
        };
        let p = ShadowParams::default();
        assert!(is_shadowed(&dsm, &sun, 65.0, 50.0, &p)); // 15 m < 20 m
        assert!(!is_shadowed(&dsm, &sun, 75.0, 50.0, &p)); // 25 m > 20 m
    }

    /// L'azimut oriente bien l'ombre : soleil au sud → ombre au nord du mur.
    #[test]
    fn azimuth_orientation() {
        let mut dsm = Dsm::flat(100, 100, 1.0, 0.0);
        dsm.stamp_max(0, 50, 99, 50, 10.0); // mur est-ouest à y=50
        let sun = SunPosition {
            azimuth_deg: 180.0, // soleil au sud
            elevation_deg: 45.0,
        };
        let p = ShadowParams::default();
        // y croît vers le sud : le nord du mur = y < 50.
        assert!(is_shadowed(&dsm, &sun, 50.0, 45.0, &p)); // 5 m au nord → ombre
        assert!(!is_shadowed(&dsm, &sun, 50.0, 55.0, &p)); // au sud → soleil
    }

    /// Soleil couché : tout est à l'ombre, y compris le masque complet.
    #[test]
    fn night_everything_shadowed() {
        let dsm = Dsm::flat(8, 8, 1.0, 0.0);
        let sun = SunPosition {
            azimuth_deg: 0.0,
            elevation_deg: -3.0,
        };
        let mask = render_mask(&dsm, &sun, &ShadowParams::default());
        assert!(mask.iter().all(|&v| v == 255));
    }

    /// La hauteur d'observateur sort un point de l'ombre limite.
    #[test]
    fn observer_height_matters() {
        let mut dsm = Dsm::flat(100, 100, 1.0, 0.0);
        dsm.stamp_max(50, 0, 50, 99, 10.0);
        let sun = SunPosition {
            azimuth_deg: 270.0,
            elevation_deg: 45.0,
        };
        let ground = ShadowParams::default();
        let seated = ShadowParams {
            observer_height_m: 1.5,
            ..ground
        };
        // À 9 m du mur : le sol est à l'ombre (rayon à 9 m < 10 m)…
        assert!(is_shadowed(&dsm, &sun, 59.0, 50.0, &ground));
        // …mais une tête à 1.5 m voit le soleil (1.5 + 9 > 10).
        assert!(!is_shadowed(&dsm, &sun, 59.0, 50.0, &seated));
    }
}
