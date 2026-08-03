//! Génère `sunmap.pmtiles` — l'archive vectorielle unique — depuis un extrait
//! GeoJSONSeq, **sans base de données**.
//!
//!   ./scripts/osm-extract.sh idf.osm.pbf extrait.geojsonl
//!   cargo run --release --bin tilegen -- extrait.geojsonl tiles/sunmap.pmtiles
//!
//! Les règles tags → hauteur (dont la médiane locale des bâtiments non
//! taggés) sont celles de `pbf::read_geojsonseq` / `osm.rs` — les mêmes que
//! l'ancien chemin PostGIS, qui n'existe plus pour la géométrie : l'archive
//! EST la géométrie, PostgreSQL ne garde que les lieux et les contributions.
//!
//! Tout tient en mémoire (extrait + tuiles encodées) : ~2 Go de RAM pour
//! l'Île-de-France, quelques minutes. Pour un très grand territoire, générer
//! par sous-extraits régionaux et fusionner les archives (à outiller le jour
//! venu).

use std::collections::BTreeMap;
use std::fs::File;
use std::io::BufReader;

use helios_server::vtiles::{self, zxy_to_tileid};
use helios_server::{osm, pbf};

fn main() {
    let mut args = std::env::args().skip(1);
    let (Some(input), Some(output)) = (args.next(), args.next()) else {
        eprintln!("Usage : tilegen <extrait.geojsonl> <sortie.pmtiles>");
        std::process::exit(1);
    };

    let file = match File::open(&input) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("Impossible d'ouvrir {input} : {e}");
            std::process::exit(1);
        }
    };
    let extract = match pbf::read_geojsonseq(BufReader::with_capacity(1 << 20, file)) {
        Ok(e) => e,
        Err(e) => {
            eprintln!("Lecture de l'extrait : {e}");
            std::process::exit(1);
        }
    };

    // Répartition des objets par tuile : un objet à cheval est écrit entier
    // dans chaque tuile que sa boîte englobante touche (le lecteur
    // dédoublonne par id). Indices plutôt que clones : l'extrait est gros.
    #[derive(Default)]
    struct TileRefs {
        buildings: Vec<usize>,
        woods: Vec<usize>,
        trees: Vec<usize>,
    }
    let mut tiles: BTreeMap<u64, (u32, u32, TileRefs)> = BTreeMap::new();
    let mut bbox = (f64::MAX, f64::MAX, f64::MIN, f64::MIN); // s, w, n, e

    let mut spread = |rings: &[Vec<(f64, f64)>],
                      tiles: &mut BTreeMap<u64, (u32, u32, TileRefs)>,
                      select: &mut dyn FnMut(&mut TileRefs)| {
        let Some((x0, y0, x1, y1)) = vtiles::covered_tiles(rings) else {
            return;
        };
        for ring in rings {
            for &(lat, lon) in ring {
                bbox.0 = bbox.0.min(lat);
                bbox.1 = bbox.1.min(lon);
                bbox.2 = bbox.2.max(lat);
                bbox.3 = bbox.3.max(lon);
            }
        }
        for y in y0..=y1 {
            for x in x0..=x1 {
                let entry = tiles
                    .entry(zxy_to_tileid(vtiles::ZOOM, x, y))
                    .or_insert_with(|| (x, y, TileRefs::default()));
                select(&mut entry.2);
            }
        }
    };

    for (i, b) in extract.buildings.iter().enumerate() {
        spread(&b.rings, &mut tiles, &mut |t| t.buildings.push(i));
    }
    for (i, w) in extract.woods.iter().enumerate() {
        spread(&w.rings, &mut tiles, &mut |t| t.woods.push(i));
    }
    for (i, t) in extract.trees.iter().enumerate() {
        let point = vec![vec![(t.lat, t.lng)]];
        spread(&point, &mut tiles, &mut |refs| refs.trees.push(i));
    }

    println!(
        "{} tuiles z{} à encoder ({} bâtiments, {} bois, {} arbres)",
        tiles.len(),
        vtiles::ZOOM,
        extract.buildings.len(),
        extract.woods.len(),
        extract.trees.len()
    );

    let mut writer = vtiles::ArchiveWriter::new();
    let total = tiles.len();
    for (done, (tile_id, (x, y, refs))) in tiles.into_iter().enumerate() {
        let buildings: Vec<&osm::Building> =
            refs.buildings.iter().map(|&i| &extract.buildings[i]).collect();
        let woods: Vec<&osm::Building> = refs.woods.iter().map(|&i| &extract.woods[i]).collect();
        let trees: Vec<&osm::Tree> = refs.trees.iter().map(|&i| &extract.trees[i]).collect();
        if let Some(mvt) = vtiles::encode_tile(x, y, &buildings, &woods, &trees) {
            if let Err(e) = writer.add_tile(tile_id, &mvt) {
                eprintln!("tuile {x}/{y} : {e}");
                std::process::exit(1);
            }
        }
        if (done + 1) % 500 == 0 {
            println!("{}/{total} tuiles encodées", done + 1);
        }
    }

    let count = writer.tile_count();
    let out = match File::create(&output) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("Impossible de créer {output} : {e}");
            std::process::exit(1);
        }
    };
    if let Err(e) = writer.finish(std::io::BufWriter::new(out), bbox) {
        eprintln!("Écriture de l'archive : {e}");
        std::process::exit(1);
    }
    let size = std::fs::metadata(&output).map(|m| m.len()).unwrap_or(0);
    println!("{count} tuiles écrites → {output} ({:.1} Mo)", size as f64 / 1e6);
}
