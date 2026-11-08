//! Façades d'un bâtiment : de l'emprise OSM aux points où l'on mesure le
//! soleil d'un appartement.
//!
//! Module **pur** (aucune I/O, aucune DSM) : il travaille en pixels monde
//! Web Mercator au zoom de calcul (`dem::world_px`, x vers l'est, y vers le
//! sud — le repère de la DSM), convertis en mètres par `meters_per_px`.
//!
//! Le découpage :
//! - chaque anneau du polygone (extérieur ET cours) est parcouru arête par
//!   arête, les arêtes quasi alignées sont fusionnées en une façade ;
//! - la normale sortante est celle qui pointe hors du bâti (test pair-impair,
//!   donc vers la cour pour un anneau intérieur) ;
//! - une façade collée au voisin (mur mitoyen) n'est pas une façade : elle
//!   est écartée par l'appelant via [`is_party_wall`] ;
//! - la grille étages × tronçons donne les points d'observation, décalés hors
//!   du mur pour que le rayon ne heurte pas son propre bâtiment.

use crate::dem::{latlon_of_world_px, world_px, TILE_SIZE, ZOOM};
use crate::osm::Building;

/// Deux arêtes consécutives appartiennent à la même façade si leur direction
/// s'écarte de moins de ce seuil de la corde du groupe. Les polygones OSM sont
/// pleins de sommets quasi alignés (tracé à la main, imports cadastraux).
pub const MERGE_ANGLE_DEG: f64 = 15.0;
/// En deçà, ce n'est pas une façade habitable mais un décroché de mur.
pub const MIN_FACADE_M: f64 = 3.0;
/// Largeur visée d'un tronçon de la grille.
pub const COLUMN_WIDTH_M: f64 = 8.0;
pub const MAX_COLUMNS: usize = 6;
/// Hauteur d'étage conventionnelle (la même que l'import : `levels × 3 + 3`).
pub const FLOOR_HEIGHT_M: f64 = 3.0;
/// Hauteur de la toiture ajoutée à l'import au-dessus du dernier étage.
pub const ROOF_ALLOWANCE_M: f64 = 3.0;
/// Hauteur de l'œil au-dessus du plancher : milieu de fenêtre.
pub const WINDOW_HEIGHT_M: f64 = 1.5;
/// Décalage du point d'observation hors du mur : un peu plus d'une cellule
/// DSM (~1,57 m à Paris), sinon le rayon part de l'intérieur de son propre
/// bâtiment — le piège déjà rencontré avec `nudge_out_of_building`.
pub const FACADE_OFFSET_M: f64 = 2.0;
pub const MAX_LEVELS: u32 = 60;

/// Taille d'un pixel monde (zoom de calcul) en mètres, à cette latitude.
pub fn meters_per_px(lat: f64) -> f64 {
    40_075_016.686 * lat.to_radians().cos() / ((TILE_SIZE as f64) * f64::powi(2.0, ZOOM as i32))
}

/// Nombre d'étages déduit de la hauteur, à défaut de mieux : l'inverse de la
/// règle d'import (`levels × 3 m + 3 m` de toiture). Au moins un.
pub fn estimated_levels(height_m: f32) -> u32 {
    (((height_m as f64 - ROOF_ALLOWANCE_M) / FLOOR_HEIGHT_M).round().max(1.0) as u32).min(MAX_LEVELS)
}

/// Hauteur d'observation de l'étage `level` (0 = rez-de-chaussée) au-dessus
/// du sol.
pub fn level_height_m(level: u32) -> f64 {
    level as f64 * FLOOR_HEIGHT_M + WINDOW_HEIGHT_M
}

/// Libellé en 8 directions, en français (O = ouest).
pub fn orientation_label(azimuth_deg: f64) -> &'static str {
    const LABELS: [&str; 8] = ["N", "NE", "E", "SE", "S", "SO", "O", "NO"];
    LABELS[(((azimuth_deg.rem_euclid(360.0) + 22.5) / 45.0) as usize) % 8]
}

