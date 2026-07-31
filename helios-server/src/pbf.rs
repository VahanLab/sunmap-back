//! Lecture d'un extrait OSM converti par `osmium export` en GeoJSON Text
//! Sequences (une feature JSON par ligne).
//!
//! Pourquoi passer par osmium plutôt que par Overpass : l'ingestion de Paris
//! par tuiles Overpass demandait 192 requêtes réseau, ~45 min, et a échoué 27
//! fois au premier essai. À l'échelle de la France c'est irréaliste — et c'est
//! surtout un abus d'une ressource gratuite partagée. Un extrait Geofabrik se
//! traite en local, de façon déterministe et reproductible.
//!
//! L'assemblage des multipolygones (indispensable : ce sont les anneaux
//! `inner` qui gardent les cours creuses) est fait par osmium, qui sort des
//! `Polygon`/`MultiPolygon` GeoJSON avec leurs trous.
//!
//! Les règles métier (tags → hauteur, replis) ne sont PAS redéfinies ici :
//! elles viennent de [`crate::osm`], pour que les deux sources de données se
//! comportent exactement pareil.

use std::collections::HashMap;
use std::io::BufRead;

use serde::Deserialize;

use crate::osm::{
    building_from, fill_missing_heights, is_wood, place_from, tree_from, wood_from, Building,
    Place, Tree, Wood, AMENITIES,
};

/// Une feature GeoJSON telle que produite par `osmium export -u type_id`.
#[derive(Deserialize)]
struct Feature {
    /// `--add-unique-id=type_id` produit "w123" / "r456" / "n789".
    id: Option<String>,
    geometry: Option<Geometry>,
    #[serde(default)]
    properties: HashMap<String, serde_json::Value>,
}

#[derive(Deserialize)]
#[serde(tag = "type")]
enum Geometry {
    Point { coordinates: [f64; 2] },
    Polygon { coordinates: Vec<Vec<[f64; 2]>> },
    MultiPolygon { coordinates: Vec<Vec<Vec<[f64; 2]>>> },
    #[serde(other)]
    Other,
}

/// Ce qu'on a extrait d'un fichier : les trois couches d'un coup, pour ne
/// traverser l'extrait qu'une fois.
#[derive(Default)]
pub struct Extract {
    pub buildings: Vec<Building>,
    pub woods: Vec<Wood>,
    pub trees: Vec<Tree>,
    pub places: Vec<Place>,
}

