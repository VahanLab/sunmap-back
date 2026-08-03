//! Lecture de `sunmap.pmtiles` : l'artefact vectoriel unique de la géométrie.
//!
//! Remplace les tables PostGIS `buildings`/`woods`/`trees` au runtime
//! (variable `VECTOR_TILES=chemin.pmtiles`) : l'archive est générée par
//! `scripts/build-pmtiles.py` à l'import d'une zone, et les mêmes octets
//! servent le calcul serveur, le masque Metal client et l'affichage Mapbox —
//! ce qu'on voit est ce qui fait l'ombre, par construction.
//!
//! Deux décodeurs maison, volontairement minimaux :
//!
//! - **PMTiles v3** : en-tête de 127 octets + annuaires (racine et feuilles)
//!   gzip, aplatis en un index mémoire trié — même esprit que `btiles`
//!   (`TileStore`), lecture des tuiles à la demande par offset.
//! - **MVT** (Mapbox Vector Tile) : le sous-ensemble protobuf que produit
//!   notre générateur — couches nommées, attributs, géométries
//!   MoveTo/LineTo/ClosePath. Pas de dépendance protobuf : le format de fil
//!   est petit et stable (spec 2.1), et un décodeur générique n'apporterait
//!   que du poids.
//!
//! Convention héritée de `btiles` : un objet à cheval sur plusieurs tuiles y
//! est écrit entier dans chacune, le lecteur dédoublonne par `id`. Les anneaux
//! sont renvoyés tels quels (extérieurs et trous mélangés) : la rasterisation
//! aval est en règle pair-impair, l'orientation MVT n'a pas d'importance.

use std::collections::{HashMap, HashSet};
use std::io::{Read, Seek, SeekFrom};
use std::sync::Mutex;

use crate::osm::{Building, LeafType, Tree};

// ------------------------------------------------------------------ PMTiles

/// Entrée d'annuaire aplatie : `tile_id` (courbe de Hilbert, cf. spec) →
/// tranche du fichier. `run_length` > 1 signifie que les tuiles suivantes
/// partagent le même blob (dédoublonnage du writer).
#[derive(Clone, Copy)]
struct Entry {
    tile_id: u64,
    run_length: u64,
    offset: u64,
    length: u64,
}

pub struct VectorStore {
    file: Mutex<std::fs::File>,
    /// Toutes les entrées tuile (racine + feuilles), triées par `tile_id`.
    index: Vec<Entry>,
    tile_data_offset: u64,
    /// Compression des tuiles (2 = gzip, 1 = aucune) — celle de l'en-tête.
    tile_compression: u8,
    /// Zoom unique de l'archive (min_zoom = max_zoom chez notre générateur).
    zoom: u32,
}

fn bad(msg: &str) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidData, msg.to_string())
}

impl VectorStore {
    pub fn open(path: &str) -> std::io::Result<Self> {
        let mut file = std::fs::File::open(path)?;
        let mut header = [0u8; 127];
        file.read_exact(&mut header)?;
        if &header[0..7] != b"PMTiles" || header[7] != 3 {
            return Err(bad("pas une archive PMTiles v3"));
        }
        let u64_at = |i: usize| u64::from_le_bytes(header[i..i + 8].try_into().unwrap());
        let root_offset = u64_at(8);
        let root_length = u64_at(16);
        let leaf_offset = u64_at(40);
        let tile_data_offset = u64_at(56);
        let internal_compression = header[97];
        let tile_compression = header[98];
        let min_zoom = header[100];
        let max_zoom = header[101];
        if min_zoom != max_zoom {
            // Le générateur écrit un seul niveau ; un multi-zoom signalerait
            // une archive d'une autre provenance, qu'on refuse plutôt que de
            // mal l'interpréter.
            return Err(bad("archive multi-zoom inattendue"));
        }

        let read_slice = |file: &mut std::fs::File, offset: u64, len: u64| -> std::io::Result<Vec<u8>> {
            let mut buf = vec![0u8; len as usize];
            file.seek(SeekFrom::Start(offset))?;
            file.read_exact(&mut buf)?;
            Ok(buf)
        };
        let decode_dir = |raw: Vec<u8>| -> std::io::Result<Vec<Entry>> {
            let bytes = match internal_compression {
                1 => raw,
                2 => gunzip(&raw)?,
                other => return Err(bad(&format!("compression d'annuaire {other} non gérée"))),
            };
            parse_directory(&bytes)
        };

        let root = decode_dir(read_slice(&mut file, root_offset, root_length)?)?;
        let mut index = Vec::new();
        for entry in root {
            if entry.run_length == 0 {
                // Feuille : offset relatif à la section des annuaires feuilles.
                let leaf = decode_dir(read_slice(&mut file, leaf_offset + entry.offset, entry.length)?)?;
                for e in leaf {
                    if e.run_length == 0 {
                        return Err(bad("annuaire feuille imbriqué inattendu"));
                    }
                    index.push(e);
                }
            } else {
                index.push(entry);
            }
        }
        index.sort_by_key(|e| e.tile_id);

        Ok(Self {
            file: Mutex::new(file),
            index,
            tile_data_offset,
            tile_compression,
            zoom: min_zoom as u32,
        })
    }

