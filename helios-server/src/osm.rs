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
/// hauteur. Sinon on utilise la médiane locale (cf. [`LocalHeights`]).
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
    /// Renseigné pour une emprise boisée (`Wood` est un alias de ce type),
    /// `None` pour un vrai bâtiment — un immeuble n'a pas de feuillage.
    pub leaf_type: Option<LeafType>,
}

#[derive(Clone, Debug)]
pub struct Tree {
    pub osm_id: String,
    pub lat: f64,
    pub lng: f64,
    pub height_m: f64,
    pub crown_radius_m: f64,
    pub leaf_type: LeafType,
}

/// Silhouette de l'arbre, pour choisir le modèle 3D à l'affichage.
///
/// Trois classes et pas davantage : c'est ce que `leaf_type` distingue dans
/// OSM, et c'est déjà ce qui décide de la silhouette vue de loin. Le genre
/// (`genus`) est bien plus fin mais très inégalement rempli — il ne sert ici
/// qu'à rattraper un `leaf_type` absent.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LeafType {
    /// Feuillu, et repli par défaut : ~80 % des arbres urbains d'Europe.
    Broadleaved,
    Needleleaved,
    Palm,
}

/// Genres à aiguilles les plus fréquents en Europe — l'ordre n'a pas
/// d'importance, la comparaison est exacte et en minuscules.
const NEEDLE_GENERA: &[&str] = &[
    "pinus", "picea", "abies", "cedrus", "larix", "pseudotsuga", "taxus", "thuja",
    "cupressus", "chamaecyparis", "juniperus", "sequoia", "sequoiadendron", "tsuga",
    "cryptomeria", "metasequoia", "calocedrus",
];

/// Genres à silhouette de palmier : un stipe nu couronné de frondes.
///
/// Deux familles s'y mêlent volontairement, parce que c'est la **silhouette**
/// qui choisit le modèle et non la botanique (cf. `LeafType`) :
///
/// 1. les vrais palmiers (Arecaceae) ;
/// 2. leurs sosies d'autres familles — cycas, dragonnier, yucca, cordyline,
///    bananier. Aucun n'est un palmier, tous portent un bouquet de longues
///    feuilles au sommet d'un tronc nu, et le modèle de feuillu — une boule
///    posée sur un fût — leur va bien plus mal que celui de palmier.
///
/// La liste vient du **relevé taginfo des valeurs de `genus` réellement
/// présentes dans OSM** (août 2026), pas d'une flore : la famille compte ~180
/// genres, la traîne ne se couvrirait jamais, et un genre absent d'OSM ne
/// coûte que du bruit ici. Les quatre derniers vrais palmiers sont sous le
/// seuil du relevé mais courants en plantation d'ornement — ils ne coûtent
/// rien à prévoir.
///
/// L'ordre n'a pas d'importance, la comparaison est exacte et en minuscules.
const PALM_GENERA: &[&str] = &[
    // Arecaceae, par fréquence OSM décroissante (elaeis ≈ 17 000 objets,
    // howea ≈ 25).
    "elaeis", "phoenix", "cocos", "jubaea", "washingtonia", "trachycarpus",
    "livistona", "sabal", "roystonea", "areca", "syagrus", "dypsis",
    "chamaerops", "archontophoenix", "butia", "brahea", "adonidia", "thrinax",
    "howea",
    // Attendus mais sous le seuil du relevé.
    "bismarckia", "wodyetia", "caryota", "ravenea",
    // Sosies : silhouette de palmier, autre famille.
    "musa", "cordyline", "cycas", "dracaena", "yucca", "zamia",
];

impl LeafType {
    /// Depuis les tags OSM : `leaf_type` d'abord (le tag prévu pour ça), puis
    /// repli sur le genre — les imports municipaux français (Paris, Lyon,
    /// Bordeaux) renseignent souvent `genus`/`species` sans `leaf_type`.
    pub fn from_tags(tags: &HashMap<String, String>) -> Self {
        match tags.get("leaf_type").map(String::as_str) {
            Some("needleleaved") => return Self::Needleleaved,
            Some("palm") => return Self::Palm,
            Some("broadleaved") => return Self::Broadleaved,
            _ => {}
        }
        // `species` et `taxon` commencent par le genre ("Pinus nigra") : le
        // premier mot suffit dans les trois cas. `taxon` est le tag que le wiki
        // OSM recommande pour l'identification — trois fois moins répandu que
        // `species`, mais gratuit à lire ici.
        let genus = tags
            .get("genus")
            .or_else(|| tags.get("species"))
            .or_else(|| tags.get("taxon"))
            .map(|g| g.split_whitespace().next().unwrap_or("").to_lowercase());
        match genus.as_deref() {
            Some(g) if NEEDLE_GENERA.contains(&g) => Self::Needleleaved,
            Some(g) if PALM_GENERA.contains(&g) => Self::Palm,
            _ => Self::Broadleaved,
        }
    }