/// Anneaux d'un bâtiment en pixels monde.
pub fn rings_px(b: &Building) -> Vec<Vec<(f64, f64)>> {
    b.rings
        .iter()
        .map(|r| r.iter().map(|&(lat, lon)| world_px(lat, lon)).collect())
        .collect()
}

/// Le point est-il dans un anneau (règle pair-impair) ?
fn in_ring(ring: &[(f64, f64)], x: f64, y: f64) -> bool {
    let mut inside = false;
    let n = ring.len();
    for i in 0..n {
        let (x1, y1) = ring[i];
        let (x2, y2) = ring[(i + 1) % n];
        if (y1 > y) != (y2 > y) && x < x1 + (y - y1) / (y2 - y1) * (x2 - x1) {
            inside = !inside;
        }
    }
    inside
}

/// Le point est-il dans le bâti (pair-impair sur tous les anneaux : une cour
/// est dehors) ? Même règle que la rasterisation `stamp_buildings`.
pub fn point_in_rings(rings: &[Vec<(f64, f64)>], x: f64, y: f64) -> bool {
    rings.iter().filter(|r| r.len() >= 3 && in_ring(r, x, y)).count() % 2 == 1
}

fn ring_area(ring: &[(f64, f64)]) -> f64 {
    let n = ring.len();
    (0..n)
        .map(|i| {
            let (x1, y1) = ring[i];
            let (x2, y2) = ring[(i + 1) % n];
            x1 * y2 - x2 * y1
        })
        .sum::<f64>()
        .abs()
        / 2.0
}

/// Le bâtiment sous le point : parmi les emprises qui le contiennent, la plus
/// grande. Un `building` et ses `building:part` se superposent ; c'est le
/// contour extérieur qui porte les façades.
pub fn building_at(buildings: &[Building], lat: f64, lng: f64) -> Option<usize> {
    let (x, y) = world_px(lat, lng);
    buildings
        .iter()
        .enumerate()
        .filter_map(|(i, b)| {
            let rings = rings_px(b);
            point_in_rings(&rings, x, y).then(|| {
                let area = rings.iter().map(|r| ring_area(r)).fold(0.0, f64::max);
                (i, area)
            })
        })
        .max_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal))
        .map(|(i, _)| i)
}

/// Une façade : une portion d'anneau quasi rectiligne, orientée.
#[derive(Clone, Debug)]
pub struct Facade {
    /// Sommets en pixels monde, dans l'ordre de l'anneau.
    pub line_px: Vec<(f64, f64)>,
    /// Normale sortante unitaire, repère raster (x est, y sud).
    pub normal: (f64, f64),
    /// Azimut de la normale, degrés depuis le nord, sens horaire.
    pub azimuth_deg: f64,
    pub length_m: f64,
    /// Façade sur cour (anneau intérieur).
    pub courtyard: bool,
}

impl Facade {
    /// Sommets en `[lat, lng]`, pour le tracé côté client.
    pub fn line_latlng(&self) -> Vec<[f64; 2]> {
        self.line_px
            .iter()
            .map(|&(x, y)| {
                let (lat, lng) = latlon_of_world_px(x, y);
                [lat, lng]
            })
            .collect()
    }

    /// Nombre de tronçons de la grille.
    pub fn columns(&self) -> usize {
        ((self.length_m / COLUMN_WIDTH_M).round() as usize).clamp(1, MAX_COLUMNS)
    }