    pub fn zoom(&self) -> u32 {
        self.zoom
    }

    /// Octets MVT (décompressés) de la tuile, `None` si elle est vide.
    fn tile_bytes(&self, x: u32, y: u32) -> std::io::Result<Option<Vec<u8>>> {
        let id = zxy_to_tileid(self.zoom, x, y);
        let pos = match self.index.binary_search_by_key(&id, |e| e.tile_id) {
            Ok(i) => i,
            Err(0) => return Ok(None),
            Err(i) => i - 1,
        };
        let e = self.index[pos];
        if id >= e.tile_id + e.run_length.max(1) {
            return Ok(None);
        }
        let mut buf = vec![0u8; e.length as usize];
        {
            let mut file = self.file.lock().unwrap();
            file.seek(SeekFrom::Start(self.tile_data_offset + e.offset))?;
            file.read_exact(&mut buf)?;
        }
        match self.tile_compression {
            1 => Ok(Some(buf)),
            2 => Ok(Some(gunzip(&buf)?)),
            other => Err(bad(&format!("compression de tuile {other} non gérée"))),
        }
    }

    /// Couches demandées sur toutes les tuiles couvrant la bbox, dédoublonnées
    /// par `id`. Chaque couche a son vecteur, dans l'ordre de `layers`.
    fn features_in_bbox(
        &self,
        s: f64,
        w: f64,
        n: f64,
        e: f64,
        layers: &[&str],
    ) -> std::io::Result<Vec<Vec<Feature>>> {
        let (x0, y0) = tile_of(n, w, self.zoom);
        let (x1, y1) = tile_of(s, e, self.zoom);
        let mut seen: Vec<HashSet<String>> = layers.iter().map(|_| HashSet::new()).collect();
        let mut out: Vec<Vec<Feature>> = layers.iter().map(|_| Vec::new()).collect();
        for y in y0.min(y1)..=y0.max(y1) {
            for x in x0.min(x1)..=x0.max(x1) {
                let Some(bytes) = self.tile_bytes(x, y)? else {
                    continue;
                };
                let tile = decode_mvt(&bytes, self.zoom, x, y)?;
                for (li, layer_name) in layers.iter().enumerate() {
                    let Some(features) = tile.get(*layer_name) else {
                        continue;
                    };
                    for f in features {
                        if seen[li].insert(f.id.clone()) {
                            out[li].push(f.clone());
                        }
                    }
                }
            }
        }
        Ok(out)
    }

    /// Bâtiments des tuiles couvrant la bbox (superset, comme le `&&` PostGIS).
    pub fn buildings(&self, s: f64, w: f64, n: f64, e: f64) -> std::io::Result<Vec<Building>> {
        let mut got = self.features_in_bbox(s, w, n, e, &["buildings"])?;
        Ok(got.remove(0).into_iter().filter_map(|f| f.into_building(None)).collect())
    }

    /// Emprises boisées, `leaf_type` renseigné.
    pub fn woods(&self, s: f64, w: f64, n: f64, e: f64) -> std::io::Result<Vec<Building>> {
        let mut got = self.features_in_bbox(s, w, n, e, &["woods"])?;
        Ok(got
            .remove(0)
            .into_iter()
            .filter_map(|f| {
                let leaf = LeafType::parse(f.prop_str("leaf_type"));
                f.into_building(Some(leaf))
            })
            .collect())
    }

