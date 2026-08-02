//! Tuiles binaires de bâtiments : la table `buildings` sortie de PostgreSQL.
//!
//! La table pesait ~90 % de la base (1,1 Go pour la seule Île-de-France), et
//! chaque assemblage de DSM payait une requête GIST puis un parsing WKT
//! texte. Ici : un fichier unique de tuiles pré-découpées sur la grille de la
//! DSM (z15, tuiles de 512 px Web Mercator — les mêmes que les tuiles DEM),
//! lu en accès direct via un index en mémoire.
//!
//! Format **interne**, pas du MVT : la seule consommatrice est la
//! rasterisation `stamp_buildings`, qui veut des anneaux (lat, lon) — un
//! format standard n'apporterait que des dépendances. Les tuiles *client*
//! (affichage des arbres dans Mapbox) seront du vrai MVT, pipeline séparé.
//!
//! Layout du fichier (tout en little-endian) :
//!
//! ```text
//! magic "HBT1"
//! tile_count: u64
//! index: tile_count × { x: u32, y: u32, offset: u64, len: u64 }
//! blobs, par tuile :
//!   building_count: u32
//!   par bâtiment :
//!     osm_id_len: u16, octets UTF-8
//!     name_len:   u16, octets UTF-8 (0 = pas de nom)
//!     height_m: f32, height_from_osm: u8
//!     ring_count: u16
//!     par anneau : point_count: u32, puis point_count × { dx: i32, dy: i32 }
//! ```
//!
//! Les points sont en **pixels monde z15 relatifs à l'origine de la tuile**,
//! en virgule fixe ×256 (~6 mm) : 8 octets par point au lieu de 16 en f64,
//! pour une erreur de dé-quantification négligeable devant le pixel de DSM
//! (~1,57 m). Un bâtiment à cheval sur plusieurs tuiles est écrit **entier**
//! dans chacune (le lecteur dédoublonne par `osm_id`) : la rasterisation et le
//! blocker gardent ainsi la géométrie complète, comme avec PostGIS.

use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom, Write};

use crate::dem::{latlon_of_world_px, world_px, TILE_SIZE};
use crate::osm::Building;

const MAGIC: &[u8; 4] = b"HBT1";
/// Sous-précision de la virgule fixe (1/256 de pixel monde).
const FIXED: f64 = 256.0;

// ------------------------------------------------------------- écriture

/// Accumule les bâtiments tuile par tuile, puis écrit le fichier d'un bloc.
///
/// Tout tient en mémoire (octets encodés, partagés entre tuiles) : ~500 Mo
/// pour l'Île-de-France. Pour un très grand pays, générer par sous-extraits
/// et fusionner — limitation assumée de la v1.
pub struct TileWriter {
    tiles: HashMap<(u32, u32), Vec<std::sync::Arc<[u8]>>>,
}

impl TileWriter {
    pub fn new() -> Self {
        Self { tiles: HashMap::new() }
    }

    pub fn add(&mut self, building: &Building) {
        // Emprise en pixels monde → tuiles couvertes.
        let (mut min_x, mut min_y) = (f64::MAX, f64::MAX);
        let (mut max_x, mut max_y) = (f64::MIN, f64::MIN);
        for ring in &building.rings {
            for &(lat, lon) in ring {
                let (wx, wy) = world_px(lat, lon);
                min_x = min_x.min(wx);
                min_y = min_y.min(wy);
                max_x = max_x.max(wx);
                max_y = max_y.max(wy);
            }
        }
        if min_x > max_x {
            return;
        }
        let ts = TILE_SIZE as f64;
        let tx0 = (min_x / ts).floor().max(0.0) as u32;
        let ty0 = (min_y / ts).floor().max(0.0) as u32;
        let tx1 = (max_x / ts).floor().max(0.0) as u32;
        let ty1 = (max_y / ts).floor().max(0.0) as u32;

        for ty in ty0..=ty1 {
            for tx in tx0..=tx1 {
                let blob: std::sync::Arc<[u8]> = encode(building, tx, ty).into();
                self.tiles.entry((tx, ty)).or_default().push(blob);
            }
        }
    }

