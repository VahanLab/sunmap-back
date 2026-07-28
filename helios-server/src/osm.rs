//! Extraction OSM via Overpass.
//!
//! Ce module n'est plus sur le chemin d'une requête client : il n'est utilisé
//! que par le binaire `ingest`, qui remplit PostGIS une bonne fois. L'API
//! Overpass publique met 5 à 20 s par bbox dense, répond 504 aux heures de
//! pointe et demande de la politesse — incompatible avec un appel par
//! déplacement de carte.

use std::collections::HashMap;

use serde::Deserialize;

/// Hauteur de repli ultime, si *aucun* bâtiment de la zone n'a de tag de
/// hauteur. Sinon on utilise la médiane locale (cf. [`parse_buildings`]).
pub const DEFAULT_BUILDING_HEIGHT_M: f32 = 9.0;

/// Miroirs Overpass, essayés dans l'ordre. L'instance officielle
/// (overpass-api.de) sature souvent (504) aux heures de pointe.
const MIRRORS: &[&str] = &[
    "https://overpass-api.de/api/interpreter",
    "https://overpass.kumi.systems/api/interpreter",
    "https://overpass.private.coffee/api/interpreter",
];

/// Emprise d'un bâtiment OSM + hauteur retenue.
#[derive(Clone, Debug)]
pub struct Building {
    /// Identifiant OSM complet ("way/123", "relation/456") — sert à désigner
    /// le coupable d'une ombre côté client, donc doit être recoupable avec
    /// osm.org.
    pub osm_id: String,
    pub name: Option<String>,
    /// Anneaux du polygone (lat, lon). Un seul pour un way ; pour une relation
    /// multipolygone, l'extérieur ET les intérieurs — la rasterisation en
    /// règle pair-impair creuse alors naturellement les cours.
    pub rings: Vec<Vec<(f64, f64)>>,
    pub height_m: f32,
    /// `false` = hauteur devinée, pas taggée dans OSM.
    pub height_from_osm: bool,
}

#[derive(Clone, Debug)]
pub struct Tree {
    pub osm_id: String,
    pub lat: f64,
    pub lng: f64,
    pub height_m: f64,
    pub crown_radius_m: f64,
}

/// Catégories d'établissements retenues.
///
/// Volontairement large : le tag `outdoor_seating` est très inégalement
/// renseigné dans OSM, donc filtrer dessus à l'ingestion faisait disparaître
/// beaucoup d'établissements qui ont bel et bien une terrasse. On ramasse tout
/// et on laisse le filtre à l'utilisateur, qui voit sur la carte.
pub const AMENITIES: &[&str] = &[
    "bar",
    "pub",
    "restaurant",
    "cafe",
    "fast_food",
    "biergarten",
];

/// Établissement brut (centroïde pour les ways/relations).
#[derive(Clone, Debug)]
pub struct Place {
    pub osm_id: String,
    pub name: Option<String>,
    pub amenity: Option<String>,
    /// Tag `outdoor_seating` d'OSM, en trois états. `None` = non renseigné,
    /// ce qui est le cas de ~79 % des établissements parisiens et ne veut
    /// surtout pas dire « pas de terrasse » — d'où le `Option` plutôt qu'un
    /// booléen, qui confondait l'absence d'information avec une absence de
    /// terrasse.
    pub outdoor_seating: Option<bool>,
    pub lat: f64,
    pub lng: f64,
    pub website: Option<String>,
    pub phone: Option<String>,
    pub opening_hours: Option<String>,
    pub cuisine: Option<String>,
    pub wikidata: Option<String>,
}