    /// Arbres isolés, filtrés strictement à la bbox (comme le `&&` PostGIS sur
    /// un point) — l'appelant élargit sa bbox s'il veut les houppiers
    /// débordants, exactement comme avec la base.
    pub fn trees(&self, s: f64, w: f64, n: f64, e: f64) -> std::io::Result<Vec<Tree>> {
        let mut got = self.features_in_bbox(s, w, n, e, &["trees"])?;
        Ok(got
            .remove(0)
            .into_iter()
            .filter_map(|f| {
                let &(lat, lng) = f.rings.first()?.first()?;
                if !(s..=n).contains(&lat) || !(w..=e).contains(&lng) {
                    return None;
                }
                Some(Tree {
                    osm_id: f.id.clone(),
                    lat,
                    lng,
                    height_m: f.prop_f64("height_m")?,
                    crown_radius_m: f.prop_f64("crown_radius_m")?,
                    leaf_type: LeafType::parse(f.prop_str("leaf_type")),
                })
            })
            .collect())
    }
}

fn gunzip(raw: &[u8]) -> std::io::Result<Vec<u8>> {
    let mut out = Vec::new();
    flate2::read::GzDecoder::new(raw).read_to_end(&mut out)?;
    Ok(out)
}

/// Tuile slippy contenant le point, bornée à la grille du zoom.
fn tile_of(lat: f64, lon: f64, z: u32) -> (u32, u32) {
    let n = f64::powi(2.0, z as i32);
    let x = ((lon + 180.0) / 360.0 * n).floor().clamp(0.0, n - 1.0) as u32;
    let y = ((1.0 - lat.to_radians().tan().asinh() / std::f64::consts::PI) / 2.0 * n)
        .floor()
        .clamp(0.0, n - 1.0) as u32;
    (x, y)
}

/// Identifiant de tuile PMTiles : tuiles des zooms précédents, puis position
/// sur la courbe de Hilbert du zoom courant (spec v3).
pub fn zxy_to_tileid(z: u32, x: u32, y: u32) -> u64 {
    let mut acc: u64 = 0;
    for t in 0..z {
        acc += 1u64 << (2 * t);
    }
    acc + hilbert_d(z, x, y)
}

/// Distance de Hilbert (xy → d) sur une grille 2^z × 2^z.
fn hilbert_d(z: u32, x: u32, y: u32) -> u64 {
    let n: u64 = 1 << z;
    let (mut x, mut y) = (x as u64, y as u64);
    let mut d: u64 = 0;
    let mut s = n / 2;
    while s > 0 {
        let rx = u64::from(x & s > 0);
        let ry = u64::from(y & s > 0);
        d += s * s * ((3 * rx) ^ ry);
        // Rotation.
        if ry == 0 {
            if rx == 1 {
                x = s.wrapping_sub(1).wrapping_sub(x);
                y = s.wrapping_sub(1).wrapping_sub(y);
            }
            std::mem::swap(&mut x, &mut y);
        }
        s /= 2;
    }
    d
}

/// Annuaire PMTiles : quatre colonnes de varints (ids en delta, run_lengths,
/// longueurs, offsets avec le raccourci « 0 = contigu au précédent »).
fn parse_directory(bytes: &[u8]) -> std::io::Result<Vec<Entry>> {
    let mut p = 0usize;
    let n = read_varint(bytes, &mut p)? as usize;
    let mut entries = vec![
        Entry { tile_id: 0, run_length: 0, offset: 0, length: 0 };
        n
    ];
    let mut id = 0u64;
    for e in entries.iter_mut() {
        id += read_varint(bytes, &mut p)?;
        e.tile_id = id;
    }
    for e in entries.iter_mut() {
        e.run_length = read_varint(bytes, &mut p)?;
    }
    for e in entries.iter_mut() {
        e.length = read_varint(bytes, &mut p)?;
    }
    for i in 0..n {
        let v = read_varint(bytes, &mut p)?;
        entries[i].offset = if v == 0 {
            if i == 0 {
                return Err(bad("premier offset d'annuaire implicite"));
            }
            entries[i - 1].offset + entries[i - 1].length
        } else {
            v - 1
        };
    }
    Ok(entries)
}

// ---------------------------------------------------------------------- MVT

/// Objet décodé d'une couche : identifiant, attributs, anneaux (lat, lon).
/// Un point est un anneau d'un seul sommet.
#[derive(Clone)]
struct Feature {
    id: String,
    props: HashMap<String, PropValue>,
    rings: Vec<Vec<(f64, f64)>>,
}

#[derive(Clone)]
enum PropValue {
    Str(String),
    Num(f64),
    Bool(bool),
}