    pub fn write_to(&self, mut out: impl Write + Seek) -> std::io::Result<()> {
        out.write_all(MAGIC)?;
        out.write_all(&(self.tiles.len() as u64).to_le_bytes())?;

        // Index d'abord, rempli de zéros, complété après coup : évite de
        // tenir deux fois les blobs en mémoire pour calculer les offsets.
        let index_pos = 4 + 8;
        let entry_size = 4 + 4 + 8 + 8;
        out.write_all(&vec![0u8; self.tiles.len() * entry_size])?;

        let mut entries: Vec<(u32, u32, u64, u64)> = Vec::with_capacity(self.tiles.len());
        let mut keys: Vec<&(u32, u32)> = self.tiles.keys().collect();
        keys.sort();
        for key in keys {
            let blobs = &self.tiles[key];
            let offset = out.stream_position()?;
            out.write_all(&(blobs.len() as u32).to_le_bytes())?;
            for blob in blobs {
                out.write_all(blob)?;
            }
            let len = out.stream_position()? - offset;
            entries.push((key.0, key.1, offset, len));
        }

        out.seek(SeekFrom::Start(index_pos))?;
        for (x, y, offset, len) in entries {
            out.write_all(&x.to_le_bytes())?;
            out.write_all(&y.to_le_bytes())?;
            out.write_all(&offset.to_le_bytes())?;
            out.write_all(&len.to_le_bytes())?;
        }
        Ok(())
    }

    pub fn tile_count(&self) -> usize {
        self.tiles.len()
    }
}

fn encode(b: &Building, tx: u32, ty: u32) -> Vec<u8> {
    let origin_x = tx as f64 * TILE_SIZE as f64;
    let origin_y = ty as f64 * TILE_SIZE as f64;
    let mut out = Vec::with_capacity(64);

    let id = b.osm_id.as_bytes();
    out.extend_from_slice(&(id.len() as u16).to_le_bytes());
    out.extend_from_slice(id);
    let name = b.name.as_deref().unwrap_or("").as_bytes();
    out.extend_from_slice(&(name.len() as u16).to_le_bytes());
    out.extend_from_slice(name);
    out.extend_from_slice(&b.height_m.to_le_bytes());
    out.push(b.height_from_osm as u8);
    out.extend_from_slice(&(b.rings.len() as u16).to_le_bytes());
    for ring in &b.rings {
        out.extend_from_slice(&(ring.len() as u32).to_le_bytes());
        for &(lat, lon) in ring {
            let (wx, wy) = world_px(lat, lon);
            let dx = ((wx - origin_x) * FIXED).round() as i32;
            let dy = ((wy - origin_y) * FIXED).round() as i32;
            out.extend_from_slice(&dx.to_le_bytes());
            out.extend_from_slice(&dy.to_le_bytes());
        }
    }
    out
}

// -------------------------------------------------------------- lecture

/// Fichier de tuiles ouvert : index en mémoire, blobs lus à la demande.
pub struct TileStore {
    file: std::sync::Mutex<std::fs::File>,
    index: HashMap<(u32, u32), (u64, u64)>,
}

impl TileStore {
    pub fn open(path: &str) -> std::io::Result<Self> {
        let mut file = std::fs::File::open(path)?;
        let mut magic = [0u8; 4];
        file.read_exact(&mut magic)?;
        if &magic != MAGIC {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "pas un fichier HBT1",
            ));
        }
        let mut buf8 = [0u8; 8];
        file.read_exact(&mut buf8)?;
        let count = u64::from_le_bytes(buf8) as usize;

        let mut raw = vec![0u8; count * 24];
        file.read_exact(&mut raw)?;
        let mut index = HashMap::with_capacity(count);
        for e in raw.chunks_exact(24) {
            let x = u32::from_le_bytes(e[0..4].try_into().unwrap());
            let y = u32::from_le_bytes(e[4..8].try_into().unwrap());
            let offset = u64::from_le_bytes(e[8..16].try_into().unwrap());
            let len = u64::from_le_bytes(e[16..24].try_into().unwrap());
            index.insert((x, y), (offset, len));
        }
        Ok(Self {
            file: std::sync::Mutex::new(file),
            index,
        })
    }

    /// Bâtiments de l'intervalle de tuiles (bornes incluses), dédoublonnés —
    /// un bâtiment à cheval est écrit dans chaque tuile qu'il touche.
    pub fn buildings(&self, tx0: u32, ty0: u32, tx1: u32, ty1: u32) -> std::io::Result<Vec<Building>> {
        let mut seen = std::collections::HashSet::new();
        let mut out = Vec::new();
        for ty in ty0..=ty1 {
            for tx in tx0..=tx1 {
                let Some(&(offset, len)) = self.index.get(&(tx, ty)) else {
                    continue;
                };
                let mut blob = vec![0u8; len as usize];
                {
                    let mut file = self.file.lock().unwrap();
                    file.seek(SeekFrom::Start(offset))?;
                    file.read_exact(&mut blob)?;
                }
                decode_tile(&blob, tx, ty, &mut seen, &mut out)?;
            }
        }
        Ok(out)
    }
}

