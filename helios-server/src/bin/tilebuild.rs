//! Génère le fichier de tuiles bâtiments (`btiles`, format HBT1) depuis la
//! base PostGIS.
//!
//!   cargo run --release --bin tilebuild -- buildings.hbt
//!
//! Lire depuis la base plutôt que depuis un geojsonl : les hauteurs y sont
//! déjà résolues (tag OSM ou médiane locale calculée à l'import) — le tileset
//! hérite exactement des mêmes valeurs, ce qui rend la comparaison DB/tuiles
//! équitable. Pour un nouveau pays, on importe d'abord en base (pipeline
//! existant) puis on tuile ; la base peut ensuite être purgée de `buildings`.
//!
//! Tout tient en mémoire pendant la génération (~500 Mo pour l'Île-de-France,
//! cf. `btiles::TileWriter`) : pour un très grand pays, générer par
//! sous-extraits régionaux.

use std::io::Write;

use helios_server::{btiles, db};

#[tokio::main]
async fn main() {
    let out_path = match std::env::args().nth(1) {
        Some(p) => p,
        None => {
            eprintln!("Usage : tilebuild <sortie.hbt>");
            std::process::exit(1);
        }
    };

    let pool = match db::connect().await {
        Ok(p) => p,
        Err(e) => {
            eprintln!("Connexion PostgreSQL impossible : {e}");
            std::process::exit(1);
        }
    };

    let mut writer = btiles::TileWriter::new();
    let mut after = String::new();
    let mut total = 0u64;
    loop {
        let page = match db::buildings_page(&pool, &after, 50_000).await {
            Ok(p) => p,
            Err(e) => {
                eprintln!("Lecture de la page après {after:?} : {e}");
                std::process::exit(1);
            }
        };
        let Some(last) = page.last() else { break };
        after = last.osm_id.clone();
        for b in &page {
            writer.add(b);
        }
        total += page.len() as u64;
        print!("\r{total} bâtiments tuilés…");
        let _ = std::io::stdout().flush();
    }
    println!();

    let file = match std::fs::File::create(&out_path) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("Création de {out_path} : {e}");
            std::process::exit(1);
        }
    };
    if let Err(e) = writer.write_to(std::io::BufWriter::new(file)) {
        eprintln!("Écriture de {out_path} : {e}");
        std::process::exit(1);
    }

    let size = std::fs::metadata(&out_path).map(|m| m.len()).unwrap_or(0);
    println!(
        "{out_path} : {total} bâtiments, {} tuiles, {:.1} Mo",
        writer.tile_count(),
        size as f64 / 1_048_576.0
    );
}
