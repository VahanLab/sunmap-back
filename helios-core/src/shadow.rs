//! Ray marching sur heightmap : le cœur du moteur.
//!
//! Pour savoir si un point est à l'ombre, on « marche » depuis ce point en
//! direction du soleil (projetée au sol). À chaque pas, la hauteur du rayon
//! monte de `distance · tan(élévation)` ; si la DSM opaque (terrain +
//! bâtiments) dépasse cette hauteur, un obstacle bloque le soleil → ombre.
//!
//! La canopée, elle, ne bloque pas : elle **atténue**. Chaque mètre de
//! couronne traversé multiplie la lumière restante par
//! `canopy_transmittance_per_m` ; le point est réputé à l'ombre quand la
//! lumière tombe sous `sunlit_light_threshold`. Un arbre isolé laisse donc
//! passer le soleil sur ses bords, une futaie dense l'éteint.

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
    /// Fraction de lumière conservée par mètre de canopée traversé (mesuré le
    /// long du rayon, pas au sol). 0,6 ≈ feuillu d'alignement en été : ~3 m de
    /// couronne ne laissent passer que ~20 % — ombre — mais 1 m en bord de
    /// houppier en laisse 60 % — soleil. 1,0 = végétation transparente.
    pub canopy_transmittance_per_m: f64,
    /// Lumière directe en deçà de laquelle le point est réputé à l'ombre.
    pub sunlit_light_threshold: f64,
}

impl Default for ShadowParams {
    fn default() -> Self {
        Self {
            max_distance_m: 1_000.0,
            observer_height_m: 0.0,
            step_px: 1.0,
            canopy_transmittance_per_m: 0.6,
            sunlit_light_threshold: 0.25,
        }
    }
}

/// Obstacle rencontré par le rayon : de quoi remonter à *ce qui* fait l'ombre.
///
/// L'appelant sait quelle entité occupe la cellule `(x, y)` (grille de
/// propriétaires côté serveur), et peut donc nommer le bâtiment fautif.
#[derive(Debug, Clone, Copy)]
pub struct ShadowHit {
    /// Cellule DSM (arrondie) où le rayon a été bloqué.
    pub x: usize,
    pub y: usize,
    /// Distance horizontale parcourue depuis le point testé, en mètres.
    pub distance_m: f64,
    /// Altitude de l'obstacle à cette cellule.
    pub obstacle_elevation_m: f32,
    /// Altitude du rayon à cette distance (l'écart avec l'obstacle dit de
    /// combien il manque de soleil).
    pub ray_elevation_m: f64,
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
    !sun.is_up()
        || shadow_hit_from_ground(dsm, sun, px, py, ground, params, dsm.max_elevation()).is_some()
}

/// Variante instrumentée de [`is_shadowed_from_ground`] : renvoie l'obstacle
/// qui bloque le rayon (`None` = au soleil). Même marche, même résultat —
/// c'est l'implémentation de référence, le prédicat booléen n'en est qu'un
/// raccourci.
///
/// Note : soleil couché renvoie `None` (aucun obstacle) alors que le point est
/// bien à l'ombre — d'où le test `is_up()` séparé chez l'appelant.
/// `dsm_max_elevation` : point le plus haut de toute la grille, en fourni par
/// l'appelant plutôt que recalculé ici — `Dsm::max_elevation` scanne toute la
/// grille (O(largeur×hauteur)), et cette fonction tourne une fois par
/// établissement classifié. Le recalculer à chaque appel a fait une requête
/// `/places` passer de ~150 ms à ~2 s sur une zone dense (1074 lieux × un
/// scan complet de la DSM chacun). La grille ne change pas pendant la
/// classification : un seul calcul en amont suffit.
pub fn shadow_hit_from_ground(
    dsm: &Dsm,
    sun: &SunPosition,
    px: f64,
    py: f64,
    ground: f32,
    params: &ShadowParams,
    dsm_max_elevation: f32,
) -> Option<ShadowHit> {
    if !sun.is_up() {
        return None;
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
    let max_useful_m = ((dsm_max_elevation as f64 - z0).max(0.0)) / tan_elev.max(1e-9);
    let max_m = params.max_distance_m.min(max_useful_m);
    let max_steps = (max_m / step_m).ceil() as usize;

    // Longueur 3D d'un pas : au soleil haut, chaque mètre horizontal parcourt
    // bien plus d'un mètre de rayon — c'est cette longueur-là qui traverse la
    // canopée.
    let cos_elev = (sun.elevation_deg * rad).cos().max(1e-6);
    let step_ray_m = step_m / cos_elev;
    let has_canopy = dsm.canopy_top.is_some();
    // ln(τ) une fois pour toutes : accumuler `light *= τ^d` pas à pas revient
    // à sommer `d·ln(τ)` — une addition par pas au lieu d'un `powf`.
    let ln_tau = params.canopy_transmittance_per_m.max(1e-9).ln();
    let ln_threshold = params.sunlit_light_threshold.max(1e-9).ln();
    let mut ln_light = 0.0f64;

    let mut x = px;
    let mut y = py;
    for i in 1..=max_steps {
        x += dx;
        y += dy;
        let Some(h) = dsm.sample(x, y) else {
            // Sorti de la grille sans obstacle : réputé au soleil.
            // (En production : charger la DSM avec une marge — cf. serveur.)
            return None;
        };
        let ray_z = z0 + (i as f64) * step_m * tan_elev;
        if (h as f64) > ray_z {
            return Some(ShadowHit {
                x: x.round().clamp(0.0, (dsm.width - 1) as f64) as usize,
                y: y.round().clamp(0.0, (dsm.height - 1) as f64) as usize,
                distance_m: (i as f64) * step_m,
                obstacle_elevation_m: h,
                ray_elevation_m: ray_z,
            });
        }

        // Canopée : pas d'arrêt, une atténuation. Le rayon doit être DANS la
        // couronne — au-dessus de la base (on passe librement sous le
        // houppier d'un arbre d'alignement), sous le sommet.
        if has_canopy {
            if let Some((base, top)) = dsm.canopy_at(x, y) {
                if ray_z >= base as f64 && ray_z <= top as f64 {
                    ln_light += step_ray_m * ln_tau;
                    if ln_light < ln_threshold {
                        return Some(ShadowHit {
                            x: x.round().clamp(0.0, (dsm.width - 1) as f64) as usize,
                            y: y.round().clamp(0.0, (dsm.height - 1) as f64) as usize,
                            distance_m: (i as f64) * step_m,
                            obstacle_elevation_m: top,
                            ray_elevation_m: ray_z,
                        });
                    }
                }
            }
        }
    }
    None
}

/// Nature d'une cellule rencontrée par le rayon.
///
/// `helios-core` ne connaît que des altitudes : distinguer relief et bâtiment,
/// ou arbre isolé et emprise boisée, demande la grille d'occupation que seul
/// l'appelant possède.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Cause {
    /// Obstacle opaque.
    Opaque,
    /// Cellule de canopée traversée.
    Canopy,
}

