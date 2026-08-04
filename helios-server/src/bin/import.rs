//! Charge dans PostgreSQL les **lieux** d'un extrait OSM préparé par `osmium`
//! (GeoJSONSeq) : établissements et mobilier urbain. La géométrie
//! (bâtiments, arbres, bois) ne passe pas par ici — elle va directement dans
//! l'archive vectorielle, cf. `bin/tilegen`.
//!
//!   ./scripts/osm-extract.sh idf.osm.pbf extrait.geojsonl
//!   DATABASE_URL=postgres://localhost/sunmap \
//!     cargo run --release --bin import -- extrait.geojsonl
//!
//! La base visée est **annoncée avant toute écriture**, et il n'y a pas de
//! valeur par défaut : un import de la France entière est déjà parti dans la
//! base de dev en silence faute d'avoir exporté `DATABASE_URL`.
//!
//! Lit aussi sur l'entrée standard si aucun fichier n'est donné, ce qui permet
//! d'enchaîner sans fichier intermédiaire :
//!
//!   osmium export ... -f geojsonseq | cargo run --release --bin import

use std::fs::File;
use std::io::{BufReader, IsTerminal};

use helios_server::{db, pbf};

/// Même chargement que le serveur : sans lui, les binaires ne voyaient pas
/// `helios-server/.env` et chacun contournait à sa façon — c'est cette
/// asymétrie qui avait fini par mettre des identifiants de production dans un
/// fichier lu par le serveur de développement.
fn load_dotenv() {
    for candidate in ["helios-server/.env", ".env"] {
        if dotenvy::from_filename(candidate).is_ok() {
            return;
        }
    }
}

#[tokio::main]
async fn main() {
    load_dotenv();
    let path = std::env::args().nth(1);

    // Lue avant de parser l'extrait : inutile de passer plusieurs minutes sur
    // un fichier de plusieurs Go pour échouer sur la configuration.
    let url = match db::database_url() {
        Ok(u) => u,
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(1);
        }
    };
    println!(
        "cible : {} ({})",
        db::host_of(&url).unwrap_or_else(|| "?".into()),
        if db::is_local_url(&url) { "locale" } else { "DISTANTE" }
    );

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