    /// Valeur stockée en base et relue telle quelle.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Broadleaved => "broadleaved",
            Self::Needleleaved => "needleleaved",
            Self::Palm => "palm",
        }
    }

    pub fn parse(raw: Option<&str>) -> Self {
        match raw {
            Some("needleleaved") => Self::Needleleaved,
            Some("palm") => Self::Palm,
            _ => Self::Broadleaved,
        }
    }
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

/// Mobilier urbain assis : la question « au soleil à quelle heure ? » s'y pose
/// exactement comme pour une terrasse, avec un avantage — la coordonnée OSM
/// est déjà la bonne, aucun recalage hors bâtiment à faire.
///
/// Le banc arrive par `amenity=bench`, la table de pique-nique par
/// `leisure=picnic_table`, l'aire de jeux par `leisure=playground`, le
/// street workout par `leisure=fitness_station` : cette fonction les ramène
/// à une « amenity » unique, pour qu'ils traversent le pipeline des
/// établissements sans le dupliquer. Aires de jeux et street workout sont le
/// plus souvent des polygones — ramenés à leur centroïde comme tout objet
/// surfacique.
pub fn furniture_kind(tags: &HashMap<String, String>) -> Option<&'static str> {
    if tags.get("amenity").map(String::as_str) == Some("bench") {
        return Some("bench");
    }
    match tags.get("leisure").map(String::as_str) {
        Some("picnic_table") => Some("picnic_table"),
        Some("playground") => Some("playground"),
        Some("fitness_station") => Some("fitness_station"),
        _ => None,
    }
}

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
    // --- Mobilier urbain (bancs, tables de pique-nique) ; None ailleurs ---
    /// Tag `direction` : où porte le regard une fois assis, en degrés depuis
    /// le nord. Croisé avec l'azimut solaire, il dit « soleil de face » ou
    /// « dans le dos » — rare mais précieux.
    pub direction_deg: Option<f64>,
    /// Sous abri : jamais « au soleil », quelle que soit la classification.
    pub covered: Option<bool>,
    pub backrest: Option<bool>,
    pub seats: Option<i32>,
    pub material: Option<String>,
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

/// Comble les hauteurs manquantes par la médiane locale du lot (cf.
/// [`LocalHeights`]). Renvoie le repli de l'extrait, pour le journal.
///
/// Les bâtiments sans hauteur doivent arriver avec `height_m` à `NaN` et
/// `height_from_osm` à `false`.
pub fn fill_missing_heights(buildings: &mut [Building]) -> f32 {
    let mut heights = LocalHeights::default();
    for b in buildings.iter() {
        heights.add(b);
    }
    heights.finish();
    for b in buildings.iter_mut().filter(|b| !b.height_from_osm) {
        b.height_m = heights.height_for(b);
    }
    heights.fallback()
}

/// Côté d'une cellule de médiane locale, en mètres.
pub const LOCAL_HEIGHT_CELL_M: f64 = 250.0;
/// Hauteurs taggées qu'il faut au moins pour qu'une médiane locale compte ;
/// en dessous, on élargit aux cellules voisines (3×3, puis 5×5), puis on
/// retombe sur la médiane de l'extrait.
pub const LOCAL_HEIGHT_MIN_SAMPLES: usize = 15;
/// Bornes d'une hauteur devinée : une médiane ne doit ni écraser un bourg
/// sous 6 m ni bâtir des tours à partir d'un quartier d'affaires.
const GUESSED_HEIGHT_RANGE_M: (f32, f32) = (6.0, 40.0);