impl Feature {
    fn prop_str(&self, key: &str) -> Option<&str> {
        match self.props.get(key) {
            Some(PropValue::Str(s)) => Some(s),
            _ => None,
        }
    }

    fn prop_f64(&self, key: &str) -> Option<f64> {
        match self.props.get(key) {
            Some(PropValue::Num(v)) => Some(*v),
            _ => None,
        }
    }

    fn into_building(self, leaf_type: Option<LeafType>) -> Option<Building> {
        if self.rings.is_empty() {
            return None;
        }
        Some(Building {
            height_m: self.prop_f64("height_m")? as f32,
            height_from_osm: matches!(self.props.get("height_from_osm"), Some(PropValue::Bool(true))),
            name: self.prop_str("name").map(str::to_string),
            osm_id: self.id,
            rings: self.rings,
            leaf_type,
        })
    }
}

/// Décode une tuile MVT en couches nommées. `(z, x, y)` sert à reprojeter les
/// coordonnées de tuile (extent, y vers le bas) en (lat, lon).
fn decode_mvt(
    bytes: &[u8],
    z: u32,
    x: u32,
    y: u32,
) -> std::io::Result<HashMap<String, Vec<Feature>>> {
    let mut layers = HashMap::new();
    let mut p = 0usize;
    while p < bytes.len() {
        let (field, wire) = read_tag(bytes, &mut p)?;
        if field == 3 && wire == 2 {
            let msg = read_bytes(bytes, &mut p)?;
            let (name, features) = decode_layer(msg, z, x, y)?;
            layers.insert(name, features);
        } else {
            skip_field(bytes, &mut p, wire)?;
        }
    }
    Ok(layers)
}

fn decode_layer(bytes: &[u8], z: u32, x: u32, y: u32) -> std::io::Result<(String, Vec<Feature>)> {
    let mut name = String::new();
    let mut extent = 4096u64;
    let mut keys: Vec<String> = Vec::new();
    let mut values: Vec<PropValue> = Vec::new();
    let mut raw_features: Vec<&[u8]> = Vec::new();

    let mut p = 0usize;
    while p < bytes.len() {
        let (field, wire) = read_tag(bytes, &mut p)?;
        match (field, wire) {
            (1, 2) => name = String::from_utf8_lossy(read_bytes(bytes, &mut p)?).into_owned(),
            (2, 2) => raw_features.push(read_bytes(bytes, &mut p)?),
            (3, 2) => keys.push(String::from_utf8_lossy(read_bytes(bytes, &mut p)?).into_owned()),
            (4, 2) => values.push(decode_value(read_bytes(bytes, &mut p)?)?),
            (5, 0) => extent = read_varint(bytes, &mut p)?,
            _ => skip_field(bytes, &mut p, wire)?,
        }
    }

    let features = raw_features
        .into_iter()
        .filter_map(|raw| decode_feature(raw, &keys, &values, extent, z, x, y).ok().flatten())
        .collect();
    Ok((name, features))
}

fn decode_value(bytes: &[u8]) -> std::io::Result<PropValue> {
    let mut p = 0usize;
    let mut out = PropValue::Str(String::new());
    while p < bytes.len() {
        let (field, wire) = read_tag(bytes, &mut p)?;
        out = match (field, wire) {
            (1, 2) => PropValue::Str(String::from_utf8_lossy(read_bytes(bytes, &mut p)?).into_owned()),
            (2, 5) => {
                let v = f32::from_le_bytes(bytes[p..p + 4].try_into().map_err(|_| bad("float"))?);
                p += 4;
                PropValue::Num(v as f64)
            }
            (3, 1) => {
                let v = f64::from_le_bytes(bytes[p..p + 8].try_into().map_err(|_| bad("double"))?);
                p += 8;
                PropValue::Num(v)
            }
            (4, 0) => PropValue::Num(read_varint(bytes, &mut p)? as i64 as f64),
            (5, 0) => PropValue::Num(read_varint(bytes, &mut p)? as f64),
            (6, 0) => PropValue::Num(zigzag(read_varint(bytes, &mut p)?) as f64),
            (7, 0) => PropValue::Bool(read_varint(bytes, &mut p)? != 0),
            _ => {
                skip_field(bytes, &mut p, wire)?;
                continue;
            }
        };
    }
    Ok(out)
}