fn decode_tile(
    blob: &[u8],
    tx: u32,
    ty: u32,
    seen: &mut std::collections::HashSet<String>,
    out: &mut Vec<Building>,
) -> std::io::Result<()> {
    let origin_x = tx as f64 * TILE_SIZE as f64;
    let origin_y = ty as f64 * TILE_SIZE as f64;
    let bad = || std::io::Error::new(std::io::ErrorKind::InvalidData, "tuile HBT1 corrompue");

    let mut p = 0usize;
    let take = |p: &mut usize, n: usize| -> std::io::Result<&[u8]> {
        let s = blob.get(*p..*p + n).ok_or_else(bad)?;
        *p += n;
        Ok(s)
    };

    let count = u32::from_le_bytes(take(&mut p, 4)?.try_into().unwrap());
    for _ in 0..count {
        let id_len = u16::from_le_bytes(take(&mut p, 2)?.try_into().unwrap()) as usize;
        let osm_id = String::from_utf8(take(&mut p, id_len)?.to_vec()).map_err(|_| bad())?;
        let name_len = u16::from_le_bytes(take(&mut p, 2)?.try_into().unwrap()) as usize;
        let name = String::from_utf8(take(&mut p, name_len)?.to_vec()).map_err(|_| bad())?;
        let height_m = f32::from_le_bytes(take(&mut p, 4)?.try_into().unwrap());
        let height_from_osm = take(&mut p, 1)?[0] != 0;
        let ring_count = u16::from_le_bytes(take(&mut p, 2)?.try_into().unwrap()) as usize;

        let duplicate = !seen.insert(osm_id.clone());
        let mut rings = Vec::with_capacity(if duplicate { 0 } else { ring_count });
        for _ in 0..ring_count {
            let n = u32::from_le_bytes(take(&mut p, 4)?.try_into().unwrap()) as usize;
            let pts = take(&mut p, n * 8)?;
            if duplicate {
                continue;
            }
            let mut ring = Vec::with_capacity(n);
            for pt in pts.chunks_exact(8) {
                let dx = i32::from_le_bytes(pt[0..4].try_into().unwrap()) as f64 / FIXED;
                let dy = i32::from_le_bytes(pt[4..8].try_into().unwrap()) as f64 / FIXED;
                let (lat, lon) = latlon_of_world_px(origin_x + dx, origin_y + dy);
                ring.push((lat, lon));
            }
            rings.push(ring);
        }
        if !duplicate {
            out.push(Building {
                osm_id,
                name: (!name.is_empty()).then_some(name),
                rings,
                height_m,
                height_from_osm,
                // Les tuiles ne portent que des bâtiments — la végétation a
                // ses propres tuiles (`canopy_tiles`).
                leaf_type: None,
            });
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn square(osm_id: &str, lat: f64, lon: f64, d: f64) -> Building {
        Building {
            osm_id: osm_id.into(),
            name: Some("Test".into()),
            rings: vec![vec![
                (lat, lon),
                (lat + d, lon),
                (lat + d, lon + d),
                (lat, lon + d),
                (lat, lon),
            ]],
            height_m: 12.5,
            height_from_osm: true,
            leaf_type: None,
        }
    }

    /// Aller-retour complet : les coordonnées reviennent au centimètre près,
    /// les attributs à l'identique, et un bâtiment à cheval sur deux tuiles
    /// n'est renvoyé qu'une fois.
    #[test]
    fn roundtrip_through_file() {
        let mut writer = TileWriter::new();
        writer.add(&square("way/1", 48.85, 2.35, 0.0005));
        // Grand : couvre plusieurs tuiles à coup sûr (~0,03° ≈ 2,2 km).
        writer.add(&square("way/2", 48.86, 2.33, 0.03));

        let mut buf = std::io::Cursor::new(Vec::new());
        writer.write_to(&mut buf).unwrap();

        let dir = std::env::temp_dir().join("btiles_test.hbt");
        std::fs::write(&dir, buf.into_inner()).unwrap();
        let store = TileStore::open(dir.to_str().unwrap()).unwrap();

        // Intervalle couvrant toute la zone.
        let (wx0, wy0) = world_px(48.90, 2.30);
        let (wx1, wy1) = world_px(48.80, 2.40);
        let ts = TILE_SIZE as f64;
        let list = store
            .buildings(
                (wx0 / ts) as u32,
                (wy0 / ts) as u32,
                (wx1 / ts) as u32,
                (wy1 / ts) as u32,
            )
            .unwrap();

        assert_eq!(list.len(), 2, "dédoublonnage : chaque bâtiment une fois");
        let b = list.iter().find(|b| b.osm_id == "way/1").unwrap();
        assert_eq!(b.name.as_deref(), Some("Test"));
        assert_eq!(b.height_m, 12.5);
        assert!(b.height_from_osm);
        let (lat, lon) = b.rings[0][0];
        assert!((lat - 48.85).abs() < 1e-6, "lat dé-quantifiée : {lat}");
        assert!((lon - 2.35).abs() < 1e-6, "lon dé-quantifiée : {lon}");
    }
}
