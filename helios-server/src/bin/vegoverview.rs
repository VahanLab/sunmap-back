//! Génère `sunmap-veg.pmtiles` — l'**aperçu de canopée** : la couche `woods`
//! seule, aux niveaux z12 et z13, dérivée de `sunmap.pmtiles`.
//!
//!   cargo run --release --bin vegoverview -- tiles/sunmap.pmtiles tiles/sunmap-veg.pmtiles
//!
//! ## Pourquoi
//!
//! L'archive principale n'a qu'un niveau (z14, ~1,7 km de côté). Le masque
//! d'ombre du client réclame la végétation de toute l'emprise visible, donc
//! le nombre de tuiles suit l'**aire** : une vue inclinée à z12 en demande
//! ~1 100, huit à la fois. C'est le coût réel des zooms lointains — pas le
//! shader, dont la grille est de taille fixe.
//!
//! Un niveau z12 ramène ces 1 100 requêtes à une douzaine.
//!
//! ## Ce qu'on garde, ce qu'on jette
//!
//! - `buildings` : jeté. Le masque de canopée ne s'en sert pas, et c'est
//!   71 % des octets d'une tuile parisienne (316 Ko sur 447 Ko mesurés).
//! - `trees` : jeté. Sous z14 la DSM du client tourne à 6-12 m/pixel — une
//!   couronne de 8 m y pèse un pixel ou moins, et les arbres isolés sont le
//!   gros du volume (128 Ko sur la même tuile). Ils reprennent la main à z14
//!   sur l'archive principale.
//! - `woods` : gardé tel quel. Regrouper les tuiles supprime au passage la
//!   duplication : un massif à cheval sur seize tuiles z14 y est écrit seize
//!   fois, une seule dans sa tuile z12.
//!
//! Dériver de l'archive plutôt que de l'extrait OSM n'est pas qu'une économie
//! de temps : c'est la garantie que l'aperçu porte **exactement** les emprises
//! que le serveur classe.
//!
//! ## Un seul passage de lecture
//!
//! Les identifiants PMTiles suivent la courbe de Hilbert, dont les
//! descendants d'une tuile forment un intervalle contigu : parcourir
//! l'archive par `tile_id` croissant, c'est parcourir les groupes z12 dans
//! l'ordre, et dans chacun les seize sous-groupes z13 dans l'ordre. On émet
//! donc les tuiles z12 au fil de l'eau, et les z13 dans un fichier de débord
//! déjà trié — que l'on rejoue ensuite, puisque `ArchiveWriter` exige des
//! identifiants croissants et que tout z12 précède tout z13.

use std::collections::HashMap;
use std::io::{BufReader, BufWriter, Read, Write};

use helios_server::osm::Building;
use helios_server::vtiles::{self, zxy_to_tileid, ArchiveWriter, VectorStore};