fn decode_feature(
    bytes: &[u8],
    keys: &[String],
    values: &[PropValue],
    extent: u64,
    z: u32,
    tx: u32,
    ty: u32,
) -> std::io::Result<Option<Feature>> {
    let mut tags: Vec<u64> = Vec::new();
    let mut geometry: Vec<u64> = Vec::new();

    let mut p = 0usize;
    while p < bytes.len() {
        let (field, wire) = read_tag(bytes, &mut p)?;
        match (field, wire) {
            (2, 2) => read_packed(bytes, &mut p, &mut tags)?,
            (2, 0) => tags.push(read_varint(bytes, &mut p)?),
            (4, 2) => read_packed(bytes, &mut p, &mut geometry)?,
            (4, 0) => geometry.push(read_varint(bytes, &mut p)?),
            _ => skip_field(bytes, &mut p, wire)?,
        }
    }

    let mut props = HashMap::new();
    for pair in tags.chunks_exact(2) {
        let (k, v) = (pair[0] as usize, pair[1] as usize);
        if k < keys.len() && v < values.len() {
            props.insert(keys[k].clone(), values[v].clone());
        }
    }
    let Some(PropValue::Str(id)) = props.get("id").cloned() else {
        // Sans identifiant, impossible de dédoublonner entre tuiles : on
        // écarte plutôt que de compter un objet plusieurs fois.
        return Ok(None);
    };

    // Reprojection : coordonnée de tuile (extent, y vers le bas) → (lat, lon).
    let n = f64::powi(2.0, z as i32);
    let to_latlon = |gx: i64, gy: i64| -> (f64, f64) {
        let fx = tx as f64 + gx as f64 / extent as f64;
        let fy = ty as f64 + gy as f64 / extent as f64;
        let lon = fx / n * 360.0 - 180.0;
        let lat = (std::f64::consts::PI * (1.0 - 2.0 * fy / n)).sinh().atan().to_degrees();
        (lat, lon)
    };

    let mut rings: Vec<Vec<(f64, f64)>> = Vec::new();
    let (mut cx, mut cy) = (0i64, 0i64);
    let mut i = 0usize;
    while i < geometry.len() {
        let cmd = geometry[i];
        i += 1;
        let (op, count) = (cmd & 0x7, cmd >> 3);
        match op {
            1 | 2 => {
                // MoveTo (1) ouvre un anneau par point, LineTo (2) prolonge
                // le dernier anneau ouvert.
                for _ in 0..count {
                    let dx = zigzag(*geometry.get(i).ok_or_else(|| bad("géométrie tronquée"))?);
                    let dy = zigzag(*geometry.get(i + 1).ok_or_else(|| bad("géométrie tronquée"))?);
                    i += 2;
                    cx += dx;
                    cy += dy;
                    if op == 1 {
                        rings.push(vec![to_latlon(cx, cy)]);
                    } else if let Some(ring) = rings.last_mut() {
                        ring.push(to_latlon(cx, cy));
                    }
                }
            }
            7 => {} // ClosePath : la rasterisation referme implicitement.
            other => return Err(bad(&format!("commande MVT {other} inconnue"))),
        }
    }

    Ok(Some(Feature { id, props, rings }))
}

// ----------------------------------------------------------- protobuf de base

fn read_varint(bytes: &[u8], p: &mut usize) -> std::io::Result<u64> {
    let mut out = 0u64;
    let mut shift = 0u32;
    loop {
        let b = *bytes.get(*p).ok_or_else(|| bad("varint tronqué"))?;
        *p += 1;
        out |= u64::from(b & 0x7f) << shift;
        if b & 0x80 == 0 {
            return Ok(out);
        }
        shift += 7;
        if shift >= 64 {
            return Err(bad("varint trop long"));
        }
    }
}

fn zigzag(v: u64) -> i64 {
    ((v >> 1) as i64) ^ -((v & 1) as i64)
}

fn read_tag(bytes: &[u8], p: &mut usize) -> std::io::Result<(u64, u64)> {
    let tag = read_varint(bytes, p)?;
    Ok((tag >> 3, tag & 0x7))
}

fn read_bytes<'a>(bytes: &'a [u8], p: &mut usize) -> std::io::Result<&'a [u8]> {
    let len = read_varint(bytes, p)? as usize;
    let out = bytes.get(*p..*p + len).ok_or_else(|| bad("champ tronqué"))?;
    *p += len;
    Ok(out)
}

fn read_packed(bytes: &[u8], p: &mut usize, out: &mut Vec<u64>) -> std::io::Result<()> {
    let slice = read_bytes(bytes, p)?;
    let mut q = 0usize;
    while q < slice.len() {
        out.push(read_varint(slice, &mut q)?);
    }
    Ok(())
}