/// Ce que le rayon a rencontré sur toute sa longueur.
#[derive(Clone, Copy, Debug, Default)]
pub struct ShadowCauses {
    /// Un obstacle opaque coupe le rayon.
    pub opaque_blocked: bool,
    /// La canopée a éteint le soleil à elle seule : transmittance cumulée
    /// passée sous le seuil.
    pub canopy_extinguished: bool,
}

impl ShadowCauses {
    pub fn shadowed(&self) -> bool {
        self.opaque_blocked || self.canopy_extinguished
    }
}

/// Parcourt le rayon **en entier** et signale chaque cellule qui l'ombre, au
/// lieu de s'arrêter au premier obstacle comme [`shadow_hit_from_ground`].
///
/// L'arrêt anticipé suffit à répondre « suis-je à l'ombre », et c'est lui qui
/// rend la classification en masse rapide. Il ne suffit pas à répondre « de
/// QUOI suis-je à l'ombre » : l'arbre au-dessus de la tête n'efface pas la
/// montagne qui, plus loin, ombre toute la vallée. On voit donc tout le
/// rayon, et l'appelant arbitre.
///
/// À réserver aux requêtes ponctuelles — le parcours est complet par
/// construction, là où `/places` doit garder son early-exit.
pub fn shadow_causes_from_ground(
    dsm: &Dsm,
    sun: &SunPosition,
    px: f64,
    py: f64,
    ground: f32,
    params: &ShadowParams,
    dsm_max_elevation: f32,
    // Coordonnées FRACTIONNAIRES : `sample` interpole sur quatre cellules, et
    // l'appelant doit pouvoir consulter les mêmes. Arrondir ici lui ferait
    // attribuer au relief un obstacle dont seul le bord de bâtiment voisin
    // dépasse le rayon.
    mut on_cause: impl FnMut(Cause, f64, f64),
) -> ShadowCauses {
    let mut out = ShadowCauses::default();
    if !sun.is_up() {
        return out;
    }
    let z0 = ground as f64 + params.observer_height_m;

    let rad = std::f64::consts::PI / 180.0;
    let az = sun.azimuth_deg * rad;
    let tan_elev = (sun.elevation_deg * rad).tan();
    let dx = az.sin() * params.step_px;
    let dy = -az.cos() * params.step_px;
    let step_m = params.step_px * dsm.meters_per_pixel;

    let max_useful_m = ((dsm_max_elevation as f64 - z0).max(0.0)) / tan_elev.max(1e-9);
    let max_m = params.max_distance_m.min(max_useful_m);
    let max_steps = (max_m / step_m).ceil() as usize;

    let cos_elev = (sun.elevation_deg * rad).cos().max(1e-6);
    let step_ray_m = step_m / cos_elev;
    let has_canopy = dsm.canopy_top.is_some();
    let ln_tau = params.canopy_transmittance_per_m.max(1e-9).ln();
    let ln_threshold = params.sunlit_light_threshold.max(1e-9).ln();
    let mut ln_light = 0.0f64;

    let (mut x, mut y) = (px, py);
    for i in 1..=max_steps {
        x += dx;
        y += dy;
        let Some(h) = dsm.sample(x, y) else { break };
        let ray_z = z0 + (i as f64) * step_m * tan_elev;

        if (h as f64) > ray_z {
            // Signalé puis dépassé : ce qui est derrière ombre tout autant, et
            // c'est souvent ce qui ombre le PLUS (une crête derrière un mur).
            out.opaque_blocked = true;
            on_cause(Cause::Opaque, x, y);
            continue;
        }

        if has_canopy {
            if let Some((base, top)) = dsm.canopy_at(x, y) {
                if ray_z >= base as f64 && ray_z <= top as f64 {
                    // Toutes les cellules traversées sont signalées, y compris
                    // avant l'extinction : ce sont elles qui la causent, et
                    // l'appelant n'en tiendra compte que si elle survient.
                    on_cause(Cause::Canopy, x, y);
                    ln_light += step_ray_m * ln_tau;
                    if ln_light < ln_threshold {
                        out.canopy_extinguished = true;
                    }
                }
            }
        }
    }
    out
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

    /// Tamponne une bande de canopée (colonnes x0..=x1, toutes les lignes)
    /// entre deux altitudes.
    fn stamp_canopy_band(dsm: &mut Dsm, x0: usize, x1: usize, base: f32, top: f32) {
        let width = dsm.width;
        let (tops, bases) = dsm.canopy_layers_mut();
        for i in 0..tops.len() {
            let x = i % width;
            if x >= x0 && x <= x1 {
                tops[i] = top;
                bases[i] = base;
            }
        }
    }

    /// Une couronne étroite atténue sans éteindre : soleil. Une bande large
    /// accumule au-delà du seuil : ombre. C'est toute la différence avec
    /// l'ancien tamponnage opaque, où le moindre houppier bloquait net.
    #[test]
    fn canopy_attenuates_instead_of_blocking() {
        let sun = SunPosition {
            azimuth_deg: 270.0, // soleil à l'ouest → rayon marche vers l'ouest
            elevation_deg: 45.0,
        };
        let p = ShadowParams::default(); // τ=0.6/m, seuil 25 %

        // Couronne de 2 m de large (x=48..=49), de 3 à 10 m d'altitude.
        // Rayon depuis x=52 à 45° : traverse ~2 m horizontaux de couronne
        // → ~2.8 m de rayon → 0.6^2.8 ≈ 24 %… juste sous le seuil. Prenons
        // 1 cellule (x=49) : ~1.4 m de rayon → 49 % → soleil.
        let mut dsm = Dsm::flat(100, 100, 1.0, 0.0);
        stamp_canopy_band(&mut dsm, 49, 49, 3.0, 10.0);
        assert!(
            !is_shadowed(&dsm, &sun, 52.0, 50.0, &ShadowParams { observer_height_m: 1.5, ..p }),
            "une cellule de couronne doit laisser passer le soleil"
        );

        // Bande boisée de 20 cellules : extinction garantie.
        let mut dsm = Dsm::flat(100, 100, 1.0, 0.0);
        stamp_canopy_band(&mut dsm, 30, 49, 0.0, 18.0);
        assert!(
            is_shadowed(&dsm, &sun, 52.0, 50.0, &ShadowParams { observer_height_m: 1.5, ..p }),
            "20 m de futaie doivent éteindre le rayon"
        );
    }

    /// Sous la base de la couronne, le rayon passe librement : un observateur
    /// dont le rayon file sous le houppier voit le soleil rasant.
    #[test]
    fn ray_passes_under_crown_base() {
        let sun = SunPosition {
            azimuth_deg: 270.0,
            elevation_deg: 2.0, // très rasant : le rayon reste bas
        };
        let mut dsm = Dsm::flat(100, 100, 1.0, 0.0);
        stamp_canopy_band(&mut dsm, 40, 45, 4.0, 10.0); // base à 4 m
        let p = ShadowParams {
            observer_height_m: 1.5,
            ..ShadowParams::default()
        };
        // Le rayon part de 1.5 m et ne monte qu'à ~1.9 m au niveau de la
        // bande : sous la base, aucune atténuation.
        assert!(!is_shadowed(&dsm, &sun, 52.0, 50.0, &p));
    }

    /// La canopée compte dans `max_elevation` : sans ça, une futaie plus
    /// haute que tout bâtiment serait ignorée par l'early-exit du rayon.
    #[test]
    fn canopy_counts_in_max_elevation() {
        let mut dsm = Dsm::flat(10, 10, 1.0, 0.0);
        stamp_canopy_band(&mut dsm, 2, 3, 0.0, 25.0);
        assert_eq!(dsm.max_elevation(), 25.0);
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