/// Hauteur de repli des bâtiments non taggés : **médiane locale** des
/// hauteurs taggées autour d'eux.
///
/// Une seule médiane par extrait ne marche pas : sur la France entière elle
/// vaut 9 m (2 étages) — le tissu pavillonnaire, majoritaire en nombre —, et
/// les ~60 % d'immeubles parisiens sans tag de hauteur sortaient à 2 étages
/// au lieu de 5 ou 6, avec des ombres portées divisées par deux. Mesuré sur
/// Paris (validation sur les bâtiments taggés, chacun prédit sans lui) :
/// erreur moyenne 10,5 m → 5,8 m, étages justes à ±1 de 29 % → 56 % ; en
/// zone pavillonnaire, rien ne bouge (2,3 → 2,2 m).
///
/// Cellules carrées de [`LOCAL_HEIGHT_CELL_M`] (500 m faisait un peu moins
/// bien), élargies tant qu'elles ont moins de [`LOCAL_HEIGHT_MIN_SAMPLES`]
/// hauteurs, repli sur la médiane de l'extrait au-delà du 5×5. Le bâtiment
/// est rattaché à sa cellule par la moyenne des sommets de son anneau
/// extérieur.
///
/// Mémoire : un `f32` par bâtiment taggé — c'est ce qui permet à `tilegen`
/// de la remplir en passe 1 et de la consulter en passe 2 sans retenir les
/// géométries.
#[derive(Default)]
pub struct LocalHeights {
    cells: HashMap<(i32, i32), Vec<f32>>,
    /// Médiane de l'extrait, posée par [`Self::finish`].
    fallback: Option<f32>,
    /// Médiane retenue par cellule interrogée (`None` = repli de l'extrait).
    cache: HashMap<(i32, i32), Option<f32>>,
    /// Bâtiments comblés par une médiane locale / par le repli de l'extrait.
    pub local_hits: usize,
    pub fallback_hits: usize,
}

impl LocalHeights {
    /// Retient la hauteur d'un bâtiment taggé ; ignore les autres.
    pub fn add(&mut self, b: &Building) {
        if !b.height_from_osm {
            return;
        }
        if let Some((lat, lng)) = anchor(b) {
            self.cells.entry(cell_of(lat, lng)).or_default().push(b.height_m);
        }
    }

    /// Fige la médiane de l'extrait. À appeler une fois tout ajouté.
    pub fn finish(&mut self) {
        let mut all: Vec<f32> = self.cells.values().flatten().copied().collect();
        self.fallback = median(&mut all);
    }

    /// Médiane de l'extrait, ou [`DEFAULT_BUILDING_HEIGHT_M`] si rien n'y est
    /// taggé.
    pub fn fallback(&self) -> f32 {
        self.fallback.unwrap_or(DEFAULT_BUILDING_HEIGHT_M)
    }

    /// Nombre de bâtiments taggés retenus.
    pub fn tagged(&self) -> usize {
        self.cells.values().map(Vec::len).sum()
    }

    /// Hauteur à donner à un bâtiment non taggé.
    pub fn height_for(&mut self, b: &Building) -> f32 {
        let local = anchor(b).and_then(|(lat, lng)| {
            let key = cell_of(lat, lng);
            if let Some(cached) = self.cache.get(&key) {
                return *cached;
            }
            let found = self.local_median(key);
            self.cache.insert(key, found);
            found
        });
        match local {
            Some(h) => {
                self.local_hits += 1;
                h
            }
            None => {
                self.fallback_hits += 1;
                self.fallback()
            }
        }
    }

    fn local_median(&self, (cx, cy): (i32, i32)) -> Option<f32> {
        for r in 0..=2 {
            let mut v: Vec<f32> = Vec::new();
            for dx in -r..=r {
                for dy in -r..=r {
                    if let Some(list) = self.cells.get(&(cx + dx, cy + dy)) {
                        v.extend_from_slice(list);
                    }
                }
            }
            if v.len() >= LOCAL_HEIGHT_MIN_SAMPLES {
                return median(&mut v);
            }
        }
        None
    }
}

/// Médiane bornée à [`GUESSED_HEIGHT_RANGE_M`] ; `None` sur un lot vide.
fn median(v: &mut [f32]) -> Option<f32> {
    if v.is_empty() {
        return None;
    }
    v.sort_by(f32::total_cmp);
    let (lo, hi) = GUESSED_HEIGHT_RANGE_M;
    Some(v[v.len() / 2].clamp(lo, hi))
}

/// Point de rattachement d'un bâtiment : moyenne des sommets de l'anneau
/// extérieur — assez juste pour choisir une cellule de 250 m.
fn anchor(b: &Building) -> Option<(f64, f64)> {
    let ring = b.rings.first().filter(|r| !r.is_empty())?;
    let (la, lo) = ring.iter().fold((0.0, 0.0), |a, p| (a.0 + p.0, a.1 + p.1));
    let n = ring.len() as f64;
    Some((la / n, lo / n))
}

