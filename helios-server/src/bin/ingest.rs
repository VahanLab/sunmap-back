//! Remplit PostGIS avec la géométrie OSM d'une zone (Paris par défaut).
//!
//! Overpass refuse les très grandes emprises et sature aux heures de pointe :
//! la zone est donc découpée en tuiles traitées une par une, chaque tuile
//! réussie étant tracée dans `ingest_log`. Une interruption (504, coupure
//! réseau, Ctrl-C) se reprend en relançant la commande — les tuiles déjà
//! absorbées sont sautées.
//!
//!   cargo run --release --bin ingest              # Paris, les 3 couches
//!   cargo run --release --bin ingest -- buildings # une seule couche
//!   cargo run --release --bin ingest -- --force   # réingère tout
//!
//! Variables : `DATABASE_URL` (défaut `postgres://localhost/sunmap`),
//! `INGEST_BBOX` = "s,w,n,e" pour une autre zone que Paris.

use std::io::Write;
use std::time::Duration;

use helios_server::{db, osm};

/// Paris intra-muros + une marge d'environ 1,5 km.
///
/// La marge n'est pas cosmétique : un immeuble situé hors de la zone porte
/// quand même ombre à l'intérieur. Au soleil rasant (5°), 20 m de haut
/// projettent 230 m — la marge couvre largement les casters de bordure.
const PARIS_BBOX: (f64, f64, f64, f64) = (48.8000, 2.2100, 48.9150, 2.4800);

/// Découpage de la zone. 8×8 sur Paris ≈ 1,6 × 3,7 km par tuile, soit l'ordre
/// de grandeur qu'Overpass sert en 10-20 s sur du bâti dense.
const GRID: usize = 8;

/// Politesse entre deux requêtes Overpass : l'API publique est gratuite et
/// partagée, et l'enchaînement sans pause déclenche des 429.
const DELAY: Duration = Duration::from_secs(6);

/// Une tuile qui échoue a épuisé les trois miroirs — c'est presque toujours du
/// throttling passager, pas une requête invalide. On retente en laissant le
/// temps au quota de se reconstituer plutôt que de reporter la tuile à un
/// second passage.
const ATTEMPTS: u32 = 3;
const RETRY_BACKOFF: Duration = Duration::from_secs(30);

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let force = args.iter().any(|a| a == "--force");
    let wanted: Vec<&str> = {
        let explicit: Vec<&str> = args
            .iter()
            .filter(|a| !a.starts_with("--"))
            .map(|s| s.as_str())
            .collect();
        if explicit.is_empty() {
            vec!["buildings", "trees", "places"]
        } else {
            explicit
        }
    };

    let bbox = std::env::var("INGEST_BBOX")
        .ok()
        .and_then(|raw| {
            let v: Vec<f64> = raw.split(',').filter_map(|p| p.trim().parse().ok()).collect();
            match v[..] {
                [s, w, n, e] => Some((s, w, n, e)),
                _ => None,
            }
        })
        .unwrap_or(PARIS_BBOX);

    let pool = match db::connect().await {
        Ok(p) => p,
        Err(e) => {
            eprintln!("Connexion PostgreSQL impossible : {e}");
            eprintln!("Base attendue : {} (créer avec `createdb sunmap` puis \
                       `psql -d sunmap -f helios-server/schema.sql`)", db::DEFAULT_URL);
            std::process::exit(1);
        }
    };

    let http = reqwest::Client::builder()
        .user_agent("sunmap-helios/0.1 (+https://github.com/VahanLab/sunmap-back)")
        .timeout(Duration::from_secs(300))
        .build()
        .expect("client HTTP");

    let (s0, w0, n0, e0) = bbox;
    println!("Zone : {s0},{w0} → {n0},{e0}  ({GRID}×{GRID} tuiles)");

    for layer in wanted {
        let done = if force {
            Vec::new()
        } else {
            db::done_chunks(&pool, layer).await.unwrap_or_default()
        };
        let mut total = 0usize;
        let mut skipped = 0usize;
        let mut failed = Vec::new();

        for iy in 0..GRID {
            for ix in 0..GRID {
                let s = s0 + (n0 - s0) * iy as f64 / GRID as f64;
                let n = s0 + (n0 - s0) * (iy + 1) as f64 / GRID as f64;
                let w = w0 + (e0 - w0) * ix as f64 / GRID as f64;
                let e = w0 + (e0 - w0) * (ix + 1) as f64 / GRID as f64;
                let key = format!("{s:.5},{w:.5},{n:.5},{e:.5}");

                if done.contains(&key) {
                    skipped += 1;
                    continue;
                }

                let index = iy * GRID + ix + 1;
                print!("[{layer}] tuile {index}/{} … ", GRID * GRID);
                let _ = std::io::stdout().flush();

                let mut outcome = Err(String::new());
                for attempt in 1..=ATTEMPTS {
                    outcome = ingest_chunk(&pool, &http, layer, s, w, n, e).await;
                    if outcome.is_ok() || attempt == ATTEMPTS {
                        break;
                    }
                    print!("retry… ");
                    let _ = std::io::stdout().flush();
                    tokio::time::sleep(RETRY_BACKOFF * attempt).await;
                }

                match outcome {
                    Ok(count) => {
                        total += count;
                        let _ = db::mark_chunk(&pool, layer, &key, count as i32).await;
                        println!("{count}");
                    }
                    Err(e) => {
                        println!("ÉCHEC : {e}");
                        failed.push(key);
                    }
                }
                tokio::time::sleep(DELAY).await;
            }
        }

        println!(
            "[{layer}] terminé — {total} objets ingérés, {skipped} tuiles déjà faites, \
             {} en échec",
            failed.len()
        );
        if !failed.is_empty() {
            println!("  Relancer la commande pour reprendre uniquement ces tuiles.");
        }
    }

    for (table, label) in [
        ("buildings", "bâtiments"),
        ("trees", "arbres"),
        ("places", "établissements"),
    ] {
        let n: i64 = sqlx::query_scalar(&format!("SELECT count(*) FROM {table}"))
            .fetch_one(&pool)
            .await
            .unwrap_or(0);
        println!("{label:12} {n}");
    }
}

async fn ingest_chunk(
    pool: &sqlx::PgPool,
    http: &reqwest::Client,
    layer: &str,
    s: f64,
    w: f64,
    n: f64,
    e: f64,
) -> Result<usize, String> {
    match layer {
        "buildings" => {
            let items = osm::fetch_buildings(http, s, w, n, e).await?;
            db::upsert_buildings(pool, &items)
                .await
                .map_err(|e| e.to_string())?;
            Ok(items.len())
        }
        "trees" => {
            let items = osm::fetch_trees(http, s, w, n, e).await?;
            db::upsert_trees(pool, &items)
                .await
                .map_err(|e| e.to_string())?;
            Ok(items.len())
        }
        "places" => {
            let items = osm::fetch_places(http, s, w, n, e).await?;
            db::upsert_places(pool, &items)
                .await
                .map_err(|e| e.to_string())?;
            Ok(items.len())
        }
        other => Err(format!("couche inconnue : {other}")),
    }
}