    /// Point du mur à l'abscisse curviligne `fraction` (0 → 1) de la façade.
    fn point_along(&self, fraction: f64) -> (f64, f64) {
        let lengths: Vec<f64> = self
            .line_px
            .windows(2)
            .map(|w| (w[1].0 - w[0].0).hypot(w[1].1 - w[0].1))
            .collect();
        let mut remaining = fraction * lengths.iter().sum::<f64>();
        for (w, len) in self.line_px.windows(2).zip(&lengths) {
            if remaining <= *len && *len > 0.0 {
                let t = remaining / len;
                return (w[0].0 + t * (w[1].0 - w[0].0), w[0].1 + t * (w[1].1 - w[0].1));
            }
            remaining -= len;
        }
        *self.line_px.last().expect("façade non vide")
    }

    /// Points d'observation (pixels monde), un par tronçon, au milieu de
    /// chacun et décalés de [`FACADE_OFFSET_M`] hors du mur.
    pub fn observation_points(&self, meters_per_px: f64) -> Vec<(f64, f64)> {
        let n = self.columns();
        let offset = FACADE_OFFSET_M / meters_per_px;
        (0..n)
            .map(|k| {
                let (x, y) = self.point_along((k as f64 + 0.5) / n as f64);
                (x + self.normal.0 * offset, y + self.normal.1 * offset)
            })
            .collect()
    }

    /// Le soleil d'azimut donné est-il devant la façade ? Derrière (90° ou
    /// plus de la normale), elle est à l'ombre sans même lancer de rayon.
    pub fn faces_sun(&self, sun_azimuth_deg: f64) -> bool {
        let az = sun_azimuth_deg.to_radians();
        // Direction au sol vers le soleil dans le repère raster.
        let (sx, sy) = (az.sin(), -az.cos());
        sx * self.normal.0 + sy * self.normal.1 > 0.0
    }
}

/// Azimut (degrés depuis le nord, horaire) d'un vecteur du repère raster.
fn azimuth_of(dx: f64, dy: f64) -> f64 {
    dx.atan2(-dy).to_degrees().rem_euclid(360.0)
}

/// Écart angulaire entre deux directions, dans [0, 180].
fn angle_between(a: (f64, f64), b: (f64, f64)) -> f64 {
    let d = (a.1.atan2(a.0) - b.1.atan2(b.0)).to_degrees().rem_euclid(360.0);
    if d > 180.0 { 360.0 - d } else { d }
}

/// Anneau nettoyé : sans sommet de fermeture répété ni doublon consécutif.
fn clean_ring(ring: &[(f64, f64)]) -> Vec<(f64, f64)> {
    let mut out: Vec<(f64, f64)> = Vec::with_capacity(ring.len());
    for &p in ring {
        if out.last().is_none_or(|q: &(f64, f64)| (q.0 - p.0).hypot(q.1 - p.1) > 1e-9) {
            out.push(p);
        }
    }
    while out.len() > 1 {
        let (a, b) = (out[0], out[out.len() - 1]);
        if (a.0 - b.0).hypot(a.1 - b.1) > 1e-9 {
            break;
        }
        out.pop();
    }
    out
}

/// Découpe un anneau en groupes de sommets quasi alignés.
fn split_ring(ring: &[(f64, f64)]) -> Vec<Vec<(f64, f64)>> {
    let n = ring.len();
    let dir = |i: usize| {
        let (a, b) = (ring[i % n], ring[(i + 1) % n]);
        (b.0 - a.0, b.1 - a.1)
    };
    // Démarrer sur le vrai angle le plus marqué : partir du sommet 0, souvent
    // posé au milieu d'un mur, couperait cette façade en deux.
    let start = (0..n)
        .max_by(|&i, &j| {
            let ti = angle_between(dir(i + n - 1), dir(i));
            let tj = angle_between(dir(j + n - 1), dir(j));
            ti.partial_cmp(&tj).unwrap_or(std::cmp::Ordering::Equal)
        })
        .unwrap_or(0);

    let mut groups: Vec<Vec<(f64, f64)>> = Vec::new();
    let mut current = vec![ring[start]];
    for k in 0..n {
        let i = start + k;
        let next = ring[(i + 1) % n];
        let edge = dir(i);
        if current.len() >= 2 {
            let first = current[0];
            let last = *current.last().unwrap();
            let chord = (last.0 - first.0, last.1 - first.1);
            if angle_between(chord, edge) >= MERGE_ANGLE_DEG {
                groups.push(std::mem::replace(&mut current, vec![last]));
            }
        }
        current.push(next);
    }
    groups.push(current);
    groups
}