/// Cellule de ~[`LOCAL_HEIGHT_CELL_M`] de côté. Les rangées suivent la
/// latitude ; la largeur des colonnes est corrigée du cosinus de la rangée,
/// pour des cellules à peu près carrées de l'Andalousie à l'Écosse.
fn cell_of(lat: f64, lng: f64) -> (i32, i32) {
    const M_PER_DEG: f64 = 111_320.0;
    let row = (lat * M_PER_DEG / LOCAL_HEIGHT_CELL_M).floor();
    let row_lat = (row + 0.5) * LOCAL_HEIGHT_CELL_M / M_PER_DEG;
    let col = (lng * M_PER_DEG * row_lat.to_radians().cos() / LOCAL_HEIGHT_CELL_M).floor();
    (col as i32, row as i32)
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
        leaf_type: None,
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
        leaf_type: Some(LeafType::from_tags(tags)),
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
        leaf_type: LeafType::from_tags(tags),
    }
}

/// Établissement à partir de ses tags.
/// Tag OSM `direction` → degrés depuis le nord.
///
/// Le wiki accepte deux écritures et les deux existent en base : des degrés
/// (`direction=225`) et des points cardinaux (`direction=SW`, jusqu'à 16
/// points). Les valeurs hors de ces deux formes (`both`, plages…) sont
/// ignorées plutôt que devinées.
fn parse_direction_deg(raw: &str) -> Option<f64> {
    if let Ok(deg) = raw.trim().parse::<f64>() {
        return Some(deg.rem_euclid(360.0));
    }
    let cardinal: &[(&str, f64)] = &[
        ("N", 0.0), ("NNE", 22.5), ("NE", 45.0), ("ENE", 67.5),
        ("E", 90.0), ("ESE", 112.5), ("SE", 135.0), ("SSE", 157.5),
        ("S", 180.0), ("SSW", 202.5), ("SW", 225.0), ("WSW", 247.5),
        ("W", 270.0), ("WNW", 292.5), ("NW", 315.0), ("NNW", 337.5),
    ];
    let upper = raw.trim().to_ascii_uppercase();
    cardinal.iter().find(|(k, _)| *k == upper).map(|(_, v)| *v)
}

/// Tag booléen OSM : seul « no » est un refus, tout le reste vaut oui — même
/// convention que `outdoor_seating`.
fn parse_yes_no(raw: &str) -> bool {
    raw != "no"
}