/// Trie chaque feature dans sa couche selon ses tags.
///
/// Une même feature peut être bâtiment ET terrasse (un restaurant cartographié
/// comme bâtiment) : les deux couches la reçoivent, chacune l'utilisant pour ce
/// qu'elle vaut — l'emprise d'un côté, le POI de l'autre.
pub fn read_geojsonseq<R: BufRead>(reader: R) -> Result<Extract, String> {
    let mut out = Extract::default();
    let mut ignored = 0usize;

    for (n, line) in reader.lines().enumerate() {
        let line = line.map_err(|e| format!("lecture ligne {n} : {e}"))?;
        let line = line.trim_start_matches('\u{1e}'); // séparateur RS du format
        if line.trim().is_empty() {
            continue;
        }
        let Ok(feature) = serde_json::from_str::<Feature>(line) else {
            ignored += 1;
            continue;
        };
        let Some(geometry) = feature.geometry else {
            ignored += 1;
            continue;
        };

        let tags: HashMap<String, String> = feature
            .properties
            .iter()
            .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
            .collect();
        let osm_id = osm_id(feature.id.as_deref());

        let is_building = tags.contains_key("building") || tags.contains_key("building:part");
        let is_tree = tags.get("natural").map(String::as_str) == Some("tree");
        // Un bois n'est jamais un bâtiment : le `if` plus bas est exclusif, mais
        // rien n'empêche une forêt de porter aussi un `amenity`, d'où un test
        // indépendant.
        let wood = is_wood(&tags);
        // Pas de filtre sur `outdoor_seating` : le tag est très inégalement
        // renseigné, et filtrer dessus faisait disparaître des établissements
        // qui ont bel et bien une terrasse. On ramasse tout, l'utilisateur
        // filtre ensuite.
        let is_terrace = tags
            .get("amenity")
            .is_some_and(|a| AMENITIES.contains(&a.as_str()));
        // Bancs et tables de pique-nique : même table et même pipeline que les
        // établissements — `place_from` normalise leur « amenity ».
        let is_furniture = crate::osm::furniture_kind(&tags).is_some();

        if wood {
            if let Some(w) = wood_from(osm_id.clone(), &tags, rings(&geometry)) {
                out.woods.push(w);
            }
        }
        if is_building {
            if let Some(b) = building_from(osm_id.clone(), &tags, rings(&geometry)) {
                out.buildings.push(b);
            }
        }
        // Arbres et terrasses sont ponctuels : un objet surfacique (un
        // restaurant cartographié en bâtiment) est ramené à son centroïde,
        // comme le faisait `out center` côté Overpass.
        if is_tree || is_terrace || is_furniture {
            if let Some((lat, lng)) = representative_point(&geometry) {
                if is_tree {
                    out.trees.push(tree_from(osm_id.clone(), lat, lng, &tags));
                }
                if is_terrace || is_furniture {
                    out.places.push(place_from(osm_id, lat, lng, &tags));
                }
            }
        }
    }

    let fallback = fill_missing_heights(&mut out.buildings);
    let tagged = out.buildings.iter().filter(|b| b.height_from_osm).count();
    println!(
        "extrait : {} emprises ({tagged} avec hauteur OSM, {} au défaut {fallback:.1} m), \
         {} arbres, {} établissements — {ignored} features ignorées",
        out.buildings.len(),
        out.buildings.len() - tagged,
        out.trees.len(),
        out.places.len(),
    );
    Ok(out)
}

/// Identifiant osmium → identifiant OSM canonique, pour rester recoupable avec
/// osm.org et avec ce qu'écrit l'ingestion Overpass.
///
/// `osmium export -u type_id` préfixe `n`/`w`/`r` pour les objets bruts, mais
/// **`a` pour les aires assemblées** — c'est-à-dire pour tous nos bâtiments. Un
/// identifiant d'aire n'est pas l'identifiant d'origine : libosmium y encode la
/// provenance sur le bit de poids faible, `way_id × 2` ou `relation_id × 2 + 1`.
/// Sans ce décodage, les emprises entrent en base sous des identifiants
/// fantômes ("a497609322"), ne dédoublonnent plus avec l'ingestion Overpass et
/// ne renvoient plus vers osm.org.
fn osm_id(raw: Option<&str>) -> String {
    let Some(raw) = raw else { return "unknown".into() };
    let (prefix, rest) = raw.split_at(1);
    match prefix {
        "n" => format!("node/{rest}"),
        "w" => format!("way/{rest}"),
        "r" => format!("relation/{rest}"),
        "a" => match rest.parse::<u64>() {
            Ok(id) if id % 2 == 0 => format!("way/{}", id / 2),
            Ok(id) => format!("relation/{}", (id - 1) / 2),
            Err(_) => raw.to_string(),
        },
        _ => raw.to_string(),
    }
}

/// Anneaux `(lat, lon)` — GeoJSON est en `[lon, lat]`.
///
/// Extérieurs et trous sont mis à plat dans la même liste : la rasterisation
/// pair-impair les traite indifféremment, et c'est ce que stocke déjà
/// `buildings.geom`.
fn rings(geometry: &Geometry) -> Vec<Vec<(f64, f64)>> {
    let to_ring = |r: &Vec<[f64; 2]>| -> Vec<(f64, f64)> {
        r.iter().map(|c| (c[1], c[0])).collect()
    };
    match geometry {
        Geometry::Polygon { coordinates } => coordinates.iter().map(to_ring).collect(),
        Geometry::MultiPolygon { coordinates } => {
            coordinates.iter().flatten().map(to_ring).collect()
        }
        _ => Vec::new(),
    }
}