/// Toutes les façades d'un bâtiment, murs mitoyens compris (le tri revient à
/// l'appelant, qui connaît les voisins). Ordre déterministe : anneaux dans
/// l'ordre de l'archive, groupes dans l'ordre du polygone.
pub fn facades(b: &Building) -> Vec<Facade> {
    let rings = rings_px(b);
    let Some(lat0) = b.rings.iter().flatten().map(|p| p.0).next() else {
        return Vec::new();
    };
    let mpp = meters_per_px(lat0);
    let probe = 0.5 / mpp;
    let mut out = Vec::new();

    for (ri, raw) in rings.iter().enumerate() {
        let ring = clean_ring(raw);
        if ring.len() < 3 {
            continue;
        }
        // Un anneau contenu dans un nombre impair d'autres anneaux est une cour.
        let courtyard = rings
            .iter()
            .enumerate()
            .filter(|&(j, r)| j != ri && r.len() >= 3 && in_ring(r, ring[0].0, ring[0].1))
            .count()
            % 2
            == 1;

        for line in split_ring(&ring) {
            let length_px: f64 = line.windows(2).map(|w| (w[1].0 - w[0].0).hypot(w[1].1 - w[0].1)).sum();
            let length_m = length_px * mpp;
            if length_m < MIN_FACADE_M {
                continue;
            }
            let (first, last) = (line[0], *line.last().unwrap());
            let chord = (last.0 - first.0, last.1 - first.1);
            let norm = chord.0.hypot(chord.1);
            if norm == 0.0 {
                continue;
            }
            let mut normal = (-chord.1 / norm, chord.0 / norm);
            let mut facade = Facade {
                line_px: line,
                normal,
                azimuth_deg: 0.0,
                length_m,
                courtyard,
            };
            let (mx, my) = facade.point_along(0.5);
            if point_in_rings(&rings, mx + normal.0 * probe, my + normal.1 * probe) {
                normal = (-normal.0, -normal.1);
            }
            facade.normal = normal;
            facade.azimuth_deg = azimuth_of(normal.0, normal.1);
            out.push(facade);
        }
    }
    out
}