/// Requête Overpass générique : essaie chaque miroir dans l'ordre, renvoie le
/// premier succès.
pub async fn query<T: serde::de::DeserializeOwned>(
    http: &reqwest::Client,
    body: &str,
) -> Result<T, String> {
    let mut last_err = String::new();
    for mirror in MIRRORS {
        match http
            .post(*mirror)
            .form(&[("data", body)])
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
    Err(format!("Overpass : {last_err}"))
}

// ---------------------------------------------------------------- bâtiments

/// Trois familles à ramasser, sinon des casters bien visibles à l'écran
/// manquent purement et simplement de la DSM :
///  - `way[building]`      : le cas courant ;
///  - `way[building:part]` : Simple 3D Buildings — tours, corps surélevés.
///    Mapbox les rend, et la rasterisation garde le plus haut ;
///  - `rel[building]`      : multipolygones, c'est-à-dire précisément les
///    grands bâtiments à cour intérieure, très fréquents à Paris. Ils ne sont
///    PAS des ways et échappaient donc entièrement à la requête.
///
/// Pas de plafond sur `out geom` : une bbox de 2,4 km de côté au cœur de Paris
/// contient ~10 800 emprises, un `out geom 5000` en perdait la moitié.
pub async fn fetch_buildings(
    http: &reqwest::Client,
    s: f64,
    w: f64,
    n: f64,
    e: f64,
) -> Result<Vec<Building>, String> {
    let body = format!(
        r#"[out:json][timeout:180];
(
  way["building"]({s},{w},{n},{e});
  way["building:part"]({s},{w},{n},{e});
  relation["building"]({s},{w},{n},{e});
);
out geom;"#
    );
    let raw: BuildingsResponse = query(http, &body).await?;
    Ok(parse_buildings(raw))
}

/// Hauteur d'un bâtiment d'après ses tags. `None` = non taggé, à combler par
/// [`fill_missing_heights`].
///
/// **Règle métier partagée** : ce sont les mêmes tags quelle que soit la
/// provenance de la donnée (Overpass ou extrait PBF), donc la règle vit ici et
/// nulle part ailleurs. Elle a coûté assez de mises au point pour ne pas être
/// réécrite en double.
pub fn height_from_tags(tags: &HashMap<String, String>) -> Option<f32> {
    tags.get("height")
        .and_then(|h| parse_meters(Some(h)).map(|v| v as f32))
        .or_else(|| {
            tags.get("building:levels")
                .and_then(|l| l.trim().parse::<f32>().ok())
                // 3 m par niveau + ~3 m de comble/toiture, sinon les immeubles
                // haussmanniens sortent trop bas.
                .map(|levels| levels * 3.0 + 3.0)
        })
        .filter(|h| h.is_finite() && *h > 0.0)
}

/// Comble les hauteurs manquantes par la médiane locale du lot.
///
/// Défaut local plutôt que global : ~30 % des bâtiments parisiens n'ont aucun
/// tag de hauteur, et 9 m (3 étages) sous-estime largement un tissu
/// haussmannien à 20 m — leurs ombres portées disparaissaient. La médiane des
/// hauteurs connues du lot s'adapte au quartier.
///
/// Les bâtiments sans hauteur doivent arriver avec `height_m` à `NaN` et
/// `height_from_osm` à `false`.
pub fn fill_missing_heights(buildings: &mut [Building]) -> f32 {
    let mut known: Vec<f32> = buildings
        .iter()
        .filter(|b| b.height_from_osm)
        .map(|b| b.height_m)
        .collect();
    known.sort_by(f32::total_cmp);
    let fallback = if known.is_empty() {
        DEFAULT_BUILDING_HEIGHT_M
    } else {
        known[known.len() / 2].clamp(6.0, 40.0)
    };
    for b in buildings.iter_mut().filter(|b| !b.height_from_osm) {
        b.height_m = fallback;
    }
    fallback
}

/// Assemble un bâtiment à partir de ses tags et de ses anneaux déjà résolus.
/// `height_m` reste `NaN` si rien n'est taggé — cf. [`fill_missing_heights`].
pub fn building_from(
    osm_id: String,
    tags: &HashMap<String, String>,
    rings: Vec<Vec<(f64, f64)>>,
) -> Option<Building> {
    let rings: Vec<Vec<(f64, f64)>> = rings.into_iter().filter(|r| r.len() >= 3).collect();
    if rings.is_empty() {
        return None;
    }
    let tagged = height_from_tags(tags);
    Some(Building {
        osm_id,
        name: tags.get("name").cloned(),
        rings,
        height_m: tagged.unwrap_or(f32::NAN),
        height_from_osm: tagged.is_some(),
    })
}

/// Emprise boisée.
///
/// Même forme qu'un bâtiment — un contour, une hauteur — donc même type, pour
/// ne pas dupliquer la rasterisation en scanline ni le parsing WKT. C'est la
/// table qui les sépare, et le tampon dans la DSM qui les traite différemment.
pub type Wood = Building;

/// Valeurs de `natural` et `landuse` qui désignent une emprise boisée.
pub const WOOD_TAGS: &[(&str, &str)] = &[
    ("natural", "wood"),
    ("landuse", "forest"),
    ("natural", "tree_row"),
    ("natural", "scrub"),
];

/// Hauteur de canopée par défaut, en mètres, selon le type d'emprise.
///
/// OSM ne tague quasiment jamais la hauteur d'un bois. Ces valeurs sont des
/// ordres de grandeur pour la végétation d'Europe tempérée, destinées à être
/// remplacées par un modèle de hauteur de canopée. Volontairement prudentes :
/// surestimer allongerait des ombres qui n'existent pas.
fn default_canopy_height(tags: &HashMap<String, String>) -> f64 {
    match (
        tags.get("natural").map(String::as_str),
        tags.get("landuse").map(String::as_str),
    ) {
        // Broussailles : au-dessus d'une personne assise, sous un arbre.
        (Some("scrub"), _) => 3.0,
        // Alignement d'arbres de rue, généralement étêté.
        (Some("tree_row"), _) => 12.0,
        // Futaie tempérée arrivée à maturité.
        _ => 18.0,
    }
}

/// Emprise boisée à partir de ses tags et de ses anneaux.
///
/// Renvoie `None` si aucun anneau exploitable — même garde-fou que pour les
/// bâtiments.
pub fn wood_from(
    osm_id: String,
    tags: &HashMap<String, String>,
    rings: Vec<Vec<(f64, f64)>>,
) -> Option<Wood> {
    let rings: Vec<Vec<(f64, f64)>> = rings.into_iter().filter(|r| r.len() >= 3).collect();
    if rings.is_empty() {
        return None;
    }
    let tagged = parse_meters(tags.get("height"));
    Some(Wood {
        osm_id,
        name: tags.get("name").cloned(),
        rings,
        height_m: tagged.unwrap_or_else(|| default_canopy_height(tags)) as f32,
        height_from_osm: tagged.is_some(),
    })
}

/// Est-ce une emprise boisée ?
pub fn is_wood(tags: &HashMap<String, String>) -> bool {
    WOOD_TAGS
        .iter()
        .any(|(k, v)| tags.get(*k).map(String::as_str) == Some(*v))
}

/// Arbre à partir de ses tags. Mêmes replis quelle que soit la source.
pub fn tree_from(osm_id: String, lat: f64, lng: f64, tags: &HashMap<String, String>) -> Tree {
    let height_m = parse_meters(tags.get("height")).unwrap_or(10.0).min(40.0);
    let crown_radius_m = parse_meters(tags.get("diameter_crown"))
        .map(|d| d / 2.0)
        .unwrap_or_else(|| (height_m * 0.3).clamp(2.0, 6.0));
    Tree {
        osm_id,
        lat,
        lng,
        height_m,
        crown_radius_m,
    }
}

/// Établissement à partir de ses tags.
pub fn place_from(osm_id: String, lat: f64, lng: f64, tags: &HashMap<String, String>) -> Place {
    Place {
        osm_id,
        name: tags.get("name").cloned(),
        amenity: tags.get("amenity").cloned(),
        // "no" est le seul refus explicite ; "seasonal", "sidewalk",
        // "garden"… décrivent une terrasse et valent donc oui.
        outdoor_seating: tags
            .get("outdoor_seating")
            .map(|v| v.as_str() != "no"),
        lat,
        lng,
        // Website/téléphone : "contact:*" en repli si le tag simple manque.
        website: tags
            .get("website")
            .or_else(|| tags.get("contact:website"))
            .cloned(),
        phone: tags
            .get("phone")
            .or_else(|| tags.get("contact:phone"))
            .cloned(),
        opening_hours: tags.get("opening_hours").cloned(),
        cuisine: tags.get("cuisine").cloned(),
        wikidata: tags.get("wikidata").cloned(),
    }
}

fn parse_buildings(raw: BuildingsResponse) -> Vec<Building> {
    let mut buildings: Vec<Building> = raw
        .elements
        .into_iter()
        .filter_map(|el| {
            let tags = el.tags.unwrap_or_default();

            // Un way donne un anneau. Une relation en donne plusieurs, et on
            // garde AUSSI les "inner" : la rasterisation pair-impair s'en sert
            // pour creuser les cours. Les ignorer bétonnerait la cour, et tout
            // point à l'intérieur serait à l'ombre en permanence (observé sur
            // relation/2779974, un immeuble à cour du 3e).
            let rings: Vec<Vec<(f64, f64)>> = match (el.geometry, el.members) {
                (Some(g), _) => vec![g],
                (None, Some(members)) => members
                    .into_iter()
                    .filter(|m| matches!(m.role.as_deref(), Some("outer") | Some("inner")))
                    .filter_map(|m| m.geometry)
                    .collect(),
                _ => Vec::new(),
            }
            .into_iter()
            .map(|r| r.iter().map(|node| (node.lat, node.lon)).collect())
            .collect();

            building_from(format!("{}/{}", el.element_type, el.id), &tags, rings)
        })
        .collect();

    fill_missing_heights(&mut buildings);
    buildings
}

// ------------------------------------------------------------------ arbres

pub async fn fetch_trees(
    http: &reqwest::Client,
    s: f64,
    w: f64,
    n: f64,
    e: f64,
) -> Result<Vec<Tree>, String> {
    let body = format!(
        r#"[out:json][timeout:180];
node["natural"="tree"]({s},{w},{n},{e});
out;"#
    );
    let raw: ElementsResponse = query(http, &body).await?;

    Ok(raw
        .elements
        .into_iter()
        .filter_map(|el| {
            let (lat, lng) = (el.lat?, el.lon?);
            let tags = el.tags.unwrap_or_default();
            Some(tree_from(
                format!("{}/{}", el.element_type, el.id),
                lat,
                lng,
                &tags,
            ))
        })
        .collect())
}

// ---------------------------------------------------------- établissements

pub async fn fetch_places(
    http: &reqwest::Client,
    s: f64,
    w: f64,
    n: f64,
    e: f64,
) -> Result<Vec<Place>, String> {
    let body = format!(
        r#"[out:json][timeout:180];
nwr["amenity"~"^({amenities})$"]({s},{w},{n},{e});
out center;"#,
        amenities = AMENITIES.join("|")
    );
    let raw: ElementsResponse = query(http, &body).await?;

    Ok(raw
        .elements
        .into_iter()
        .filter_map(|el| {
            let (lat, lng) = match (el.lat, el.lon, &el.center) {
                (Some(la), Some(lo), _) => (la, lo),
                (_, _, Some(c)) => (c.lat, c.lon),
                _ => return None,
            };
            let tags = el.tags.unwrap_or_default();
            Some(place_from(
                format!("{}/{}", el.element_type, el.id),
                lat,
                lng,
                &tags,
            ))
        })
        .collect())
}

/// "12", "12 m", "12,5" → mètres (tags OSM laxistes).
pub fn parse_meters(raw: Option<&String>) -> Option<f64> {
    let raw = raw?;
    raw.replace(',', ".")
        .replace('m', "")
        .trim()
        .parse::<f64>()
        .ok()
}

// ------------------------------------------------------------------- DTOs

#[derive(Deserialize)]
struct BuildingsResponse {
    elements: Vec<BuildingElement>,
}

#[derive(Deserialize)]
struct BuildingElement {
    id: u64,
    /// "way" ou "relation" — sert à composer un id OSM recoupable.
    #[serde(rename = "type")]
    element_type: String,
    tags: Option<HashMap<String, String>>,
    /// Présent sur les ways.
    geometry: Option<Vec<GeomNode>>,
    /// Présent sur les relations (multipolygones).
    members: Option<Vec<RelationMember>>,
}

#[derive(Deserialize)]
struct RelationMember {
    /// "outer" / "inner" pour un multipolygone.
    role: Option<String>,
    geometry: Option<Vec<GeomNode>>,
}

#[derive(Deserialize)]
struct GeomNode {
    lat: f64,
    lon: f64,
}

#[derive(Deserialize)]
struct ElementsResponse {
    elements: Vec<Element>,
}

#[derive(Deserialize)]
struct Element {
    #[serde(rename = "type")]
    element_type: String,
    id: u64,
    lat: Option<f64>,
    lon: Option<f64>,
    /// Centroïde renvoyé par `out center` pour les ways/relations.
    center: Option<Center>,
    tags: Option<HashMap<String, String>>,
}

#[derive(Deserialize)]
struct Center {
    lat: f64,
    lon: f64,
}