/// Point représentatif : la position pour un nœud, le centroïde de la boîte
/// englobante pour une surface.
fn representative_point(geometry: &Geometry) -> Option<(f64, f64)> {
    match geometry {
        Geometry::Point { coordinates } => Some((coordinates[1], coordinates[0])),
        _ => {
            let all = rings(geometry);
            let pts: Vec<(f64, f64)> = all.into_iter().flatten().collect();
            if pts.is_empty() {
                return None;
            }
            let (mut min_lat, mut max_lat) = (f64::MAX, f64::MIN);
            let (mut min_lng, mut max_lng) = (f64::MAX, f64::MIN);
            for (lat, lng) in pts {
                min_lat = min_lat.min(lat);
                max_lat = max_lat.max(lat);
                min_lng = min_lng.min(lng);
                max_lng = max_lng.max(lng);
            }
            Some(((min_lat + max_lat) / 2.0, (min_lng + max_lng) / 2.0))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_are_comparable_with_overpass_ingest() {
        assert_eq!(osm_id(Some("w248804661")), "way/248804661");
        assert_eq!(osm_id(Some("r2779974")), "relation/2779974");
        assert_eq!(osm_id(Some("n42")), "node/42");
    }

    /// Les aires assemblées — donc tous nos bâtiments — portent un id `a` qui
    /// encode la provenance sur le bit de poids faible. Vérifié sur deux objets
    /// réels déjà ingérés via Overpass.
    #[test]
    fn area_ids_decode_to_source_object() {
        assert_eq!(osm_id(Some("a497609322")), "way/248804661");
        assert_eq!(osm_id(Some("a5559949")), "relation/2779974");
    }

    /// Un bâtiment à cour doit ressortir avec ses deux anneaux, sinon la cour
    /// se retrouve bétonnée à la rasterisation.
    #[test]
    fn polygon_hole_survives() {
        let line = r#"{"type":"Feature","id":"w1","properties":{"building":"yes","building:levels":"5"},"geometry":{"type":"Polygon","coordinates":[[[2.0,48.0],[2.1,48.0],[2.1,48.1],[2.0,48.1],[2.0,48.0]],[[2.04,48.04],[2.06,48.04],[2.06,48.06],[2.04,48.06],[2.04,48.04]]]}}"#;
        let extract = read_geojsonseq(line.as_bytes()).unwrap();
        assert_eq!(extract.buildings.len(), 1);
        let b = &extract.buildings[0];
        assert_eq!(b.osm_id, "way/1");
        assert_eq!(b.rings.len(), 2, "extérieur + trou attendus");
        // 5 niveaux × 3 m + 3 m de toiture.
        assert!(b.height_from_osm);
        assert!((b.height_m - 18.0).abs() < 1e-6);
        // GeoJSON est en [lon, lat], nos anneaux en (lat, lon).
        assert!((b.rings[0][0].0 - 48.0).abs() < 1e-9);
        assert!((b.rings[0][0].1 - 2.0).abs() < 1e-9);
    }

    /// Un restaurant cartographié en bâtiment alimente les deux couches.
    #[test]
    fn foret_devient_une_emprise_boisee() {
        let line = r#"{"type":"Feature","id":"w9","properties":{"landuse":"forest","name":"Bois test"},"geometry":{"type":"Polygon","coordinates":[[[2.0,48.0],[2.1,48.0],[2.1,48.1],[2.0,48.1],[2.0,48.0]]]}}"#;
        let extract = read_geojsonseq(std::io::Cursor::new(line)).unwrap();
        assert_eq!(extract.woods.len(), 1);
        assert_eq!(extract.buildings.len(), 0, "un bois n'est pas un caster opaque");
        let w = &extract.woods[0];
        assert_eq!(w.osm_id, "way/9");
        assert_eq!(w.height_m, 18.0, "hauteur de futaie par défaut");
        assert!(!w.height_from_osm, "aucune hauteur taguée");
    }

    /// Une clairière est un anneau intérieur : la perdre bétonnerait le trou,
    /// exactement comme pour la cour d'un immeuble.
    #[test]
    fn clairiere_conservee_comme_anneau_interieur() {
        let line = r#"{"type":"Feature","id":"r4","properties":{"natural":"wood"},"geometry":{"type":"Polygon","coordinates":[[[2.0,48.0],[2.1,48.0],[2.1,48.1],[2.0,48.1],[2.0,48.0]],[[2.04,48.04],[2.06,48.04],[2.06,48.06],[2.04,48.06],[2.04,48.04]]]}}"#;
        let extract = read_geojsonseq(std::io::Cursor::new(line)).unwrap();
        assert_eq!(extract.woods.len(), 1);
        assert_eq!(extract.woods[0].rings.len(), 2);
    }

    /// Les hauteurs par défaut diffèrent : des broussailles ne portent pas
    /// l'ombre d'une futaie.
    #[test]
    fn hauteurs_par_defaut_selon_le_type() {
        for (tags, expected) in [
            (r#""natural":"scrub""#, 3.0),
            (r#""natural":"tree_row""#, 12.0),
            (r#""natural":"wood""#, 18.0),
        ] {
            let line = format!(
                r#"{{"type":"Feature","id":"w1","properties":{{{tags}}},"geometry":{{"type":"Polygon","coordinates":[[[2.0,48.0],[2.1,48.0],[2.1,48.1],[2.0,48.0]]]}}}}"#
            );
            let extract = read_geojsonseq(std::io::Cursor::new(line)).unwrap();
            assert_eq!(extract.woods[0].height_m, expected, "pour {tags}");
        }
    }

    /// Une hauteur taguée prime sur le repli, et se signale comme telle.
    #[test]
    fn hauteur_taguee_prime() {
        let line = r#"{"type":"Feature","id":"w5","properties":{"natural":"wood","height":"25"},"geometry":{"type":"Polygon","coordinates":[[[2.0,48.0],[2.1,48.0],[2.1,48.1],[2.0,48.0]]]}}"#;
        let extract = read_geojsonseq(std::io::Cursor::new(line)).unwrap();
        assert_eq!(extract.woods[0].height_m, 25.0);
        assert!(extract.woods[0].height_from_osm);
    }

    #[test]
    fn building_that_is_also_a_terrace_feeds_both_layers() {
        let line = r#"{"type":"Feature","id":"w2","properties":{"building":"yes","amenity":"restaurant","outdoor_seating":"yes","name":"Chez X"},"geometry":{"type":"Polygon","coordinates":[[[2.0,48.0],[2.2,48.0],[2.2,48.2],[2.0,48.2],[2.0,48.0]]]}}"#;
        let extract = read_geojsonseq(line.as_bytes()).unwrap();
        assert_eq!(extract.buildings.len(), 1);
        assert_eq!(extract.places.len(), 1);
        let poi = &extract.places[0];
        assert_eq!(poi.name.as_deref(), Some("Chez X"));
        // Ramené au centre de l'emprise, comme `out center` côté Overpass.
        assert!((poi.lat - 48.1).abs() < 1e-9);
        assert!((poi.lng - 2.1).abs() < 1e-9);
    }

    #[test]
    fn tree_uses_shared_defaults() {
        let line = r#"{"type":"Feature","id":"n3","properties":{"natural":"tree"},"geometry":{"type":"Point","coordinates":[2.35,48.86]}}"#;
        let extract = read_geojsonseq(line.as_bytes()).unwrap();
        assert_eq!(extract.trees.len(), 1);
        let t = &extract.trees[0];
        assert!((t.height_m - 10.0).abs() < 1e-9);
        assert!((t.crown_radius_m - 3.0).abs() < 1e-9);
    }
}