pub fn place_from(osm_id: String, lat: f64, lng: f64, tags: &HashMap<String, String>) -> Place {
    Place {
        osm_id,
        name: tags.get("name").cloned(),
        // Le mobilier prime : une table de pique-nique n'a pas d'`amenity`
        // (elle vient de `leisure=picnic_table`), et la normaliser ici évite à
        // tout le reste du pipeline de connaître deux clés de tag.
        amenity: furniture_kind(tags)
            .map(str::to_string)
            .or_else(|| tags.get("amenity").cloned()),
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
        direction_deg: tags.get("direction").and_then(|d| parse_direction_deg(d)),
        covered: tags.get("covered").map(|v| parse_yes_no(v)),
        backrest: tags.get("backrest").map(|v| parse_yes_no(v)),
        seats: tags.get("seats").and_then(|s| s.trim().parse().ok()),
        material: tags.get("material").cloned(),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn direction_numerique_et_cardinale() {
        assert_eq!(parse_direction_deg("225"), Some(225.0));
        assert_eq!(parse_direction_deg("SW"), Some(225.0));
        assert_eq!(parse_direction_deg("sw"), Some(225.0));
        assert_eq!(parse_direction_deg("NNW"), Some(337.5));
        // Normalisé dans [0, 360) : certains mappeurs écrivent -90 ou 450.
        assert_eq!(parse_direction_deg("-90"), Some(270.0));
        // Valeurs libres du wiki qu'on refuse de deviner.
        assert_eq!(parse_direction_deg("both"), None);
        assert_eq!(parse_direction_deg("90-180"), None);
    }

    #[test]
    fn mobilier_normalise_en_amenity() {
        let mut tags = HashMap::new();
        tags.insert("leisure".to_string(), "picnic_table".to_string());
        tags.insert("covered".to_string(), "yes".to_string());
        tags.insert("direction".to_string(), "SE".to_string());
        tags.insert("seats".to_string(), "6".to_string());
        let p = place_from("node/1".into(), 48.0, 2.0, &tags);
        assert_eq!(p.amenity.as_deref(), Some("picnic_table"));
        assert_eq!(p.covered, Some(true));
        assert_eq!(p.direction_deg, Some(135.0));
        assert_eq!(p.seats, Some(6));

        let mut bench = HashMap::new();
        bench.insert("amenity".to_string(), "bench".to_string());
        bench.insert("backrest".to_string(), "no".to_string());
        let b = place_from("node/2".into(), 48.0, 2.0, &bench);
        assert_eq!(b.amenity.as_deref(), Some("bench"));
        assert_eq!(b.backrest, Some(false));
        assert_eq!(b.direction_deg, None);

        let mut playground = HashMap::new();
        playground.insert("leisure".to_string(), "playground".to_string());
        let g = place_from("way/3".into(), 48.0, 2.0, &playground);
        assert_eq!(g.amenity.as_deref(), Some("playground"));

        let mut fitness = HashMap::new();
        fitness.insert("leisure".to_string(), "fitness_station".to_string());
        let f = place_from("way/4".into(), 48.0, 2.0, &fitness);
        assert_eq!(f.amenity.as_deref(), Some("fitness_station"));
    }

    fn leaf_of(pairs: &[(&str, &str)]) -> LeafType {
        let tags = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        LeafType::from_tags(&tags)
    }

    /// `leaf_type` fait foi quand il est là — c'est le tag prévu pour ça, et il
    /// doit gagner même contre un genre qui dirait autre chose.
    #[test]
    fn leaf_type_prime_sur_le_genre() {
        assert_eq!(leaf_of(&[("leaf_type", "palm")]), LeafType::Palm);
        assert_eq!(
            leaf_of(&[("leaf_type", "broadleaved"), ("genus", "Phoenix")]),
            LeafType::Broadleaved
        );
    }

    /// Le repli par genre, sur les trois tags qui le portent. La casse et le
    /// nom d'espèce complet ne doivent rien changer.
    #[test]
    fn genre_rattrape_un_leaf_type_absent() {
        assert_eq!(leaf_of(&[("genus", "Areca")]), LeafType::Palm);
        assert_eq!(leaf_of(&[("genus", "areca")]), LeafType::Palm);
        assert_eq!(leaf_of(&[("species", "Phoenix canariensis")]), LeafType::Palm);
        assert_eq!(leaf_of(&[("taxon", "Washingtonia robusta")]), LeafType::Palm);
        assert_eq!(leaf_of(&[("species", "Pinus nigra")]), LeafType::Needleleaved);
        assert_eq!(leaf_of(&[("genus", "Platanus")]), LeafType::Broadleaved);
    }

    /// Les sosies : pas des palmiers, mais la même silhouette — un tronc nu
    /// couronné de longues feuilles. Le modèle de feuillu leur irait plus mal.
    #[test]
    fn les_sosies_prennent_la_silhouette_palmier() {
        for genus in ["Cycas", "Yucca", "Cordyline", "Dracaena", "Musa"] {
            assert_eq!(leaf_of(&[("genus", genus)]), LeafType::Palm, "{genus}");
        }
    }

    /// Sans rien pour trancher, feuillu : c'est ~80 % des arbres urbains
    /// d'Europe, donc le pari le moins souvent faux.
    #[test]
    fn repli_feuillu_sans_indice() {
        assert_eq!(leaf_of(&[]), LeafType::Broadleaved);
        assert_eq!(leaf_of(&[("genus", "")]), LeafType::Broadleaved);
        assert_eq!(leaf_of(&[("leaf_type", "n_importe_quoi")]), LeafType::Broadleaved);
    }

    /// Un genre ne peut pas être dans deux tables à la fois — la première qui
    /// répond gagnerait en silence.
    #[test]
    fn aucun_genre_dans_deux_tables() {
        for g in PALM_GENERA {
            assert!(!NEEDLE_GENERA.contains(g), "{g} est dans les deux tables");
        }
    }

    // ------------------------------------------------- médiane locale

    /// Carré de ~10 m posé à (lat, lng) ; `height` = `None` pour un non taggé.
    fn bat(lat: f64, lng: f64, height: Option<f32>) -> Building {
        let d = 0.00005;
        Building {
            osm_id: "way/1".into(),
            name: None,
            rings: vec![vec![(lat, lng), (lat + d, lng), (lat + d, lng + d), (lat, lng + d)]],
            height_m: height.unwrap_or(f32::NAN),
            height_from_osm: height.is_some(),
            leaf_type: None,
        }
    }

    /// Quartier dense (Javel) et bourg pavillonnaire (Le Vésinet), à ~15 km.
    const JAVEL: (f64, f64) = (48.8418, 2.2785);
    const VESINET: (f64, f64) = (48.8900, 2.1350);

    #[test]
    fn mediane_locale_suit_le_quartier() {
        let mut lot: Vec<Building> = Vec::new();
        for i in 0..20 {
            lot.push(bat(JAVEL.0 + i as f64 * 1e-5, JAVEL.1, Some(21.0)));
        }
        // Plus nombreux : ce sont eux qui font la médiane de l'extrait.
        for i in 0..40 {
            lot.push(bat(VESINET.0 + i as f64 * 1e-5, VESINET.1, Some(7.0)));
        }
        lot.push(bat(JAVEL.0 + 0.0003, JAVEL.1 + 0.0003, None));
        lot.push(bat(VESINET.0 + 0.0003, VESINET.1 + 0.0003, None));
        let fallback = fill_missing_heights(&mut lot);

        assert_eq!(fallback, 7.0);
        assert_eq!(lot[60].height_m, 21.0, "immeuble non taggé de Javel");
        assert_eq!(lot[61].height_m, 7.0, "pavillon non taggé du Vésinet");
        assert!(!lot[60].height_from_osm, "la hauteur reste marquée devinée");
        assert_eq!(lot[0].height_m, 21.0, "une hauteur taggée n'est pas touchée");
    }

    #[test]
    fn cellule_clairsemee_elargie_aux_voisines() {
        let mut h = LocalHeights::default();
        // Les hauteurs sont dans la cellule d'à côté (~300 m au nord) : la
        // cellule du bâtiment est vide, le 3×3 les trouve.
        for i in 0..LOCAL_HEIGHT_MIN_SAMPLES {
            h.add(&bat(JAVEL.0 + 0.0027 + i as f64 * 1e-5, JAVEL.1, Some(24.0)));
        }
        h.finish();
        assert_eq!(h.height_for(&bat(JAVEL.0, JAVEL.1, None)), 24.0);
        assert_eq!(h.local_hits, 1);
    }

    #[test]
    fn trop_peu_de_hauteurs_autour_repli_extrait() {
        let mut h = LocalHeights::default();
        // Quatorze hauteurs ne suffisent pas, même en 5×5.
        for i in 0..LOCAL_HEIGHT_MIN_SAMPLES - 1 {
            h.add(&bat(JAVEL.0 + i as f64 * 1e-5, JAVEL.1, Some(24.0)));
        }
        // Le reste de l'extrait est loin (> 5 cellules).
        for i in 0..30 {
            h.add(&bat(VESINET.0 + i as f64 * 1e-5, VESINET.1, Some(8.0)));
        }
        h.finish();
        assert_eq!(h.fallback(), 8.0);
        assert_eq!(h.height_for(&bat(JAVEL.0, JAVEL.1, None)), 8.0);
        assert_eq!(h.fallback_hits, 1);
    }

    #[test]
    fn mediane_bornee_et_extrait_sans_tag() {
        let mut h = LocalHeights::default();
        for i in 0..LOCAL_HEIGHT_MIN_SAMPLES {
            h.add(&bat(JAVEL.0 + i as f64 * 1e-5, JAVEL.1, Some(120.0)));
        }
        h.finish();
        assert_eq!(h.height_for(&bat(JAVEL.0, JAVEL.1, None)), 40.0);

        let mut vide = LocalHeights::default();
        vide.finish();
        assert_eq!(vide.height_for(&bat(JAVEL.0, JAVEL.1, None)), DEFAULT_BUILDING_HEIGHT_M);
    }

    #[test]
    fn cellules_d_environ_250_m_a_toute_latitude() {
        for lat in [37.0, 48.85, 57.0] {
            let (c0, r0) = cell_of(lat, 2.0);
            // ~245 m vers l'est et vers le nord restent à une cellule au plus.
            let dlng = 245.0 / (111_320.0 * f64::cos(lat.to_radians()));
            let (c1, _) = cell_of(lat, 2.0 + dlng);
            let (_, r1) = cell_of(lat + 245.0 / 111_320.0, 2.0);
            assert!((c1 - c0).abs() <= 1 && (r1 - r0).abs() <= 1, "lat {lat}");
            // Et ~510 m vers l'est franchissent au moins deux frontières.
            let (c2, _) = cell_of(lat, 2.0 + dlng * 510.0 / 245.0);
            assert!(c2 - c0 >= 2, "lat {lat}");
        }
    }
}
