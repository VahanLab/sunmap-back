//! Génère `sunmap.pmtiles` — l'archive vectorielle unique — depuis un extrait
//! GeoJSONSeq, **sans base de données et en mémoire bornée**.
//!
//!   ./scripts/osm-extract.sh france.osm.pbf extrait.geojsonl
//!   cargo run --release --bin tilegen -- extrait.geojsonl tiles/sunmap.pmtiles
//!
//! Les règles tags → hauteur (dont la médiane locale des bâtiments non
//! taggés) sont celles de `pbf`/`osm.rs` — les mêmes qu'à l'époque PostGIS,
//! qui n'existe plus pour la géométrie : l'archive EST la géométrie.
//!
//! La mémoire ne dépend pas de la taille du pays (une VM de 4 Go suffit) :
//!
//! 1. **Passe 1** (flux) : hauteurs taggées → médiane du lot (c'est le seul
//!    calcul qui a besoin de voir tout l'extrait, et il ne retient que des
//!    `f32`).
//! 2. **Passe 2** (flux) : chaque objet est sérialisé compact dans un
//!    **bucket disque** par plage d'identifiants de tuile (courbe de
//!    Hilbert : un bucket = une région contiguë) — dupliqué dans chaque
//!    tuile que sa boîte englobante touche, comme toujours.
//! 3. **Passe 3** : bucket par bucket (ordre = ordre des tuiles), regrouper
//!    par tuile, encoder le MVT, pousser au `ArchiveWriter` — qui lui-même
//!    déborde ses blobs sur disque.
//!
//! Le pic mémoire est le plus gros bucket (la conurbation la plus dense),
//! pas le pays.

use std::collections::BTreeMap;
use std::fs::File;
use std::io::{BufRead, BufReader, BufWriter, Read, Write};

use helios_server::osm::{self, Building, LeafType, Tree};
use helios_server::pbf::{self, StreamedFeature};
use helios_server::vtiles::{self, zxy_to_tileid};

/// Nombre de buckets de spill. 256 découpe la France en régions d'encodage
/// de quelques dizaines de Mo (le pire bucket — Paris — reste sous le Go).
const BUCKETS: u64 = 256;