fn fail(msg: &str) -> ! {
    eprintln!("vegoverview : {msg}");
    std::process::exit(1);
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let [input, output] = &args[..] else {
        eprintln!("Usage : vegoverview <sunmap.pmtiles> <sortie.pmtiles>");
        std::process::exit(1);
    };

    let [coarse, fine] = vtiles::OVERVIEW_ZOOMS;
    if fine <= coarse {
        fail("OVERVIEW_ZOOMS doit aller du plus grossier au plus fin");
    }

    let store = match VectorStore::open(input) {
        Ok(s) => s,
        Err(e) => fail(&format!("ouverture de {input} : {e}")),
    };
    let src_zoom = store.zoom();
    if fine >= src_zoom {
        fail(&format!(
            "niveaux d'aperçu (z{coarse}/z{fine}) attendus sous le zoom de l'archive (z{src_zoom})"
        ));
    }

    let ids = store.tile_ids();
    let base = zxy_to_tileid(src_zoom, 0, 0);
    eprintln!(
        "=== aperçu de canopée : {} tuiles z{src_zoom} → niveaux z{coarse} et z{fine}",
        ids.len()
    );

    let mut writer = match ArchiveWriter::new() {
        Ok(w) => w,
        Err(e) => fail(&format!("création de l'archive : {e}")),
    };
    // Débord des tuiles fines, déjà dans l'ordre des identifiants (cf. entête).
    let spill_path = std::env::temp_dir().join(format!("sunmap-veg-fine-{}.bin", std::process::id()));
    let mut spill = BufWriter::new(match std::fs::File::create(&spill_path) {
        Ok(f) => f,
        Err(e) => fail(&format!("fichier de débord : {e}")),
    });

    // Groupe courant : les woods du groupe grossier, et ceux de chacun de ses
    // sous-groupes fins. Dédoublonnés par identifiant OSM — un massif à cheval
    // arrive une fois par tuile source qu'il touche.
    let mut coarse_key: Option<(u32, u32)> = None;
    let mut coarse_woods: HashMap<String, Building> = HashMap::new();
    let mut fine_key: Option<(u32, u32)> = None;
    let mut fine_woods: HashMap<String, Building> = HashMap::new();
    let (mut n_coarse, mut n_fine) = (0usize, 0usize);
    let mut done = 0usize;

    // Fermetures impossibles ici (elles emprunteraient tout) : les deux
    // « chasses d'eau » sont écrites en dur, appelées aux trois endroits utiles.
    macro_rules! flush_fine {
        () => {
            if let Some((fx, fy)) = fine_key.take() {
                let list: Vec<&Building> = fine_woods.values().collect();
                if let Some(mvt) = vtiles::encode_tile_at(fine, fx, fy, &[], &list, &[]) {
                    let id = zxy_to_tileid(fine, fx, fy);
                    if let Err(e) = spill
                        .write_all(&id.to_le_bytes())
                        .and_then(|_| spill.write_all(&(mvt.len() as u32).to_le_bytes()))
                        .and_then(|_| spill.write_all(&mvt))
                    {
                        fail(&format!("écriture du débord : {e}"));
                    }
                    n_fine += 1;
                }
                fine_woods.clear();
            }
        };
    }
    macro_rules! flush_coarse {
        () => {
            if let Some((cx, cy)) = coarse_key.take() {
                let list: Vec<&Building> = coarse_woods.values().collect();
                if let Some(mvt) = vtiles::encode_tile_at(coarse, cx, cy, &[], &list, &[]) {
                    if let Err(e) = writer.add_tile(zxy_to_tileid(coarse, cx, cy), &mvt) {
                        fail(&format!("ajout de la tuile z{coarse} {cx}/{cy} : {e}"));
                    }
                    n_coarse += 1;
                }
                coarse_woods.clear();
            }
        };
    }

    for id in ids {
        if id < base {
            fail("identifiant de tuile sous le zoom déclaré de l'archive");
        }
        let (x, y) = vtiles::tileid_to_zxy(id, src_zoom);
        let ck = (x >> (src_zoom - coarse), y >> (src_zoom - coarse));
        let fk = (x >> (src_zoom - fine), y >> (src_zoom - fine));

        if fine_key != Some(fk) {
            flush_fine!();
            fine_key = Some(fk);
        }
        if coarse_key != Some(ck) {
            flush_coarse!();
            coarse_key = Some(ck);
        }

        match store.tile_woods(x, y) {
            Ok(woods) => {
                for w in woods {
                    fine_woods.insert(w.osm_id.clone(), w.clone());
                    coarse_woods.insert(w.osm_id.clone(), w);
                }
            }
            Err(e) => fail(&format!("lecture de la tuile {x}/{y} : {e}")),
        }

        done += 1;
        if done % 20_000 == 0 {
            eprintln!("    {done} tuiles lues — {n_coarse} z{coarse}, {n_fine} z{fine}");
        }
    }
    flush_fine!();
    flush_coarse!();

    // Rejeu du débord : tout identifiant z13 est supérieur à tout z12 (le
    // préfixe de l'identifiant PMTiles compte les zooms précédents), et le
    // fichier est déjà trié — la relecture est linéaire.
    if let Err(e) = spill.flush() {
        fail(&format!("vidage du débord : {e}"));
    }
    drop(spill);
    let mut reader = BufReader::new(match std::fs::File::open(&spill_path) {
        Ok(f) => f,
        Err(e) => fail(&format!("relecture du débord : {e}")),
    });
    loop {
        let mut head = [0u8; 12];
        match reader.read_exact(&mut head) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => break,
            Err(e) => fail(&format!("relecture du débord : {e}")),
        }
        let id = u64::from_le_bytes(head[0..8].try_into().unwrap());
        let len = u32::from_le_bytes(head[8..12].try_into().unwrap()) as usize;
        let mut mvt = vec![0u8; len];
        if let Err(e) = reader.read_exact(&mut mvt) {
            fail(&format!("relecture du débord : {e}"));
        }
        if let Err(e) = writer.add_tile(id, &mvt) {
            fail(&format!("ajout d'une tuile z{fine} : {e}"));
        }
    }
    std::fs::remove_file(&spill_path).ok();

    let out = match std::fs::File::create(output) {
        Ok(f) => f,
        Err(e) => fail(&format!("création de {output} : {e}")),
    };
    let total = writer.tile_count();
    if let Err(e) = writer.finish_levels(
        BufWriter::new(out),
        store.bounds(),
        coarse,
        fine,
        vtiles::OVERVIEW_METADATA,
    ) {
        fail(&format!("écriture de {output} : {e}"));
    }
    let size = std::fs::metadata(output).map(|m| m.len()).unwrap_or(0);
    eprintln!(
        "=== {output} : {total} tuiles ({n_coarse} z{coarse} + {n_fine} z{fine}), {:.1} Mo",
        size as f64 / 1e6
    );
}
