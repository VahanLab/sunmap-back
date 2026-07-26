//! Le socle Mapterhorn est-il un MNT (sol nu) ou un DSM (bâtiments inclus) ?
//!
//! La question n'est pas théorique : Mapterhorn utilise Copernicus GLO-30 comme
//! socle mondial, raffiné par des modèles LiDAR nationaux là où ils existent.
//! GLO-30 étant un DSM, les bâtiments y sont déjà. Or on stampe les emprises
//! OSM par-dessus — donc hors zones raffinées, on compterait les bâtiments
//! deux fois, et un point de rue hériterait de l'altitude du toit voisin.
//!
//! Protocole : comparer l'altitude au centre d'un grand bâtiment à celle d'un
//! point de sol dégagé à quelques centaines de mètres. Si l'écart approche la
//! hauteur du bâtiment, le socle est un DSM ; s'il reste proche de zéro, c'est
//! un MNT. Les cas les plus tranchants sont les tours isolées : un écart de
//! plusieurs centaines de mètres ne peut pas venir du relief.
//!
//!   cargo run --release --bin dem_probe

use std::collections::HashMap;

use helios_server::dem;
use tokio::sync::RwLock;

/// Un site de test : un point sur le bâtiment, un point de sol dégagé proche,
/// et la hauteur réelle du bâtiment pour interpréter l'écart.
struct Site {
    ville: &'static str,
    batiment: &'static str,
    /// Hauteur réelle, mètres.
    hauteur_m: f64,
    /// Point au centre du bâtiment.
    sur: (f64, f64),
    /// Point de sol dégagé à proximité (place, parc, plan d'eau, large avenue).
    sol: (f64, f64),
    /// Un LiDAR national raffine-t-il la zone ? Sert à interpréter : une
    /// réponse « MNT » n'est concluante pour le socle mondial que si NON.
    lidar_national: bool,
}

const SITES: &[Site] = &[
    // Tours isolées et très hautes : l'écart attendu est sans ambiguïté si le
    // socle est un DSM, et le relief local ne peut pas l'expliquer.
    Site {
        ville: "Dubaï",
        batiment: "Burj Khalifa",
        hauteur_m: 828.0,
        sur: (25.197197, 55.274376),
        sol: (25.191500, 55.281000),
        lidar_national: false,
    },
    Site {
        ville: "Le Caire",
        batiment: "Tour du Caire",
        hauteur_m: 187.0,
        sur: (30.045833, 31.224167),
        sol: (30.043000, 31.230000),
        lidar_national: false,
    },
    Site {
        ville: "Kuala Lumpur",
        batiment: "Tours Petronas",
        hauteur_m: 452.0,
        sur: (3.157900, 101.711600),
        sol: (3.153000, 101.716000),
        lidar_national: false,
    },
    Site {
        ville: "São Paulo",
        batiment: "Edifício Itália",
        hauteur_m: 168.0,
        sur: (-23.545600, -46.643900),
        sol: (-23.547500, -46.639000),
        lidar_national: false,
    },
    // Zones à LiDAR national : témoins. Si elles répondent « MNT » alors que
    // les précédentes répondent « DSM », le diagnostic est net.
    Site {
        ville: "Paris",
        batiment: "Tour Montparnasse",
        hauteur_m: 210.0,
        sur: (48.842200, 2.322000),
        sol: (48.846000, 2.321000),
        lidar_national: true,
    },
    Site {
        ville: "New York",
        batiment: "Empire State Building",
        hauteur_m: 381.0,
        sur: (40.748440, -73.985664),
        sol: (40.752000, -73.977000),
        lidar_national: true,
    },
];

#[tokio::main]
async fn main() {
    let http = reqwest::Client::builder()
        .user_agent("sunmap-helios/0.1 (+https://github.com/VahanLab/sunmap-back)")
        .timeout(std::time::Duration::from_secs(60))
        .build()
        .expect("client HTTP");
    let cache: dem::TileCache = RwLock::new(HashMap::new());

    println!(
        "Résolution des tuiles : z{} — {:.2} m/px à 48° de latitude\n",
        dem::ZOOM,
        dem::meters_per_pixel(48.0)
    );
    println!(
        "{:<14} {:<24} {:>8} {:>4} {:>9} {:>9} {:>8}  {:<7} verdict",
        "ville", "bâtiment", "hauteur", "zoom", "sur", "sol", "écart", "LiDAR"
    );

    for site in SITES {
        // La couverture Mapterhorn n'est pas uniforme : z15 n'existe pas
        // partout. On sonde le zoom réellement servi avant de comparer, sinon
        // on ne mesure que des 404.
        let Some(z) = dem::max_zoom_at(&http, &cache, site.sur.0, site.sur.1).await else {
            println!(
                "{:<14} {:<24} AUCUNE TUILE à aucun zoom",
                site.ville, site.batiment
            );
            continue;
        };

        let sur = dem::elevation_at_zoom(&http, &cache, site.sur.0, site.sur.1, z).await;
        let sol = dem::elevation_at_zoom(&http, &cache, site.sol.0, site.sol.1, z).await;

        let (sur, sol) = match (sur, sol) {
            (Ok(a), Ok(b)) => (a, b),
            (a, b) => {
                println!(
                    "{:<14} {:<24} ERREUR : {}",
                    site.ville,
                    site.batiment,
                    a.err().or(b.err()).unwrap_or_default()
                );
                continue;
            }
        };

        let ecart = (sur - sol) as f64;
        let ratio = ecart / site.hauteur_m;
        // Seuils larges : le relief réel entre les deux points ajoute du bruit,
        // et GLO-30 lisse un bâtiment sur ~30 m. On cherche un ordre de
        // grandeur, pas une mesure.
        let verdict = if ratio > 0.5 {
            "DSM — bâtiment présent dans le socle"
        } else if ratio > 0.15 {
            "AMBIGU — socle partiellement lissé, à creuser"
        } else {
            "MNT — sol nu, stamping OSM légitime"
        };

        println!(
            "{:<14} {:<24} {:>7.0}m {:>4} {:>8.1}m {:>8.1}m {:>7.1}m  {:<7} {verdict}",
            site.ville,
            site.batiment,
            site.hauteur_m,
            z,
            sur,
            sol,
            ecart,
            if site.lidar_national { "oui" } else { "non" },
        );
    }

    println!(
        "\nLecture : seuls les sites SANS LiDAR national renseignent sur le socle\n\
         mondial (Copernicus GLO-30). Les autres servent de témoins."
    );
}