fn main() {
    let mut args = std::env::args().skip(1);
    let (Some(input), Some(output)) = (args.next(), args.next()) else {
        eprintln!("Usage : tilegen <extrait.geojsonl> <sortie.pmtiles>");
        std::process::exit(1);
    };

    let open = |path: &str| -> BufReader<File> {
        match File::open(path) {
            Ok(f) => BufReader::with_capacity(1 << 20, f),
            Err(e) => {
                eprintln!("Impossible d'ouvrir {path} : {e}");
                std::process::exit(1);
            }
        }
    };
    let fail = |ctx: &str, e: &dyn std::fmt::Display| -> ! {
        eprintln!("{ctx} : {e}");
        std::process::exit(1);
    };

    // ------------------------------------------------ passe 1 : la médiane
    let mut known: Vec<f32> = Vec::new();
    let mut counts = (0usize, 0usize, 0usize); // bâtiments, bois, arbres
    if let Err(e) = pbf::stream_geojsonseq(open(&input), |f| {
        match f {
            StreamedFeature::Building(b) => {
                counts.0 += 1;
                if b.height_from_osm {
                    known.push(b.height_m);
                }
            }
            StreamedFeature::Wood(_) => counts.1 += 1,
            StreamedFeature::Tree(_) => counts.2 += 1,
            StreamedFeature::Place(_) => {}
        }
    }) {
        fail("Passe 1 (médiane)", &e);
    }
    known.sort_by(f32::total_cmp);
    let fallback = if known.is_empty() {
        osm::DEFAULT_BUILDING_HEIGHT_M
    } else {
        known[known.len() / 2].clamp(6.0, 40.0)
    };
    println!(
        "passe 1 : {} bâtiments ({} avec hauteur OSM, défaut {fallback:.1} m), {} bois, {} arbres",
        counts.0,
        known.len(),
        counts.1,
        counts.2
    );
    drop(known);

    // ---------------------------------------- passe 2 : spill par bucket
    let spill_dir = std::env::temp_dir().join(format!("sunmap-tilegen-{}", std::process::id()));
    if let Err(e) = std::fs::create_dir_all(&spill_dir) {
        fail("Dossier de spill", &e);
    }
    // Identifiants z14 : [base, base + 4^14). Le bucket est la tranche de
    // Hilbert — l'ordre des buckets est l'ordre exigé par l'archive.
    let id_base = zxy_to_tileid(vtiles::ZOOM, 0, 0);
    let id_span = 1u64 << (2 * vtiles::ZOOM);
    let per_bucket = id_span.div_ceil(BUCKETS);
    let mut buckets: Vec<Option<BufWriter<File>>> = (0..BUCKETS).map(|_| None).collect();
    {
        let mut write_record = |tile_id: u64, payload: &[u8]| {
            let idx = ((tile_id - id_base) / per_bucket) as usize;
            let w = buckets[idx].get_or_insert_with(|| {
                let path = spill_dir.join(format!("{idx:03}.bin"));
                BufWriter::new(File::create(path).expect("bucket de spill"))
            });
            w.write_all(&tile_id.to_le_bytes()).expect("spill");
            w.write_all(&(payload.len() as u32).to_le_bytes()).expect("spill");
            w.write_all(payload).expect("spill");
        };
        let mut spread = |rings: &[Vec<(f64, f64)>], payload: &[u8]| {
            let Some((x0, y0, x1, y1)) = vtiles::covered_tiles(rings) else {
                return;
            };
            for y in y0..=y1 {
                for x in x0..=x1 {
                    write_record(zxy_to_tileid(vtiles::ZOOM, x, y), payload);
                }
            }
        };
        if let Err(e) = pbf::stream_geojsonseq(open(&input), |f| match f {
            StreamedFeature::Building(mut b) => {
                if !b.height_from_osm {
                    b.height_m = fallback;
                }
                spread(&b.rings, &encode_polygon(0, &b));
            }
            StreamedFeature::Wood(w) => spread(&w.rings, &encode_polygon(1, &w)),
            StreamedFeature::Tree(t) => {
                let rings = vec![vec![(t.lat, t.lng)]];
                spread(&rings, &encode_tree(&t));
            }
            StreamedFeature::Place(_) => {}
        }) {
            fail("Passe 2 (spill)", &e);
        }
        for w in buckets.iter_mut().flatten() {
            w.flush().expect("spill");
        }
    }

    // ------------------------------------- passe 3 : encodage par bucket
    let mut writer = match vtiles::ArchiveWriter::new() {
        Ok(w) => w,
        Err(e) => fail("ArchiveWriter", &e),
    };
    let mut bbox = (f64::MAX, f64::MAX, f64::MIN, f64::MIN); // s, w, n, e
    for idx in 0..BUCKETS as usize {
        if buckets[idx].is_none() {
            continue;
        }
        let path = spill_dir.join(format!("{idx:03}.bin"));
        let mut raw = Vec::new();
        if let Err(e) = File::open(&path).and_then(|mut f| f.read_to_end(&mut raw)) {
            fail("Lecture de bucket", &e);
        }
        std::fs::remove_file(&path).ok();

        // Regroupe les enregistrements par tuile (BTreeMap : ordre croissant).
        let mut tiles: BTreeMap<u64, Vec<&[u8]>> = BTreeMap::new();
        let mut p = 0usize;
        while p + 12 <= raw.len() {
            let tile_id = u64::from_le_bytes(raw[p..p + 8].try_into().unwrap());
            let len = u32::from_le_bytes(raw[p + 8..p + 12].try_into().unwrap()) as usize;
            p += 12;
            tiles.entry(tile_id).or_default().push(&raw[p..p + len]);
            p += len;
        }

        for (tile_id, records) in tiles {
            let mut buildings = Vec::new();
            let mut woods = Vec::new();
            let mut trees = Vec::new();
            for r in records {
                match r[0] {
                    0 => buildings.push(decode_polygon(r)),
                    1 => woods.push(decode_polygon(r)),
                    _ => trees.push(decode_tree(r)),
                }
            }
            for b in buildings.iter().chain(woods.iter()) {
                for ring in &b.rings {
                    for &(lat, lon) in ring {
                        bbox.0 = bbox.0.min(lat);
                        bbox.1 = bbox.1.min(lon);
                        bbox.2 = bbox.2.max(lat);
                        bbox.3 = bbox.3.max(lon);
                    }
                }
            }
            for t in &trees {
                bbox.0 = bbox.0.min(t.lat);
                bbox.1 = bbox.1.min(t.lng);
                bbox.2 = bbox.2.max(t.lat);
                bbox.3 = bbox.3.max(t.lng);
            }
            let (x, y) = tileid_to_xy(tile_id);
            let b_refs: Vec<&Building> = buildings.iter().collect();
            let w_refs: Vec<&Building> = woods.iter().collect();
            let t_refs: Vec<&Tree> = trees.iter().collect();
            if let Some(mvt) = vtiles::encode_tile(x, y, &b_refs, &w_refs, &t_refs) {
                if let Err(e) = writer.add_tile(tile_id, &mvt) {
                    fail("add_tile", &e);
                }
            }
        }
        if (idx + 1) % 32 == 0 {
            println!("bucket {}/{BUCKETS}, {} tuiles écrites", idx + 1, writer.tile_count());
        }
    }
    std::fs::remove_dir_all(&spill_dir).ok();

    let count = writer.tile_count();
    let out = match File::create(&output) {
        Ok(f) => f,
        Err(e) => fail("Création de l'archive", &e),
    };
    if let Err(e) = writer.finish(BufWriter::new(out), bbox) {
        fail("Écriture de l'archive", &e);
    }
    let size = std::fs::metadata(&output).map(|m| m.len()).unwrap_or(0);
    println!("{count} tuiles écrites → {output} ({:.1} Mo)", size as f64 / 1e6);
}