/// Mur mitoyen : la majorité des points d'observation (au sol, à
/// [`FACADE_OFFSET_M`] du mur) tombe dans un autre bâtiment.
/// `occupied(x, y)` (pixels monde) dit si un bâtiment voisin occupe ce point.
pub fn is_party_wall(facade: &Facade, meters_per_px: f64, occupied: impl Fn(f64, f64) -> bool) -> bool {
    let points = facade.observation_points(meters_per_px);
    let blocked = points.iter().filter(|&&(x, y)| occupied(x, y)).count();
    blocked * 2 > points.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    const LAT: f64 = 48.85;
    const LNG: f64 = 2.35;

    /// Décalage en degrés pour `m` mètres vers le nord / l'est.
    fn north(m: f64) -> f64 {
        m / 111_320.0
    }
    fn east(m: f64) -> f64 {
        m / (111_320.0 * LAT.to_radians().cos())
    }

    /// Rectangle de `w` m (est-ouest) × `h` m (nord-sud), coin sud-ouest en
    /// (`x0`, `y0`) mètres de l'origine.
    fn rect(x0: f64, y0: f64, w: f64, h: f64) -> Vec<(f64, f64)> {
        vec![
            (LAT + north(y0), LNG + east(x0)),
            (LAT + north(y0), LNG + east(x0 + w)),
            (LAT + north(y0 + h), LNG + east(x0 + w)),
            (LAT + north(y0 + h), LNG + east(x0)),
            (LAT + north(y0), LNG + east(x0)),
        ]
    }

    fn building(id: &str, rings: Vec<Vec<(f64, f64)>>, height: f32) -> Building {
        Building {
            osm_id: id.into(),
            name: None,
            rings,
            height_m: height,
            height_from_osm: false,
            leaf_type: None,
        }
    }

    fn labels(fs: &[Facade]) -> Vec<&'static str> {
        let mut l: Vec<_> = fs.iter().map(|f| orientation_label(f.azimuth_deg)).collect();
        l.sort();
        l
    }

    #[test]
    fn rectangle_has_four_cardinal_facades() {
        let b = building("way/1", vec![rect(0.0, 0.0, 20.0, 12.0)], 21.0);
        let fs = facades(&b);
        assert_eq!(fs.len(), 4);
        assert_eq!(labels(&fs), vec!["E", "N", "O", "S"]);
        for f in &fs {
            let expected = if matches!(orientation_label(f.azimuth_deg), "N" | "S") { 20.0 } else { 12.0 };
            assert!((f.length_m - expected).abs() < 0.2, "{} : {}", f.azimuth_deg, f.length_m);
            assert!(!f.courtyard);
            // Normale exacte au degré près.
            let snapped = (f.azimuth_deg / 90.0).round() * 90.0;
            assert!((f.azimuth_deg - snapped).abs() < 0.5);
        }
    }

    #[test]
    fn winding_order_does_not_change_outward_normals() {
        let mut ring = rect(0.0, 0.0, 20.0, 12.0);
        ring.reverse();
        let fs = facades(&building("way/1", vec![ring], 21.0));
        assert_eq!(labels(&fs), vec!["E", "N", "O", "S"]);
    }

    #[test]
    fn courtyard_facades_face_into_the_courtyard() {
        let outer = rect(0.0, 0.0, 40.0, 40.0);
        let inner = rect(15.0, 15.0, 10.0, 10.0);
        let fs = facades(&building("relation/2", vec![outer, inner], 21.0));
        assert_eq!(fs.len(), 8);
        let court: Vec<&Facade> = fs.iter().filter(|f| f.courtyard).collect();
        assert_eq!(court.len(), 4);
        // La façade sur cour du côté nord de la cour regarde vers le sud.
        let north_side = court
            .iter()
            .find(|f| {
                let y25 = world_px(LAT + north(25.0), LNG).1;
                f.line_px.iter().all(|p| (p.1 - y25).abs() < 0.1)
            })
            .expect("côté nord de la cour");
        assert_eq!(orientation_label(north_side.azimuth_deg), "S");
    }

    #[test]
    fn nearly_collinear_vertices_are_merged_from_a_real_corner() {
        // Rectangle dont le côté sud porte deux sommets intermédiaires
        // légèrement désaxés, et dont l'anneau démarre au milieu du côté sud.
        let r = rect(0.0, 0.0, 30.0, 12.0);
        let ring = vec![
            (LAT + north(0.2), LNG + east(10.0)),
            (LAT + north(-0.2), LNG + east(20.0)),
            r[1],
            r[2],
            r[3],
            r[0],
            (LAT + north(0.2), LNG + east(10.0)),
        ];
        let fs = facades(&building("way/3", vec![ring], 12.0));
        assert_eq!(labels(&fs), vec!["E", "N", "O", "S"]);
        let south = fs.iter().find(|f| orientation_label(f.azimuth_deg) == "S").unwrap();
        assert!((south.length_m - 30.0).abs() < 0.3, "{}", south.length_m);
    }

    #[test]
    fn short_wall_offsets_are_dropped() {
        // Décroché de 1 m sur la façade est : il ne fait pas une façade.
        let ring = vec![
            (LAT, LNG),
            (LAT, LNG + east(20.0)),
            (LAT + north(6.0), LNG + east(20.0)),
            (LAT + north(6.0), LNG + east(21.0)),
            (LAT + north(12.0), LNG + east(21.0)),
            (LAT + north(12.0), LNG),
            (LAT, LNG),
        ];
        let fs = facades(&building("way/4", vec![ring], 12.0));
        assert!(fs.iter().all(|f| f.length_m >= MIN_FACADE_M));
        assert_eq!(fs.iter().filter(|f| orientation_label(f.azimuth_deg) == "E").count(), 2);
    }

    #[test]
    fn party_wall_is_detected_against_the_neighbour() {
        let me = building("way/1", vec![rect(0.0, 0.0, 20.0, 12.0)], 21.0);
        let neighbour = building("way/2", vec![rect(20.0, 0.0, 15.0, 12.0)], 18.0);
        let neighbour_rings = rings_px(&neighbour);
        let mpp = meters_per_px(LAT);
        let fs = facades(&me);
        let party: Vec<&str> = fs
            .iter()
            .filter(|f| is_party_wall(f, mpp, |x, y| point_in_rings(&neighbour_rings, x, y)))
            .map(|f| orientation_label(f.azimuth_deg))
            .collect();
        assert_eq!(party, vec!["E"]);
    }

    #[test]
    fn observation_points_sit_outside_the_building() {
        let b = building("way/1", vec![rect(0.0, 0.0, 40.0, 12.0)], 21.0);
        let rings = rings_px(&b);
        let mpp = meters_per_px(LAT);
        for f in facades(&b) {
            let points = f.observation_points(mpp);
            assert_eq!(points.len(), f.columns());
            for (x, y) in points {
                assert!(!point_in_rings(&rings, x, y));
            }
        }
        let south = facades(&b).into_iter().find(|f| orientation_label(f.azimuth_deg) == "S").unwrap();
        assert_eq!(south.columns(), 5);
    }

    #[test]
    fn south_facade_faces_the_noon_sun_only() {
        let b = building("way/1", vec![rect(0.0, 0.0, 20.0, 12.0)], 21.0);
        let south = facades(&b).into_iter().find(|f| orientation_label(f.azimuth_deg) == "S").unwrap();
        assert!(south.faces_sun(180.0));
        assert!(south.faces_sun(100.0));
        assert!(!south.faces_sun(80.0));
        assert!(!south.faces_sun(0.0));
    }

    #[test]
    fn largest_footprint_wins_at_a_point() {
        let outline = building("way/10", vec![rect(0.0, 0.0, 30.0, 30.0)], 15.0);
        let part = building("way/11", vec![rect(5.0, 5.0, 10.0, 10.0)], 40.0);
        let other = building("way/12", vec![rect(50.0, 0.0, 10.0, 10.0)], 9.0);
        let all = vec![part, outline, other];
        assert_eq!(building_at(&all, LAT + north(10.0), LNG + east(10.0)), Some(1));
        assert_eq!(building_at(&all, LAT + north(5.0), LNG + east(55.0)), Some(2));
        assert_eq!(building_at(&all, LAT + north(40.0), LNG), None);
    }

    #[test]
    fn levels_from_height_and_window_heights() {
        assert_eq!(estimated_levels(21.0), 6);
        assert_eq!(estimated_levels(9.0), 2);
        assert_eq!(estimated_levels(2.0), 1);
        assert_eq!(estimated_levels(1_000.0), MAX_LEVELS);
        assert_eq!(level_height_m(0), 1.5);
        assert_eq!(level_height_m(3), 10.5);
    }

    #[test]
    fn orientation_labels() {
        assert_eq!(orientation_label(0.0), "N");
        assert_eq!(orientation_label(350.0), "N");
        assert_eq!(orientation_label(44.0), "NE");
        assert_eq!(orientation_label(180.0), "S");
        assert_eq!(orientation_label(225.0), "SO");
        assert_eq!(orientation_label(270.0), "O");
        assert_eq!(orientation_label(315.0), "NO");
    }
}
