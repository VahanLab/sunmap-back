//! Charge dans PostGIS un extrait OSM préparé par `osmium` (GeoJSONSeq).
//!
//! Remplace l'ingestion Overpass par tuiles (`bin/ingest.rs`) dès qu'on veut
//! plus qu'une ville : 192 requêtes réseau et ~45 min pour Paris, contre un
//! téléchargement et quelques minutes ici. `ingest` reste utile pour rafraîchir
//! une zone restreinte sans re-télécharger un extrait.
//!
//!   ./scripts/osm-extract.sh idf.osm.pbf extrait.geojsonl
//!   cargo run --release --bin import -- extrait.geojsonl
//!
//! Lit aussi sur l'entrée standard si aucun fichier n'est donné, ce qui permet
//! d'enchaîner sans fichier intermédiaire :
//!
//!   osmium export ... -f geojsonseq | cargo run --release --bin import

use std::fs::File;
use std::io::{BufReader, IsTerminal};

use helios_server::{db, pbf};

#[tokio::main]
async fn main() {
    let path = std::env::args().nth(1);

    let extract = match &path {
        Some(p) => {
            let file = match File::open(p) {
                Ok(f) => f,
                Err(e) => {
                    eprintln!("Impossible d'ouvrir {p} : {e}");
                    std::process::exit(1);
                }
            };
            // Tampon large : le fichier fait des centaines de Mo en une seule
            // passe séquentielle.
            pbf::read_geojsonseq(BufReader::with_capacity(1 << 20, file))
        }
        None => {
            if std::io::stdin().is_terminal() {
                eprintln!("Usage : import <extrait.geojsonl>  (ou sur stdin)");
                std::process::exit(1);
            }
            pbf::read_geojsonseq(BufReader::with_capacity(1 << 20, std::io::stdin().lock()))
        }
    };

    let extract = match extract {
        Ok(e) => e,
        Err(e) => {
            eprintln!("Lecture de l'extrait : {e}");
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

    // Seuls les lieux (établissements + mobilier urbain) vont en base : la
    // géométrie (bâtiments, arbres, bois) part dans l'archive vectorielle via
    // `bin/tilegen`, sans passer par PostgreSQL. Upsert sur l'identifiant
    // OSM : réimporter un extrait plus récent met à jour en place.
    match db::upsert_places(&pool, &extract.places).await {
        Ok(n) => println!("établissements : {n} lignes écrites"),
        Err(e) => eprintln!("établissements : ÉCHEC : {e}"),
    }

    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM places")
        .fetch_one(&pool)
        .await
        .unwrap_or(-1);
    println!("base : {n} établissements");
}