/// Inverse de `zxy_to_tileid` au zoom de l'archive (Hilbert d → xy).
fn tileid_to_xy(tile_id: u64) -> (u32, u32) {
    let d = tile_id - zxy_to_tileid(vtiles::ZOOM, 0, 0);
    let n: u64 = 1 << vtiles::ZOOM;
    let (mut x, mut y) = (0u64, 0u64);
    let mut t = d;
    let mut s = 1u64;
    while s < n {
        let rx = 1 & (t / 2);
        let ry = 1 & (t ^ rx);
        // Rotation inverse.
        if ry == 0 {
            if rx == 1 {
                x = s - 1 - x;
                y = s - 1 - y;
            }
            std::mem::swap(&mut x, &mut y);
        }
        x += s * rx;
        y += s * ry;
        t /= 4;
        s *= 2;
    }
    (x as u32, y as u32)
}

// --------------------------- sérialisation compacte des records de spill

fn encode_polygon(layer: u8, b: &Building) -> Vec<u8> {
    let mut out = Vec::with_capacity(64);
    out.push(layer);
    let id = b.osm_id.as_bytes();
    out.extend_from_slice(&(id.len() as u16).to_le_bytes());
    out.extend_from_slice(id);
    let name = b.name.as_deref().unwrap_or("").as_bytes();
    out.extend_from_slice(&(name.len() as u16).to_le_bytes());
    out.extend_from_slice(name);
    out.extend_from_slice(&b.height_m.to_le_bytes());
    out.push(b.height_from_osm as u8);
    out.push(match b.leaf_type {
        None => 255,
        Some(l) => l as u8,
    });
    out.extend_from_slice(&(b.rings.len() as u16).to_le_bytes());
    for ring in &b.rings {
        out.extend_from_slice(&(ring.len() as u32).to_le_bytes());
        for &(lat, lon) in ring {
            out.extend_from_slice(&lat.to_le_bytes());
            out.extend_from_slice(&lon.to_le_bytes());
        }
    }
    out
}