fn skip_field(bytes: &[u8], p: &mut usize, wire: u64) -> std::io::Result<()> {
    match wire {
        0 => {
            read_varint(bytes, p)?;
        }
        1 => *p += 8,
        2 => {
            read_bytes(bytes, p)?;
        }
        5 => *p += 4,
        other => return Err(bad(&format!("wire type {other} inconnu"))),
    }
    if *p > bytes.len() {
        return Err(bad("champ tronqué"));
    }
    Ok(())
}

// -------------------------------------------------------------------- tests

#[cfg(test)]
mod tests {
    use super::*;

    /// L'archive de test est générée par `scripts/build-pmtiles.py --fixture` :
    /// une tuile z14 (Notre-Dame) avec un bâtiment, un bois à clairière et un
    /// arbre — les valeurs attendues ici sont celles de `fixture_layers()`
    /// côté Python. Deux implémentations indépendantes doivent se lire.
    fn store() -> VectorStore {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/testdata/mini.pmtiles");
        VectorStore::open(path).expect("fixture lisible")
    }

    #[test]
    fn tileid_matches_python_writer() {
        // Valeur calculée par pmtiles.tile.zxy_to_tileid côté Python.
        assert_eq!(zxy_to_tileid(14, 8298, 5636), 317_461_555);
        assert_eq!(zxy_to_tileid(0, 0, 0), 0);
        assert_eq!(zxy_to_tileid(1, 0, 0), 1);
    }

    #[test]
    fn building_roundtrip() {
        let s = store();
        assert_eq!(s.zoom(), 14);
        let buildings = s.buildings(48.85, 2.34, 48.86, 2.36).unwrap();
        assert_eq!(buildings.len(), 1);
        let b = &buildings[0];
        assert_eq!(b.osm_id, "way/1");
        assert_eq!(b.name.as_deref(), Some("Tour Test"));
        assert_eq!(b.height_m, 42.5);
        assert!(b.height_from_osm);
        assert!(b.leaf_type.is_none());
        assert_eq!(b.rings.len(), 1);
        // Coin sud-ouest du carré : (48.8525, 2.3480), à la quantification
        // près (extent 4096 sur une tuile z14 ≈ 0,6 m ≈ 6e-6 degré).
        let (lat, lon) = b.rings[0]
            .iter()
            .copied()
            .min_by(|a, b| (a.0 + a.1).partial_cmp(&(b.0 + b.1)).unwrap())
            .unwrap();
        assert!((lat - 48.8525).abs() < 2e-5, "lat {lat}");
        assert!((lon - 2.3480).abs() < 2e-5, "lon {lon}");
    }

    #[test]
    fn wood_keeps_hole_and_leaf_type() {
        let woods = store().woods(48.85, 2.34, 48.86, 2.36).unwrap();
        assert_eq!(woods.len(), 1);
        let w = &woods[0];
        assert_eq!(w.osm_id, "relation/2");
        assert_eq!(w.leaf_type, Some(LeafType::Needleleaved));
        assert!(!w.height_from_osm);
        assert_eq!(w.rings.len(), 2, "extérieur + clairière");
    }

    #[test]
    fn tree_position_and_attributes() {
        let trees = store().trees(48.85, 2.34, 48.86, 2.36).unwrap();
        assert_eq!(trees.len(), 1);
        let t = &trees[0];
        assert_eq!(t.osm_id, "node/3");
        assert_eq!(t.leaf_type, LeafType::Palm);
        assert_eq!(t.height_m, 10.0);
        assert_eq!(t.crown_radius_m, 3.0);
        assert!((t.lat - 48.8530).abs() < 2e-5, "lat {}", t.lat);
        assert!((t.lng - 2.3499).abs() < 2e-5, "lng {}", t.lng);
    }

    #[test]
    fn bbox_outside_returns_nothing() {
        let s = store();
        assert!(s.buildings(45.0, 4.0, 45.01, 4.01).unwrap().is_empty());
        assert!(s.trees(45.0, 4.0, 45.01, 4.01).unwrap().is_empty());
    }

    #[test]
    fn tree_bbox_filter_is_strict() {
        // Bbox couvrant la tuile mais pas l'arbre (2.3499, 48.8530).
        let trees = store().trees(48.8531, 2.34, 48.86, 2.36).unwrap();
        assert!(trees.is_empty());
    }
}