fn decode_polygon(r: &[u8]) -> Building {
    let mut p = 1usize;
    let take = |p: &mut usize, n: usize| -> &[u8] {
        let s = &r[*p..*p + n];
        *p += n;
        s
    };
    let id_len = u16::from_le_bytes(take(&mut p, 2).try_into().unwrap()) as usize;
    let osm_id = String::from_utf8_lossy(take(&mut p, id_len)).into_owned();
    let name_len = u16::from_le_bytes(take(&mut p, 2).try_into().unwrap()) as usize;
    let name = String::from_utf8_lossy(take(&mut p, name_len)).into_owned();
    let height_m = f32::from_le_bytes(take(&mut p, 4).try_into().unwrap());
    let height_from_osm = take(&mut p, 1)[0] != 0;
    let leaf_type = match take(&mut p, 1)[0] {
        255 => None,
        0 => Some(LeafType::Broadleaved),
        1 => Some(LeafType::Needleleaved),
        _ => Some(LeafType::Palm),
    };
    let ring_count = u16::from_le_bytes(take(&mut p, 2).try_into().unwrap()) as usize;
    let mut rings = Vec::with_capacity(ring_count);
    for _ in 0..ring_count {
        let n = u32::from_le_bytes(take(&mut p, 4).try_into().unwrap()) as usize;
        let mut ring = Vec::with_capacity(n);
        for _ in 0..n {
            let lat = f64::from_le_bytes(take(&mut p, 8).try_into().unwrap());
            let lon = f64::from_le_bytes(take(&mut p, 8).try_into().unwrap());
            ring.push((lat, lon));
        }
        rings.push(ring);
    }
    Building {
        osm_id,
        name: (!name.is_empty()).then_some(name),
        rings,
        height_m,
        height_from_osm,
        leaf_type,
    }
}

fn encode_tree(t: &Tree) -> Vec<u8> {
    let mut out = Vec::with_capacity(48);
    out.push(2u8);
    let id = t.osm_id.as_bytes();
    out.extend_from_slice(&(id.len() as u16).to_le_bytes());
    out.extend_from_slice(id);
    out.extend_from_slice(&t.lat.to_le_bytes());
    out.extend_from_slice(&t.lng.to_le_bytes());
    out.extend_from_slice(&t.height_m.to_le_bytes());
    out.extend_from_slice(&t.crown_radius_m.to_le_bytes());
    out.push(t.leaf_type as u8);
    out
}

fn decode_tree(r: &[u8]) -> Tree {
    let mut p = 1usize;
    let id_len = u16::from_le_bytes(r[p..p + 2].try_into().unwrap()) as usize;
    p += 2;
    let osm_id = String::from_utf8_lossy(&r[p..p + id_len]).into_owned();
    p += id_len;
    let lat = f64::from_le_bytes(r[p..p + 8].try_into().unwrap());
    p += 8;
    let lng = f64::from_le_bytes(r[p..p + 8].try_into().unwrap());
    p += 8;
    let height_m = f64::from_le_bytes(r[p..p + 8].try_into().unwrap());
    p += 8;
    let crown_radius_m = f64::from_le_bytes(r[p..p + 8].try_into().unwrap());
    p += 8;
    let leaf_type = match r[p] {
        1 => LeafType::Needleleaved,
        2 => LeafType::Palm,
        _ => LeafType::Broadleaved,
    };
    Tree {
        osm_id,
        lat,
        lng,
        height_m,
        crown_radius_m,
        leaf_type,
    }
}
