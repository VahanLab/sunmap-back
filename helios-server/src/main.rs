//! Serveur de query ensoleillement.
//!
//! - `GET /sunlit?lat=&lng=[&t=][&observer_height=]` : le point est-il au
//!   soleil à l'instant t ? (t = RFC3339 ou secondes Unix, défaut maintenant)
//! - `POST /sunlit/batch` : même question pour une liste de points
//!   (classification d'établissements).
//!
//! DSM : tuiles Mapterhorn (webp 512 px, encodage Terrarium — la même source
//! que l'app iOS), assemblées 3×3 autour du point pour donner de la marge aux
//! casters (ombres portées venant de l'extérieur de la tuile centrale).

use std::collections::HashMap;
use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::routing::{get, post, put};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;

use helios_core::dsm::Dsm;
use helios_core::shadow::{shadow_hit_from_ground, ShadowHit, ShadowParams};
use helios_core::sun::sun_position;
use helios_server::auth;
use helios_server::canopy_tiles;
use helios_server::db;
use helios_server::i18n::{self, Lang};
use helios_server::opening_hours;
use helios_server::dem::{self, latlon_of_world_px, world_px, TileCache, TILE_SIZE, ZOOM};
use helios_server::osm::Building;
use helios_server::tiers;
use helios_server::osm_api;
use helios_server::osm_push;
use helios_server::username;

/// Emprises déjà lues en base pour une bbox de tuiles donnée. PostGIS répond
/// en quelques ms, mais la même fenêtre est redemandée à chaque tick du slider
/// et le cache évite surtout de refaire le parsing WKT.
type BuildingCache = RwLock<HashMap<String, Arc<Vec<Building>>>>;
/// Résultat déjà calculé de `/places` (classification soleil/ombre), par
/// clé bbox+instant+hauteur d'observateur — évite de refaire tout le ray
/// marching quand la même requête (même minute, même zone) revient.
type PlacesResultCache = RwLock<HashMap<String, Arc<PlacesResponse>>>;
/// Réponses Nominatim déjà servies, par requête normalisée. La politique du
/// service public demande expressément de mettre les résultats en cache, et
/// une même saisie renvoie la même chose d'un jour à l'autre.
type GeocodeCache = RwLock<HashMap<String, (std::time::Instant, Arc<String>)>>;

/// Valeur de la grille `owner` pour « aucun bâtiment ici » (relief nu).
const OWNER_TERRAIN: u32 = u32::MAX;
/// Idem pour la végétation. Sentinelles distinctes du terrain : sans elles,
/// une ombre d'arbre serait attribuée au relief, et `describe_blocker` irait
/// chercher un bâtiment à un indice qui n'en désigne aucun.
///
/// Deux valeurs et non une : la nature de l'ombre renvoyée au client
/// distingue l'emprise boisée de l'arbre isolé (cf. `BlockerKind`).
const OWNER_CANOPY_WOOD: u32 = u32::MAX - 1;
const OWNER_CANOPY_TREE: u32 = u32::MAX - 2;

/// Un indice de bâtiment n'atteint jamais ces valeurs : tout ce qui est
/// au-dessus est une sentinelle, pas un objet.
const OWNER_FIRST_SENTINEL: u32 = OWNER_CANOPY_TREE;

struct AppState {
    auth: auth::FirebaseAuth,
    http: reqwest::Client,
    pool: sqlx::PgPool,
    tiles: TileCache,
    buildings: BuildingCache,
    places_results: PlacesResultCache,
    /// Archive vectorielle `sunmap.pmtiles` (`VECTOR_TILES=chemin.pmtiles`) :
    /// LA géométrie — bâtiments ET végétation. Obligatoire : les tables
    /// PostGIS correspondantes n'existent plus (cf. docs/tuiles-pmtiles.md),
    /// un serveur sans archive classerait tout au soleil.
    vstore: helios_server::vtiles::VectorStore,
    geocode_cache: GeocodeCache,
    /// Date du dernier appel sortant vers Nominatim : le verrou sérialise les
    /// requêtes amont et impose l'espacement d'une seconde de la politique —
    /// ce qu'une app dans chaque poche ne peut pas garantir, et qu'un proxy
    /// central garantit par construction.
    geocode_gate: tokio::sync::Mutex<Option<std::time::Instant>>,
}

/// Charge `helios-server/.env`, quel que soit l'endroit d'où l'on lance.
///
/// `dotenvy` remonte les dossiers parents à la recherche du chemin donné, mais
/// ne descend jamais dans un sous-dossier : d'où le chemin **relatif à la
/// racine de l'espace de travail**, qui se résout aussi bien depuis cette
/// racine (`cargo run`) que depuis `helios-server/` — dans ce dernier cas la
/// remontée d'un cran suffit à retrouver le même fichier. Vérifié dans les
/// deux sens.
///
/// `.env` nu en second : dépannage, et cohérence avec l'habitude.
///
/// Silencieux quand rien n'est trouvé : en production les variables viennent
/// du conteneur, pas d'un fichier.
fn load_dotenv() {
    for candidate in ["helios-server/.env", ".env"] {
        if dotenvy::from_filename(candidate).is_ok() {
            println!("configuration : {candidate}");
            return;
        }
    }
}

#[tokio::main]
async fn main() {
    load_dotenv();

    // Garde-fou : un binaire lancé à la main ne doit pas parler à une base
    // distante — a fortiori celle de production — parce qu'un `.env` traînait.
    // C'est arrivé : un `cargo run` local a tenté d'appliquer ses migrations
    // sur la base managée, et seul un refus de droits l'a arrêté. Or ces
    // migrations contiennent un `DROP TABLE`.
    //
    // En production, le conteneur pose `ALLOW_REMOTE_DB=1` — le geste est
    // délibéré et visible dans la configuration de déploiement.
    let url = match db::database_url() {
        Ok(u) => u,
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(1);
        }
    };
    let remote_allowed = std::env::var("ALLOW_REMOTE_DB").is_ok_and(|v| v == "1");
    if !db::is_local_url(&url) && !remote_allowed {
        eprintln!(
            "DATABASE_URL vise une base DISTANTE ({}), et ALLOW_REMOTE_DB n'est pas à 1.\n\
             Refus de démarrer : les migrations embarquées s'appliquent au démarrage,\n\
             et l'une d'elles supprime des tables. Pour du développement, viser une\n\
             base locale ; pour la production, poser ALLOW_REMOTE_DB=1.",
            db::host_of(&url).unwrap_or_else(|| "hôte illisible".into())
        );
        std::process::exit(1);
    }

    let pool = match db::connect().await {
        Ok(p) => p,
        Err(e) => {
            eprintln!("Connexion PostgreSQL impossible : {e}");
            eprintln!(
                "Base visée : {}. En dev : `createdb sunmap`, puis \
                 `DATABASE_URL=postgres://localhost/sunmap cargo run`.",
                db::host_of(&url).unwrap_or_else(|| "?".into())
            );
            std::process::exit(1);
        }
    };

    // Migrations embarquées dans le binaire (`sqlx migrate add` pour en créer
    // une). Appliquées à chaque démarrage : déjà passées = no-op, et un déploiement
    // ne peut plus oublier une évolution de schéma — c'était le risque du
    // `psql -f schema.sql` à la main.
    if let Err(e) = sqlx::migrate!("./migrations").run(&pool).await {
        eprintln!("Migrations SQL impossibles : {e}");
        std::process::exit(1);
    }

    // La base ne porte plus que le métier — la géométrie vit dans l'archive
    // vectorielle (VECTOR_TILES).
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM places")
        .fetch_one(&pool)
        .await
        .unwrap_or(-1);
    println!("base : {n} établissements");

    // Projet Firebase attendu comme destinataire des jetons. Surchargeable par
    // l'environnement pour pointer un projet de test sans recompiler.
    let project_id = std::env::var("FIREBASE_PROJECT_ID")
        .unwrap_or_else(|_| "sunmap-d06df".to_string());
    println!("authentification Firebase : projet {project_id}");

    let state = Arc::new(AppState {
        auth: auth::FirebaseAuth::new(project_id),
        // Ne sert plus qu'aux tuiles DEM Mapterhorn — la géométrie OSM vient
        // de PostGIS, plus d'Overpass au runtime.
        http: reqwest::Client::builder()
            .user_agent("sunmap-helios/0.1 (+https://github.com/VahanLab/sunmap-back)")
            .timeout(std::time::Duration::from_secs(20))
            .build()
            .expect("client HTTP"),
        pool,
        tiles: RwLock::new(HashMap::new()),
        buildings: RwLock::new(HashMap::new()),
        places_results: RwLock::new(HashMap::new()),
        geocode_cache: RwLock::new(HashMap::new()),
        geocode_gate: tokio::sync::Mutex::new(None),
        // `filter` : docker-compose passe la variable vide quand elle n'est
        // pas définie dans `.env` — vide vaut absente. Mourir plutôt que
        // démarrer sans géométrie : un serveur sans archive classerait tout
        // au soleil sans le dire.
        vstore: {
            let Some(path) = std::env::var("VECTOR_TILES").ok().filter(|p| !p.is_empty())
            else {
                eprintln!(
                    "VECTOR_TILES manquant : l'archive vectorielle est LA géométrie \
                     (générer avec `scripts/import-zone.sh`, cf. docs/import-zone.md)."
                );
                std::process::exit(1);
            };
            match helios_server::vtiles::VectorStore::open(&path) {
                Ok(store) => {
                    println!("géométrie : archive vectorielle {path} (z{})", store.zoom());
                    store
                }
                Err(e) => {
                    eprintln!("VECTOR_TILES={path} : {e}");
                    std::process::exit(1);
                }
            }
        },
    });

    // Reprise des envois OSM qui ont échoué — au démarrage, puis toutes les
    // cinq minutes. Sans elle, une panne d'OSM ou un redémarrage laisserait des
    // contributions en file pour toujours.
    if osm_push::is_configured() {
        println!(
            "liaison OpenStreetMap : active ({}{})",
            osm_api::api_base(),
            if osm_api::is_sandbox() { " — BAC À SABLE" } else { "" }
        );
        osm_push::spawn_retry_loop(state.pool.clone(), state.http.clone());
    } else {
        println!("liaison OpenStreetMap : inactive (OSM_CLIENT_ID absent)");
    }

    let app = Router::new()
        .route("/sunlit", get(sunlit))
        .route("/sunlit/batch", post(sunlit_batch))
        .route("/places", get(places))
        .route("/places/terrace", post(report_terrace))
        .route("/places/furniture", post(add_furniture).put(edit_furniture))
        .route("/places/furniture/contributions", get(furniture_contributions))
        .route("/places/terrace/contributions", get(terrace_contributions))
        .route("/users/me", get(current_user).delete(delete_current_user))
        .route("/users/me/profile", get(current_profile))
        .route("/users/me/contributions", get(current_contributions))
        .route(
            "/users/me/osm",
            get(osm_link_status).post(osm_link_account).delete(osm_unlink_account),
        )
        .route("/users/username", put(set_username))
        .route("/users/username/available", get(username_availability))
        .route("/users/username/suggestions", get(username_suggestions))
        // Après les routes littérales : sinon `/users/me` tomberait dans le
        // motif et on chercherait un compte au pseudo « me ».
        .route("/users/{username}/profile", get(user_profile))
        .route("/users/{username}/contributions", get(user_contributions))
        .route("/trees", get(trees))
        .route("/canopy/{z}/{x}/{y}", get(canopy_tile))
        .route("/sun-hours", get(sun_hours))
        .route("/geocode", get(geocode))
        .route("/debug/ray", get(debug_ray))
        .with_state(state);

    // Clé d'API globale (header `X-API-Key`) : filtre le scraping opportuniste
    // et les curl anonymes — pas une authentification (la clé embarquée dans
    // l'app se lit dans le binaire ; l'identité, c'est Firebase). En header et
    // pas en query : une query part dans les logs du proxy, de Cloudflare et
    // les caches d'URL. Absente de l'environnement = filtre éteint (dev local).
    let app = match std::env::var("API_TOKEN").ok().filter(|t| !t.is_empty()) {
        Some(token) => {
            println!("clé d'API : exigée (X-API-Key)");
            let token: Arc<str> = token.into();
            app.layer(axum::middleware::from_fn(move |req: axum::extract::Request, next: axum::middleware::Next| {
                let token = token.clone();
                async move {
                    let presented = req
                        .headers()
                        .get("x-api-key")
                        .and_then(|v| v.to_str().ok())
                        .unwrap_or("");
                    // Comparaison en temps constant : ne pas offrir un oracle
                    // de préfixe à qui mesure les temps de réponse.
                    let expected = token.as_bytes();
                    let given = presented.as_bytes();
                    let mut diff = expected.len() ^ given.len();
                    for i in 0..expected.len() {
                        diff |= (expected[i] ^ *given.get(i).unwrap_or(&0)) as usize;
                    }
                    if diff != 0 {
                        return Err(axum::http::StatusCode::UNAUTHORIZED);
                    }
                    Ok(next.run(req).await)
                }
            }))
        }
        None => {
            println!("clé d'API : absente (API_TOKEN vide) — endpoints ouverts");
            app
        }
    };

    // Surchargeable pour faire tourner deux instances côte à côte (comparer
    // un chemin de données à l'autre) sans toucher au serveur de dev.
    let addr = format!(
        "0.0.0.0:{}",
        std::env::var("PORT").ok().filter(|p| !p.is_empty()).unwrap_or_else(|| "8080".into())
    );
    println!("helios-server sur http://{addr}");
    let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
    axum::serve(listener, app).await.unwrap();
}

// ---------------------------------------------------------------- handlers

#[derive(Deserialize)]
struct SunlitQuery {
    lat: f64,
    lng: f64,
    /// RFC3339 ("2026-07-25T17:00:00Z") ou secondes Unix. Défaut : maintenant.
    t: Option<String>,
    /// Hauteur d'observateur en mètres (1.5 ≈ personne attablée). Défaut 0.
    observer_height: Option<f64>,
}

#[derive(Serialize)]
struct SunlitResponse {
    sunlit: bool,
    elevation_m: f32,
    sun_azimuth_deg: f64,
    sun_elevation_deg: f64,
    t_unix: f64,
    /// Absent si le point est au soleil (ou le soleil couché) : ce qui bloque.
    #[serde(skip_serializing_if = "Option::is_none")]
    blocker: Option<Blocker>,
    /// Nature DOMINANTE de l'ombre, arbitrée sur tout le rayon
    /// (`arbre` < `bois` < `batiment` < `relief`) — là où `blocker` décrit le
    /// PREMIER obstacle rencontré. Les deux peuvent différer : à l'ombre d'un
    /// arbre devant une falaise, `blocker` nomme l'arbre et celui-ci dit
    /// « relief ».
    #[serde(skip_serializing_if = "Option::is_none")]
    shadow_source: Option<BlockerKind>,
}

async fn sunlit(
    State(state): State<Arc<AppState>>,
    Query(q): Query<SunlitQuery>,
) -> Result<Json<SunlitResponse>, (StatusCode, String)> {
    let t = parse_time(q.t.as_deref())?;
    let resp = classify(&state, q.lat, q.lng, t, q.observer_height.unwrap_or(0.0)).await?;
    Ok(Json(resp))
}

#[derive(Deserialize)]
struct BatchRequest {
    points: Vec<Point>,
    t: Option<String>,
    observer_height: Option<f64>,
}

#[derive(Deserialize)]
struct Point {
    lat: f64,
    lng: f64,
}

async fn sunlit_batch(
    State(state): State<Arc<AppState>>,
    Json(req): Json<BatchRequest>,
) -> Result<Json<Vec<SunlitResponse>>, (StatusCode, String)> {
    let t = parse_time(req.t.as_deref())?;
    let h = req.observer_height.unwrap_or(0.0);
    let mut out = Vec::with_capacity(req.points.len());
    for p in &req.points {
        out.push(classify(&state, p.lat, p.lng, t, h).await?);
    }
    Ok(Json(out))
}

// ------------------------------------------------------------ cœur métier

/// Assemble une DSM couvrant l'intervalle de tuiles `[x0..=x1] × [y0..=y1]`.
/// Renvoie la grille + l'origine (coin nord-ouest) en pixels monde.
async fn assemble_grid(
    state: &AppState,
    x0: u32,
    y0: u32,
    x1: u32,
    y1: u32,
    mid_lat: f64,
) -> Result<(Dsm, f64, f64), (StatusCode, String)> {
    let nx = (x1 - x0 + 1) as usize;
    let ny = (y1 - y0 + 1) as usize;
    let width = nx * TILE_SIZE;
    let height = ny * TILE_SIZE;
    let mut data = vec![0f32; width * height];

    for tyi in y0..=y1 {
        for txi in x0..=x1 {
            let tile = fetch_tile(state, ZOOM, txi, tyi).await?;
            let ox = (txi - x0) as usize * TILE_SIZE;
            let oy = (tyi - y0) as usize * TILE_SIZE;
            for row in 0..TILE_SIZE {
                let src = row * TILE_SIZE;
                let dst = (oy + row) * width + ox;
                data[dst..dst + TILE_SIZE].copy_from_slice(&tile[src..src + TILE_SIZE]);
            }
        }
    }

    let meters_per_pixel =
        40_075_016.686 * mid_lat.to_radians().cos() / ((TILE_SIZE as f64) * f64::powi(2.0, ZOOM as i32));
    let dsm = Dsm {
        width,
        height,
        meters_per_pixel,
        data,
        canopy_top: None,
        canopy_base: None,
    };
    Ok((
        dsm,
        (x0 as f64) * TILE_SIZE as f64,
        (y0 as f64) * TILE_SIZE as f64,
    ))
}

/// Rasterise les bâtiments dans la DSM par vraie rasterisation polygone
/// (scanline, règle pair-impair). L'approximation bbox-rectangle testée
/// initialement s'est révélée fausser des points hors du bâtiment : un
/// bâtiment en L ou avec cour intérieure a une bbox qui déborde largement
/// sur le trottoir voisin, stampant à tort des terrasses qui n'y sont pas
/// (observé : terrasse à +20 m d'altitude alors qu'elle est au niveau rue,
/// simplement parce que son point tombait dans la bbox d'un immeuble en L).
///
/// `owner` (même dimensions que la DSM) reçoit l'index du bâtiment qui a fixé
/// l'altitude de chaque cellule — c'est ce qui permet de répondre « quel
/// immeuble fait cette ombre » après le ray marching.
/// `terrain` est le relief nu (la DSM AVANT tout stamping) : c'est lui qui
/// donne l'altitude du sol sous chaque bâtiment, cf. commentaire plus bas.
/// D'où vient le sol sur lequel on pose la hauteur d'un objet.
#[derive(Clone, Copy, PartialEq)]
enum GroundRef {
    /// Une seule altitude — celle du centre de l'emprise — pour tout le
    /// polygone. C'est ce qu'il faut aux **bâtiments** : les `building:part`
    /// et les membres d'une relation se recouvrent par construction, et
    /// relire le sol sous chaque cellule ferait empiler les hauteurs (on a
    /// observé un toit à 102 m pour un bâtiment de 25 m sur un sol à 35 m).
    /// Un immeuble n'a de toute façon pas de dénivelé notable sous lui.
    BboxCenter,
    /// Le relief sous **chaque cellule**. Indispensable aux emprises
    /// boisées : une forêt épouse la pente, là où un bâtiment est posé à
    /// plat. Avec une altitude unique, le haut d'une emprise de montagne se
    /// retrouve au-dessus de sa propre canopée — la condition d'écriture ne
    /// passe plus, aucune canopée n'est posée, et le point est classé au
    /// soleil en pleine forêt (cas réel : Forêt Domaniale de Vallorcine,
    /// sol à 1 694 m contre une canopée calée à 1 651 m).
    PerPixel,
}

fn stamp_buildings(
    dsm: &mut Dsm,
    terrain: &Dsm,
    owner: &mut [u32],
    origin_x: f64,
    origin_y: f64,
    buildings: &[Building],
    ground: GroundRef,
) {
    for (bi, b) in buildings.iter().enumerate() {
        // Tous les anneaux ensemble : extérieur + cours. La règle pair-impair
        // du scanline ci-dessous fait le reste — les traversées d'un anneau
        // intérieur re-basculent en « dehors », donc la cour reste creuse.
        let rings: Vec<Vec<(f64, f64)>> = b
            .rings
            .iter()
            .filter(|r| r.len() >= 3)
            .map(|r| {
                r.iter()
                    .map(|&(lat, lon)| {
                        let (wx, wy) = world_px(lat, lon);
                        (wx - origin_x, wy - origin_y)
                    })
                    .collect()
            })
            .collect();
        if rings.is_empty() {
            continue;
        }

        let pixels: Vec<(f64, f64)> = rings.concat();
        let min_y = pixels.iter().map(|p| p.1).fold(f64::MAX, f64::min);
        let max_y = pixels.iter().map(|p| p.1).fold(f64::MIN, f64::max);
        let min_x = pixels.iter().map(|p| p.0).fold(f64::MAX, f64::min);
        let max_x = pixels.iter().map(|p| p.0).fold(f64::MIN, f64::max);
        if max_x < 0.0 || max_y < 0.0 || min_x >= dsm.width as f64 || min_y >= dsm.height as f64 {
            continue; // entièrement hors grille
        }

        // Sol de référence, échantillonné sur le RELIEF SEUL et jamais sur la
        // DSM en cours de construction : sinon un objet posé sur une emprise
        // déjà stampée prendrait le toit du précédent pour sol et les hauteurs
        // s'additionneraient. Le choix entre altitude unique et sol par
        // cellule appartient à l'appelant (cf. `GroundRef`).
        let cx = ((min_x + max_x) / 2.0).clamp(0.0, dsm.width as f64 - 1.0);
        let cy = ((min_y + max_y) / 2.0).clamp(0.0, dsm.height as f64 - 1.0);
        let target_bbox = terrain.sample(cx, cy).unwrap_or(0.0) + b.height_m;

        let y0 = min_y.max(0.0).floor() as usize;
        let y1 = max_y.min(dsm.height as f64 - 1.0).ceil() as usize;
        for y in y0..=y1.min(dsm.height - 1) {
            let scan_y = y as f64 + 0.5;
            let mut xs: Vec<f64> = Vec::new();
            for ring in &rings {
                for i in 0..ring.len() {
                    let (x1, y1p) = ring[i];
                    let (x2, y2p) = ring[(i + 1) % ring.len()];
                    if (y1p <= scan_y && y2p > scan_y) || (y2p <= scan_y && y1p > scan_y) {
                        let t = (scan_y - y1p) / (y2p - y1p);
                        xs.push(x1 + t * (x2 - x1));
                    }
                }
            }
            xs.sort_by(|a, b| a.partial_cmp(b).unwrap());

            let mut i = 0;
            while i + 1 < xs.len() {
                let x_start = xs[i].round().max(0.0) as usize;
                let x_end = xs[i + 1].round().min(dsm.width as f64 - 1.0);
                if x_end >= 0.0 && x_end as usize >= x_start {
                    for x in x_start..=(x_end as usize).min(dsm.width - 1) {
                        let idx = y * dsm.width + x;
                        let target = match ground {
                            GroundRef::BboxCenter => target_bbox,
                            GroundRef::PerPixel => terrain.data[idx] + b.height_m,
                        };
                        if dsm.data[idx] < target {
                            dsm.data[idx] = target;
                            owner[idx] = bi as u32;
                        }
                    }
                }
                i += 2;
            }
        }
    }
}

/// Récupère les bâtiments couvrant l'emprise de la DSM et les rasterise
/// dedans. Bounds calculées depuis l'origine + la taille de la grille
/// (même étendue que les tuiles Mapterhorn assemblées).
async fn add_buildings(
    state: &AppState,
    dsm: &mut Dsm,
    origin_x: f64,
    origin_y: f64,
) -> Result<(Arc<Vec<Building>>, Vec<u32>), (StatusCode, String)> {
    let (north, west) = latlon_of_world_px(origin_x, origin_y);
    let (south, east) = latlon_of_world_px(origin_x + dsm.width as f64, origin_y + dsm.height as f64);
    let phase = std::time::Instant::now();
    let buildings = load_buildings(state, south, west, north, east).await?;
    let t_buildings_query_ms = phase.elapsed().as_secs_f64() * 1000.0;
    let mut owner = vec![OWNER_TERRAIN; dsm.width * dsm.height];
    let terrain = dsm.clone();
    let phase = std::time::Instant::now();
    stamp_buildings(dsm, &terrain, &mut owner, origin_x, origin_y, &buildings, GroundRef::BboxCenter);
    let t_stamp_buildings_ms = phase.elapsed().as_secs_f64() * 1000.0;

    // Végétation ensuite, jamais avant : là où arbre et bâtiment se recouvrent,
    // c'est le bâtiment qui doit rester le coupable désigné.
    //
    // Une lecture ratée n'interrompt pas la requête : mieux vaut une réponse
    // sans ombre de feuillage qu'une erreur, la végétation étant un raffinement
    // par-dessus le relief et le bâti.
    let phase = std::time::Instant::now();
    let (woods, trees) = load_canopy(state, south, west, north, east).unwrap_or_default();
    let t_canopy_query_ms = phase.elapsed().as_secs_f64() * 1000.0;
    let phase = std::time::Instant::now();
    if !woods.is_empty() || !trees.is_empty() {
        stamp_canopy(dsm, &terrain, &mut owner, origin_x, origin_y, &woods, &trees);
    }
    let t_stamp_canopy_ms = phase.elapsed().as_secs_f64() * 1000.0;
    println!("[places] bâtiments : requête {t_buildings_query_ms:.1} ms, stamp {t_stamp_buildings_ms:.1} ms \
              — canopée ({} bois, {} arbres) : requête {t_canopy_query_ms:.1} ms, stamp {t_stamp_canopy_ms:.1} ms",
             woods.len(), trees.len());
    Ok((buildings, owner))
}

/// Emprises de la zone, depuis PostGIS. Mémoïsé par bbox : celle-ci est
/// alignée sur les tuiles DEM, donc la même fenêtre revient à chaque tick du
/// slider — le cache évite surtout de refaire le parsing WKT, la requête
/// spatiale elle-même étant servie par l'index GIST en quelques ms.
async fn load_buildings(
    state: &AppState,
    s: f64,
    w: f64,
    n: f64,
    e: f64,
) -> Result<Arc<Vec<Building>>, (StatusCode, String)> {
    let key = format!("{s:.5},{w:.5},{n:.5},{e:.5}");
    if let Some(hit) = state.buildings.read().await.get(&key) {
        return Ok(hit.clone());
    }

    let buildings = state
        .vstore
        .buildings(s, w, n, e)
        .map_err(|err| (StatusCode::INTERNAL_SERVER_ERROR, format!("vtiles : {err}")))?;
    println!("[buildings] {key} → {} emprises", buildings.len());

    let arc = Arc::new(buildings);
    state.buildings.write().await.insert(key, arc.clone());
    Ok(arc)
}

/// Végétation de la zone, depuis l'archive vectorielle — les mêmes données
/// que le rendu client.
fn load_canopy(
    state: &AppState,
    s: f64,
    w: f64,
    n: f64,
    e: f64,
) -> Result<(Vec<Building>, Vec<helios_server::osm::Tree>), String> {
    let woods = state.vstore.woods(s, w, n, e).map_err(|err| format!("vtiles : {err}"))?;
    let trees = state.vstore.trees(s, w, n, e).map_err(|err| format!("vtiles : {err}"))?;
    Ok((woods, trees))
}

/// Ce qui bloque le soleil sur un point donné, tel que renvoyé aux clients.
#[derive(Serialize, Clone)]
struct Blocker {
    /// "way/123456" si c'est un bâtiment OSM, "terrain" si c'est le relief.
    id: String,
    /// Nom OSM du bâtiment s'il en a un.
    name: Option<String>,
    /// Hauteur retenue pour le bâtiment, et si elle vient d'un tag OSM ou du
    /// défaut (`DEFAULT_BUILDING_HEIGHT_M`) — un `false` ici explique la
    /// plupart des désaccords visuels avec la réalité.
    height_m: Option<f32>,
    height_from_osm: bool,
    /// Position (centre de la cellule DSM) et distance depuis le point testé.
    lat: f64,
    lng: f64,
    distance_m: f64,
    /// Altitude de l'obstacle vs altitude du rayon à cet endroit : l'écart dit
    /// de combien il manque au point pour voir le soleil.
    obstacle_elevation_m: f32,
    ray_elevation_m: f64,
}

/// Tamponne la végétation : emprises boisées puis arbres isolés.
///
/// Dans les couches **canopée** de la DSM, pas dans la grille opaque : depuis
/// la transmittance, un arbre n'est plus un mur — le ray marching traverse la
/// couronne en atténuant la lumière au lieu de s'arrêter (cf. `shadow.rs`).
/// Une terrasse sous ses platanes d'alignement revoit ainsi le soleil, une
/// futaie dense l'éteint toujours.
///
/// Traitée après les bâtiments : là où les deux se recouvrent — un arbre de
/// cour, un bois qui mord sur un hangar — c'est le bâtiment qui doit rester le
/// coupable désigné dans `owner`.
fn stamp_canopy(
    dsm: &mut Dsm,
    terrain: &Dsm,
    owner: &mut [u32],
    origin_x: f64,
    origin_y: f64,
    woods: &[Building],
    trees: &[helios_server::osm::Tree],
) {
    // Les bois passent par la même rasterisation scanline que les bâtiments —
    // même forme de donnée, mêmes anneaux intérieurs à garder creux (une
    // clairière est un anneau intérieur). Rasterisés dans une grille de
    // travail, puis reportés dans la couche canopée : `stamp_buildings` ne
    // sait écrire que dans une grille opaque.
    let mut wood_owner = vec![OWNER_TERRAIN; owner.len()];
    let mut scratch = terrain.clone();
    stamp_buildings(&mut scratch, terrain, &mut wood_owner, origin_x, origin_y, woods, GroundRef::PerPixel);

    let width = dsm.width;
    let (canopy_top, canopy_base) = dsm.canopy_layers_mut();

    // Bois : couronne du sol au sommet — un sous-bois n'a pas de base
    // dégagée, contrairement à un arbre d'alignement taillé pour le passage.
    for (i, &w) in wood_owner.iter().enumerate() {
        if w != OWNER_TERRAIN {
            canopy_top[i] = canopy_top[i].max(scratch.data[i]);
            canopy_base[i] = terrain.data[i];
        }
    }

    // Les arbres isolés : un disque de rayon de couronne, pas un rectangle. Un
    // rectangle surestimerait l'emprise de 27 % et ferait des ombres carrées.
    for t in trees {
        let (wx, wy) = world_px(t.lat, t.lng);
        let (cx, cy) = (wx - origin_x, wy - origin_y);
        let radius_px = (t.crown_radius_m / scratch.meters_per_pixel).max(0.5);
        let ground = match terrain.sample(
            cx.clamp(0.0, width as f64 - 1.0),
            cy.clamp(0.0, terrain.height as f64 - 1.0),
        ) {
            Some(g) => g,
            None => continue,
        };
        let top = ground + t.height_m as f32;
        // Base de la couronne : le houppier occupe le haut de l'arbre, le
        // tronc laisse passer dessous. Profondeur de couronne ≈ son diamètre,
        // bornée pour qu'un arbre trapu garde au moins un mètre de couronne —
        // sans jamais passer sous le sol (`max` avant `min`, pas `clamp` :
        // un arbuste d'un mètre rend les deux bornes inversées et `clamp`
        // panique).
        let base = (top - 2.0 * t.crown_radius_m as f32)
            .min(top - 1.0)
            .max(ground);

        let x0 = (cx - radius_px).floor().max(0.0) as usize;
        let x1 = (cx + radius_px).ceil().min(width as f64 - 1.0) as usize;
        let y0 = (cy - radius_px).floor().max(0.0) as usize;
        let y1 = (cy + radius_px).ceil().min(terrain.height as f64 - 1.0) as usize;
        if x1 < x0 || y1 < y0 {
            continue;
        }
        for y in y0..=y1 {
            for x in x0..=x1 {
                let dx = x as f64 + 0.5 - cx;
                let dy = y as f64 + 0.5 - cy;
                if dx * dx + dy * dy > radius_px * radius_px {
                    continue;
                }
                let i = y * width + x;
                if top > canopy_top[i] {
                    canopy_top[i] = top;
                    canopy_base[i] = if canopy_base[i].is_finite() {
                        canopy_base[i].min(base)
                    } else {
                        base
                    };
                    wood_owner[i] = OWNER_CANOPY_TREE;
                }
            }
        }
    }

    // Report final : la végétation ne réclame une case que si elle n'est pas
    // déjà revendiquée par un bâtiment. `wood_owner` porte l'indice du bois
    // (rasterisé comme une emprise) ou la sentinelle « arbre » ; on garde la
    // distinction, seule la nature nous intéresse en aval.
    for (i, &w) in wood_owner.iter().enumerate() {
        if w != OWNER_TERRAIN && owner[i] == OWNER_TERRAIN {
            owner[i] = if w == OWNER_CANOPY_TREE {
                OWNER_CANOPY_TREE
            } else {
                OWNER_CANOPY_WOOD
            };
        }
    }
}

fn describe_blocker(
    hit: &ShadowHit,
    dsm: &Dsm,
    owner: &[u32],
    buildings: &[Building],
    origin_x: f64,
    origin_y: f64,
) -> Blocker {
    let (lat, lng) = latlon_of_world_px(origin_x + hit.x as f64 + 0.5, origin_y + hit.y as f64 + 0.5);
    let owner_at = owner.get(hit.y * dsm.width + hit.x).copied();
    let is_canopy = matches!(owner_at, Some(OWNER_CANOPY_WOOD) | Some(OWNER_CANOPY_TREE));
    let b = owner_at
        .filter(|&o| o < OWNER_FIRST_SENTINEL)
        .and_then(|o| buildings.get(o as usize));

    Blocker {
        id: b.map_or_else(
            || {
                if is_canopy { "vegetation".to_string() } else { "terrain".to_string() }
            },
            |b| b.osm_id.clone(),
        ),
        name: b.and_then(|b| b.name.clone()),
        height_m: b.map(|b| b.height_m),
        height_from_osm: b.is_some_and(|b| b.height_from_osm),
        lat,
        lng,
        distance_m: hit.distance_m,
        obstacle_elevation_m: hit.obstacle_elevation_m,
        ray_elevation_m: hit.ray_elevation_m,
    }
}

/// Assemble la DSM 3×3 tuiles autour d'un point + bâtiments, et renvoie tout
/// ce qu'il faut pour classer ce point à n'importe quel instant : la DSM
/// stampée (obstacles), ses coordonnées pixel locales, et son altitude de
/// sol sur le relief SEUL (avant bâtiments — cf. `is_shadowed_from_ground` :
/// un POI dont les coordonnées OSM tombent par erreur à l'intérieur d'un
/// immeuble ne doit pas hériter de l'altitude du toit).
async fn assemble_point(
    state: &AppState,
    lat: f64,
    lng: f64,
) -> Result<PointCtx, (StatusCode, String)> {
    if !(-85.0..=85.0).contains(&lat) || !(-180.0..=180.0).contains(&lng) {
        return Err((StatusCode::BAD_REQUEST, "lat/lng hors bornes".into()));
    }

    // Tuile centrale + marge d'une tuile ≈ 1,2 km de casters à z15.
    let (wx, wy) = world_px(lat, lng);
    let tx = (wx / TILE_SIZE as f64) as u32;
    let ty = (wy / TILE_SIZE as f64) as u32;
    let max_tile = (1u32 << ZOOM) - 1;
    let (mut dsm, origin_x, origin_y) = assemble_grid(
        state,
        tx.saturating_sub(1),
        ty.saturating_sub(1),
        (tx + 1).min(max_tile),
        (ty + 1).min(max_tile),
        lat,
    )
    .await?;

    let px = wx - origin_x;
    let py = wy - origin_y;
    let ground = dsm.sample(px, py).unwrap_or(0.0);
    let (buildings, owner) = add_buildings(state, &mut dsm, origin_x, origin_y).await?;
    // Calculé une fois ici plutôt que dans `classify_at` : la grille ne change
    // plus après `add_buildings`, mais `classify_at` peut être appelée des
    // centaines de fois sur le même contexte (`/sun-hours` : un par tranche de
    // 5 min de la journée) — un scan complet de la DSM à chaque fois y aurait
    // le même coût que le bug corrigé sur `/places`.
    let max_elevation = dsm.max_elevation();

    Ok(PointCtx {
        dsm,
        owner,
        buildings,
        origin_x,
        origin_y,
        px,
        py,
        ground,
        max_elevation,
    })
}

/// Tout ce qu'il faut pour classer un point à n'importe quel instant, plus de
/// quoi nommer l'obstacle rencontré.
struct PointCtx {
    dsm: Dsm,
    owner: Vec<u32>,
    buildings: Arc<Vec<Building>>,
    origin_x: f64,
    origin_y: f64,
    px: f64,
    py: f64,
    /// Altitude du relief SEUL sous le point (avant stamping des bâtiments).
    ground: f32,
    /// Point le plus haut de toute la grille — voir le commentaire de
    /// `assemble_point`.
    max_elevation: f32,
}

/// Nature de ce qui ombre un point, sans autre détail.
///
/// L'ordre des variantes **est** l'ordre de priorité, du plus anodin au plus
/// couvrant : `Ord` dérivé, `max()` arbitre. Un point à l'ombre d'un arbre et
/// d'un bâtiment est renvoyé « batiment » ; sous un arbre dans une vallée
/// déjà à l'ombre du versant, « relief ».
///
/// Volontairement sans identité (ni `osm_id`, ni nom) : c'est la nature de
/// l'ombre qui se lit dans l'app, pas le coupable — que `Blocker` porte déjà
/// pour les bâtiments.
#[derive(Serialize, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
#[serde(rename_all = "lowercase")]
enum BlockerKind {
    Arbre,
    Bois,
    Batiment,
    Relief,
}

impl PointCtx {
    /// Nature dominante de l'ombre à cet instant, `None` si le point est au
    /// soleil (ou la nuit).
    ///
    /// Parcourt le rayon en entier — contrairement à `classify_at`, qui
    /// s'arrête au premier obstacle. À n'appeler qu'une fois par requête :
    /// c'est le prix à payer pour voir la crête derrière le mur.
    fn shadow_source_at(
        &self,
        sun: &helios_core::sun::SunPosition,
        params: &ShadowParams,
    ) -> Option<BlockerKind> {
        if !sun.is_up() {
            return None;
        }
        let width = self.dsm.width;
        // Deux booléens par nature plutôt qu'un ensemble : la canopée n'est
        // retenue que si elle a bel et bien éteint le soleil, or on ne le sait
        // qu'à la fin du parcours.
        let (mut tree, mut wood, mut building, mut terrain) = (false, false, false, false);

        let causes = helios_core::shadow::shadow_causes_from_ground(
            &self.dsm,
            sun,
            self.px,
            self.py,
            self.ground,
            params,
            self.max_elevation,
            |cause, x, y| {
                let at = |ix: usize, iy: usize| {
                    self.owner.get(iy * width + ix).copied().unwrap_or(OWNER_TERRAIN)
                };
                match cause {
                    helios_core::shadow::Cause::Opaque => {
                        // Les QUATRE cellules que `sample` interpole, pas la
                        // seule cellule arrondie : au bord d'un toit, la
                        // hauteur interpolée dépasse le rayon alors que la
                        // cellule la plus proche est encore du terrain nu —
                        // l'ombre du bâtiment se retrouvait attribuée au
                        // relief, jusqu'en plein Paris.
                        let x0 = (x.floor().max(0.0) as usize).min(width - 1);
                        let y0 = (y.floor().max(0.0) as usize).min(self.dsm.height - 1);
                        let x1 = (x0 + 1).min(width - 1);
                        let y1 = (y0 + 1).min(self.dsm.height - 1);
                        if [at(x0, y0), at(x1, y0), at(x0, y1), at(x1, y1)]
                            .iter()
                            .any(|&o| o < OWNER_FIRST_SENTINEL)
                        {
                            building = true;
                        } else {
                            terrain = true;
                        }
                    }
                    helios_core::shadow::Cause::Canopy => {
                        // `canopy_at` lit une seule cellule arrondie : on la
                        // consulte à l'identique.
                        let ix = (x.round().max(0.0) as usize).min(width - 1);
                        let iy = (y.round().max(0.0) as usize).min(self.dsm.height - 1);
                        match at(ix, iy) {
                            OWNER_CANOPY_TREE => tree = true,
                            // Une cellule de canopée déjà revendiquée par un
                            // bâtiment perd sa nature : comptée en bois, la
                            // plus couvrante des deux. Cas marginal.
                            _ => wood = true,
                        }
                    }
                }
            },
        );

        let mut kinds = Vec::new();
        if causes.canopy_extinguished {
            if tree {
                kinds.push(BlockerKind::Arbre);
            }
            if wood {
                kinds.push(BlockerKind::Bois);
            }
        }
        if causes.opaque_blocked {
            if building {
                kinds.push(BlockerKind::Batiment);
            }
            if terrain {
                kinds.push(BlockerKind::Relief);
            }
        }
        kinds.into_iter().max()
    }

    /// Le point est-il à l'ombre à cet instant, et si oui à cause de quoi ?
    fn classify_at(&self, sun: &helios_core::sun::SunPosition, params: &ShadowParams) -> (bool, Option<Blocker>) {
        if !sun.is_up() {
            return (false, None);
        }
        match shadow_hit_from_ground(&self.dsm, sun, self.px, self.py, self.ground, params,
                                     self.max_elevation) {
            None => (true, None),
            Some(hit) => (
                false,
                Some(describe_blocker(
                    &hit,
                    &self.dsm,
                    &self.owner,
                    &self.buildings,
                    self.origin_x,
                    self.origin_y,
                )),
            ),
        }
    }
}

async fn classify(
    state: &AppState,
    lat: f64,
    lng: f64,
    t_unix: f64,
    observer_height_m: f64,
) -> Result<SunlitResponse, (StatusCode, String)> {
    let sun = sun_position(t_unix, lat, lng);
    let ctx = assemble_point(state, lat, lng).await?;

    let params = ShadowParams {
        max_distance_m: 5_000.0, // relief : ombres longues possibles
        observer_height_m,
        step_px: 1.0,
        ..ShadowParams::default()
    };
    let (sunlit, blocker) = ctx.classify_at(&sun, &params);
    // Un seul parcours complet du rayon, et seulement si le point est ombré.
    let shadow_source = (!sunlit).then(|| ctx.shadow_source_at(&sun, &params)).flatten();

    Ok(SunlitResponse {
        sunlit,
        elevation_m: ctx.ground,
        sun_azimuth_deg: sun.azimuth_deg,
        sun_elevation_deg: sun.elevation_deg,
        t_unix,
        blocker,
        shadow_source,
    })
}

// ------------------------------------------------------------ sun-hours

#[derive(Deserialize)]
struct SunHoursQuery {
    lat: f64,
    lng: f64,
    /// N'importe quel instant DANS la journée voulue (RFC3339 ou secondes
    /// Unix). La journée est le jour calendaire UTC contenant `t`. Défaut :
    /// maintenant.
    t: Option<String>,
    /// Défaut 1,5 m : usage principal = "est-ce que je peux m'asseoir là".
    observer_height: Option<f64>,
    /// Décalage horaire du LIEU par rapport à UTC, en minutes (Paris en été :
    /// 120). Détermine où sont les bornes de la journée renvoyée. Défaut 0,
    /// soit la journée UTC — rarement ce qu'on veut, le client doit
    /// l'envoyer.
    utc_offset_minutes: Option<i32>,
}

#[derive(Serialize)]
struct SunHoursResponse {
    lat: f64,
    lng: f64,
    elevation_m: f32,
    /// Instant demandé et sa classification, pour un statut "maintenant"
    /// immédiat sans avoir à chercher dans `intervals`.
    t_unix: f64,
    state_now: SunState,
    /// Ce qui bloque le soleil à `t_unix` (absent si au soleil).
    #[serde(skip_serializing_if = "Option::is_none")]
    blocker_now: Option<Blocker>,
    /// Nature dominante de cette ombre (cf. `SunlitResponse::shadow_source`).
    #[serde(skip_serializing_if = "Option::is_none")]
    shadow_source_now: Option<BlockerKind>,
    day_start_unix: f64,
    day_end_unix: f64,
    /// Décalage effectivement utilisé pour découper la journée — permet au
    /// client de vérifier que le serveur a bien compris son fuseau.
    utc_offset_minutes: i32,
    total_sunlit_minutes: u32,
    /// Ombre portée de jour uniquement. La nuit est comptée à part : un point
    /// « à l'ombre 16 h » alors qu'il fait nuit 10 h de ces 16 h ne dit rien
    /// d'utile sur la qualité de l'endroit.
    total_shadow_minutes: u32,
    total_night_minutes: u32,
    intervals: Vec<SunInterval>,
}

/// État d'un point vis-à-vis du soleil.
///
/// Trois cas et non deux : confondre l'ombre portée et la nuit rendait les
/// cumuls trompeurs, et la frise de la journée illisible.
#[derive(Serialize, Clone, Copy, PartialEq, Eq, Debug)]
#[serde(rename_all = "lowercase")]
enum SunState {
    Sunlit,
    /// Soleil levé, mais un obstacle le masque.
    Shadow,
    /// Soleil sous l'horizon.
    Night,
}

#[derive(Serialize)]
struct SunInterval {
    start_unix: f64,
    end_unix: f64,
    state: SunState,
}

/// Journée calendaire **locale** (00:00 → 24:00) contenant l'instant donné,
/// pour un décalage donné en minutes par rapport à UTC.
///
/// Découper sur la journée UTC était faux dès qu'on sort du méridien de
/// Greenwich : à Paris en été (UTC+2), un slider réglé sur 00h30 renvoyait la
/// journée de la veille, et la timeline s'affichait bornée à 02:00 → 02:00 au
/// lieu de minuit à minuit.
/// Découpage de `sun_day` : 144 tranches de 10 min couvrent la journée.
/// 10 min et non le pas de 5 min du slider : deux fois moins de rayons pour
/// une précision largement suffisante — une ombre qui balaie une terrasse ne
/// se joue pas à 5 min près.
const SUN_DAY_STEP_S: f64 = 600.0;
/// Distance au-delà de laquelle un dégagement n'apporte plus rien : le retrait
/// de caméra le plus grand qu'on pratique est de ~220 m, chercher plus loin ne
/// changerait aucun cadrage et allongerait le lancer de rayons pour rien.
const VIEW_MAX_DISTANCE_M: f64 = 250.0;

const SUN_DAY_SLOTS: usize = (86_400.0 / SUN_DAY_STEP_S) as usize;

fn day_bounds_local(t_unix: f64, utc_offset_minutes: i32) -> (f64, f64) {
    let offset = utc_offset_minutes as f64 * 60.0;
    // Passer en heure locale, tronquer au jour, revenir en UTC.
    let local = t_unix + offset;
    let start_local = (local / 86_400.0).floor() * 86_400.0;
    let start_unix = start_local - offset;
    (start_unix, start_unix + 86_400.0)
}

/// Un point, une journée : les heures au soleil et à l'ombre. Échantillonne
/// toutes les 5 min (pas du slider iOS) et regroupe en intervalles
/// contigus — plus léger à consommer côté client qu'une valeur par tick.
async fn sun_hours(
    State(state): State<Arc<AppState>>,
    Query(q): Query<SunHoursQuery>,
) -> Result<Json<SunHoursResponse>, (StatusCode, String)> {
    let t = parse_time(q.t.as_deref())?;
    let h = q.observer_height.unwrap_or(1.5);
    let utc_offset_minutes = q.utc_offset_minutes.unwrap_or(0);
    let (day_start, day_end) = day_bounds_local(t, utc_offset_minutes);

    let ctx = assemble_point(&state, q.lat, q.lng).await?;
    let params = ShadowParams {
        max_distance_m: 5_000.0,
        observer_height_m: h,
        step_px: 1.0,
        ..ShadowParams::default()
    };

    const STEP_S: f64 = 300.0; // 5 min
    let steps = (86_400.0 / STEP_S) as usize;

    let mut intervals: Vec<SunInterval> = Vec::new();
    let mut state_now = SunState::Night;
    let mut blocker_now = None;
    let mut shadow_source_now = None;
    let (mut sunlit_steps, mut shadow_steps, mut night_steps) = (0u32, 0u32, 0u32);

    for i in 0..steps {
        let step_t = day_start + i as f64 * STEP_S;
        let sun = sun_position(step_t, q.lat, q.lng);
        let (sunlit, blocker) = ctx.classify_at(&sun, &params);
        let state = if !sun.is_up() {
            SunState::Night
        } else if sunlit {
            SunState::Sunlit
        } else {
            SunState::Shadow
        };
        match state {
            SunState::Sunlit => sunlit_steps += 1,
            SunState::Shadow => shadow_steps += 1,
            SunState::Night => night_steps += 1,
        }
        if step_t <= t && t < step_t + STEP_S {
            state_now = state;
            blocker_now = blocker;
            // Hors de la boucle de classification : un parcours complet par
            // tranche de 5 min coûterait 288 fois ce qu'il faut.
            if state == SunState::Shadow {
                shadow_source_now = ctx.shadow_source_at(&sun, &params);
            }
        }

        match intervals.last_mut() {
            Some(last) if last.state == state => last.end_unix = step_t + STEP_S,
            _ => intervals.push(SunInterval {
                start_unix: step_t,
                end_unix: step_t + STEP_S,
                state,
            }),
        }
    }

    let total_sunlit_minutes = sunlit_steps * 5;
    let total_shadow_minutes = shadow_steps * 5;
    let total_night_minutes = night_steps * 5;

    Ok(Json(SunHoursResponse {
        lat: q.lat,
        lng: q.lng,
        elevation_m: ctx.ground,
        t_unix: t,
        state_now,
        blocker_now,
        shadow_source_now,
        day_start_unix: day_start,
        day_end_unix: day_end,
        utc_offset_minutes,
        total_sunlit_minutes,
        total_shadow_minutes,
        total_night_minutes,
        intervals,
    }))
}

/// Déplace un point tombé *dans* une emprise bâtie vers le sol libre le plus
/// proche (trottoir), et renvoie `(px, py, distance parcourue en mètres)`.
///
/// Nécessaire parce que les POI OSM d'un bar ou d'un restaurant sont posés sur
/// le bâtiment, pas sur sa terrasse : le nœud tombe à l'intérieur du polygone
/// dans la grande majorité des cas. Le rayon solaire percute alors le mur du
/// bâtiment hôte au tout premier pas et *tout* est classé à l'ombre — observé :
/// 410 terrasses sur 416 bloquées à 1,57 m, soit exactement une cellule.
///
/// Sortir le point est plus juste que d'exclure le bâtiment hôte du test :
/// depuis le trottoir, sa façade continue de porter ombre le soir, ce qui est
/// bien le comportement attendu.
///
/// Recherche en anneaux croissants sur la grille de propriétaires, plus une
/// cellule de marge pour ne pas rester collé à la façade. Renvoie le point
/// d'origine si rien de libre dans le rayon (POI au cœur d'un grand bâtiment).
/// Cap de caméra le plus dégagé pour regarder un point, et la distance libre
/// qu'il offre derrière l'objectif.
///
/// ## Le problème que ça résout
///
/// Mapbox place la caméra **en retrait** du point visé, d'une distance au sol
/// de `altitude × tan(pitch)` — à z19.5 et 51° d'inclinaison, environ 220 m.
/// Viser une terrasse « de face », depuis la rue, met donc l'objectif 220 m
/// dans le pâté de maisons d'en face : dans une rue étroite, l'immeuble d'en
/// face masque la terrasse. Baisser l'inclinaison ou zoomer ne suffit pas, le
/// retrait reste toujours grand devant la largeur d'une rue.
///
/// La sortie est de regarder **le long** de la rue plutôt qu'à travers : dans
/// son axe il y a 50 à 200 m de vide, exactement ce que le retrait réclame.
///
/// ## Ce que la fonction rend
///
/// Le **cap Mapbox** (direction que regarde la caméra), pas la direction
/// dégagée : la caméra se tient à l'opposé de son cap, donc le vide doit être
/// derrière elle. Les deux sont à 180° l'un de l'autre, et les confondre
/// pointerait l'objectif pile sur le mur qu'on cherchait à éviter.
///
/// - Parameter preferred_bearing_deg: cap idéal — celui qui met la façade de
///   l'établissement en toile de fond. Départage les directions à peu près
///   aussi dégagées, pour ne pas perdre ce cadrage quand la place le permet.
fn open_view_bearing(
    dsm: &Dsm,
    owner: &[u32],
    px: f64,
    py: f64,
    preferred_bearing_deg: f64,
    max_distance_m: f64,
) -> (f64, f64) {
    /// Un tour complet par pas de 10° : plus fin ne changerait rien, une rue
    /// se voit largement à cette résolution.
    const STEPS: usize = 36;

    let occupied = |x: f64, y: f64| -> bool {
        let (ix, iy) = (x.round() as i32, y.round() as i32);
        if ix < 0 || iy < 0 || ix >= dsm.width as i32 || iy >= dsm.height as i32 {
            // Hors grille : on ne sait pas, donc on arrête de compter — mieux
            // vaut sous-estimer le dégagement que promettre du vide inconnu.
            return true;
        }
        let o = owner[iy as usize * dsm.width + ix as usize];
        o < OWNER_FIRST_SENTINEL
    };

    let max_steps = (max_distance_m / dsm.meters_per_pixel).max(1.0) as i32;
    let mut measured: Vec<(f64, f64)> = Vec::with_capacity(STEPS);

    for i in 0..STEPS {
        let bearing = i as f64 * (360.0 / STEPS as f64);
        // Repère raster : x vers l'est, y vers le sud, azimut horaire depuis
        // le nord — d'où le `-cos` sur y, comme dans le ray marching.
        let (dx, dy) = (
            (bearing.to_radians()).sin(),
            -(bearing.to_radians()).cos(),
        );
        let mut free_px = 0.0;
        for step in 1..=max_steps {
            let (x, y) = (px + dx * step as f64, py + dy * step as f64);
            if occupied(x, y) {
                break;
            }
            free_px = step as f64;
        }
        measured.push((bearing, free_px * dsm.meters_per_pixel));
    }

    let best_free = measured.iter().fold(0.0_f64, |acc, (_, d)| acc.max(*d));
    if best_free <= 0.0 {
        // Enfermé de toutes parts : rien de mieux à proposer que le cap voulu,
        // et l'appelant bornera l'inclinaison avec une distance nulle.
        return (preferred_bearing_deg, 0.0);
    }

    // Parmi les directions à peu près aussi dégagées, celle dont le cap est le
    // plus proche du cadrage voulu. Sans ce départage, deux côtés d'une rue
    // droite s'échangeraient au moindre pixel de différence, et le cadrage
    // sauterait d'un tap à l'autre sur le même établissement.
    let threshold = best_free * 0.8;
    let mut best: Option<(f64, f64, f64)> = None;
    for (open_bearing, free_m) in measured {
        if free_m < threshold {
            continue;
        }
        // La caméra se tient dans la direction dégagée, donc elle regarde à
        // l'opposé.
        let camera_bearing = (open_bearing + 180.0).rem_euclid(360.0);
        let delta = angular_distance_deg(camera_bearing, preferred_bearing_deg);
        if best.as_ref().is_none_or(|(_, _, d)| delta < *d) {
            best = Some((camera_bearing, free_m, delta));
        }
    }

    best.map(|(bearing, free_m, _)| (bearing, free_m))
        .unwrap_or((preferred_bearing_deg, best_free))
}

/// Azimut de `(lat1, lng1)` vers `(lat2, lng2)` : degrés depuis le nord, sens
/// horaire — la convention de tout le projet, celle d'OSM et celle de Mapbox.
fn bearing_deg(lat1: f64, lng1: f64, lat2: f64, lng2: f64) -> f64 {
    let (p1, p2) = (lat1.to_radians(), lat2.to_radians());
    let dl = (lng2 - lng1).to_radians();
    let y = dl.sin() * p2.cos();
    let x = p1.cos() * p2.sin() - p1.sin() * p2.cos() * dl.cos();
    y.atan2(x).to_degrees().rem_euclid(360.0)
}

/// Écart entre deux caps, dans [0, 180].
fn angular_distance_deg(a: f64, b: f64) -> f64 {
    let d = (a - b).rem_euclid(360.0);
    if d > 180.0 { 360.0 - d } else { d }
}

fn nudge_out_of_building(
    dsm: &Dsm,
    owner: &[u32],
    px: f64,
    py: f64,
    max_radius_px: i32,
) -> (f64, f64, f64) {
    // La canopée compte comme du dehors : depuis la transmittance, un point
    // sous un arbre est un point valide — seul un bâtiment doit repousser.
    let at = |x: i32, y: i32| -> Option<u32> {
        if x < 0 || y < 0 || x >= dsm.width as i32 || y >= dsm.height as i32 {
            return None;
        }
        let o = owner[y as usize * dsm.width + x as usize];
        Some(if o >= OWNER_FIRST_SENTINEL { OWNER_TERRAIN } else { o })
    };

    let (cx, cy) = (px.round() as i32, py.round() as i32);
    if at(cx, cy) == Some(OWNER_TERRAIN) {
        return (px, py, 0.0); // déjà dehors
    }

    for r in 1..=max_radius_px {
        let mut best: Option<(f64, i32, i32)> = None;
        for dy in -r..=r {
            for dx in -r..=r {
                // Seulement le bord de l'anneau : l'intérieur a déjà été vu.
                if dx.abs() != r && dy.abs() != r {
                    continue;
                }
                if at(cx + dx, cy + dy) != Some(OWNER_TERRAIN) {
                    continue;
                }
                let d = ((dx * dx + dy * dy) as f64).sqrt();
                if best.as_ref().is_none_or(|(bd, _, _)| d < *bd) {
                    best = Some((d, dx, dy));
                }
            }
        }
        if let Some((d, dx, dy)) = best {
            // Une cellule de marge dans la même direction, pour se poser sur
            // le trottoir plutôt que contre le mur.
            let (ux, uy) = (dx as f64 / d, dy as f64 / d);
            let (nx, ny) = (px + (dx as f64) + ux, py + (dy as f64) + uy);
            let (nx, ny) = if at(nx.round() as i32, ny.round() as i32) == Some(OWNER_TERRAIN) {
                (nx, ny)
            } else {
                (px + dx as f64, py + dy as f64)
            };
            let moved = ((nx - px).hypot(ny - py)) * dsm.meters_per_pixel;
            return (nx, ny, moved);
        }
    }
    (px, py, 0.0)
}

// ---------------------------------------------------------------- debug

/// Profil de la DSM le long du rayon solaire : altitude du terrain+bâtiments
/// contre altitude du rayon, pas à pas. Sert à voir *pourquoi* un point est
/// classé comme il l'est — bâtiment manquant, trop bas, mal placé, etc.
#[derive(Serialize)]
struct RayStep {
    distance_m: f64,
    lat: f64,
    lng: f64,
    /// Altitude de la DSM (relief + bâtiments stampés) à ce pas.
    dsm_m: f32,
    /// Altitude du rayon solaire à ce pas.
    ray_m: f64,
    /// Bâtiment occupant la cellule, s'il y en a un.
    building: Option<String>,
    building_height_m: Option<f32>,
    /// `true` sur le pas qui bloque le rayon.
    blocks: bool,
}

#[derive(Serialize)]
struct DebugRayResponse {
    sun_azimuth_deg: f64,
    sun_elevation_deg: f64,
    ground_m: f32,
    observer_m: f64,
    meters_per_pixel: f64,
    /// Bâtiments chargés dans la fenêtre DSM — un nombre anormalement bas
    /// signale une troncature Overpass.
    buildings_loaded: usize,
    sunlit: bool,
    steps: Vec<RayStep>,
}

async fn debug_ray(
    State(state): State<Arc<AppState>>,
    Query(q): Query<SunlitQuery>,
) -> Result<Json<DebugRayResponse>, (StatusCode, String)> {
    let t = parse_time(q.t.as_deref())?;
    let h = q.observer_height.unwrap_or(1.5);
    let sun = sun_position(t, q.lat, q.lng);
    let ctx = assemble_point(&state, q.lat, q.lng).await?;

    let rad = std::f64::consts::PI / 180.0;
    let (dx, dy) = ((sun.azimuth_deg * rad).sin(), -(sun.azimuth_deg * rad).cos());
    let tan_elev = (sun.elevation_deg * rad).tan();
    let step_m = ctx.dsm.meters_per_pixel;
    let z0 = ctx.ground as f64 + h;

    // 200 pas ≈ 315 m : largement de quoi couvrir les casters urbains.
    let mut steps = Vec::new();
    let mut blocked = false;
    for i in 1..=200usize {
        let (x, y) = (ctx.px + dx * i as f64, ctx.py + dy * i as f64);
        let Some(dsm_m) = ctx.dsm.sample(x, y) else { break };
        let ray_m = z0 + i as f64 * step_m * tan_elev;
        let blocks = !blocked && (dsm_m as f64) > ray_m;
        if blocks {
            blocked = true;
        }
        let (xi, yi) = (x.round().max(0.0) as usize, y.round().max(0.0) as usize);
        let b = ctx
            .owner
            .get(yi * ctx.dsm.width + xi)
            .copied()
            .filter(|&o| o != OWNER_TERRAIN)
            .and_then(|o| ctx.buildings.get(o as usize));
        let (lat, lng) = latlon_of_world_px(ctx.origin_x + x, ctx.origin_y + y);
        steps.push(RayStep {
            distance_m: i as f64 * step_m,
            lat,
            lng,
            dsm_m,
            ray_m,
            building: b.map(|b| b.osm_id.clone()),
            building_height_m: b.map(|b| b.height_m),
            blocks,
        });
    }

    Ok(Json(DebugRayResponse {
        sun_azimuth_deg: sun.azimuth_deg,
        sun_elevation_deg: sun.elevation_deg,
        ground_m: ctx.ground,
        observer_m: z0,
        meters_per_pixel: ctx.dsm.meters_per_pixel,
        buildings_loaded: ctx.buildings.len(),
        sunlit: !blocked && sun.is_up(),
        steps,
    }))
}

/// Tuile Mapterhorn décodée, via le cache partagé du process.
async fn fetch_tile(
    state: &AppState,
    z: u32,
    x: u32,
    y: u32,
) -> Result<Arc<Vec<f32>>, (StatusCode, String)> {
    dem::fetch_tile(&state.http, &state.tiles, z, x, y)
        .await
        .map_err(|e| (StatusCode::BAD_GATEWAY, e))
}

// ---------------------------------------------------------- établissements

#[derive(Deserialize)]
struct PlacesQuery {
    /// `min_lon,min_lat,max_lon,max_lat`
    bbox: String,
    /// Langue des libellés ("fr", "en"). Défaut : français.
    lang: Option<String>,
    t: Option<String>,
    /// Défaut 1,5 m : personne attablée en terrasse.
    observer_height: Option<f64>,
    /// Décalage du fuseau du client en minutes, pour caler `sun_day` sur SA
    /// journée locale — sans lui, la journée découpée serait celle d'UTC,
    /// décalée de deux heures en été à Paris.
    utc_offset_minutes: Option<i32>,
}

#[derive(Serialize, Clone)]
struct PlacesResponse {
    t_unix: f64,
    sun_azimuth_deg: f64,
    sun_elevation_deg: f64,
    /// Début (unix) de la journée locale couverte par les `sun_day`, et pas
    /// de chaque tranche en secondes.
    day_start_unix: f64,
    day_step_s: f64,
    count: usize,
    places: Vec<Place>,
}

#[derive(Serialize, Clone)]
struct Place {
    /// Identifiant OSM, ex. "node/123456" ou "way/789".
    id: String,
    name: Option<String>,
    amenity: Option<String>,
    /// Tag `outdoor_seating` d'OSM. **Absent de la réponse si non renseigné** —
    /// ce qui est le cas le plus fréquent, et ne signifie pas « pas de
    /// terrasse ». Le client ne doit rien afficher dans ce cas.
    #[serde(skip_serializing_if = "Option::is_none")]
    outdoor_seating: Option<bool>,
    /// `true` quand la présence et/ou la position de la terrasse viennent d'une
    /// contribution utilisateur plutôt que d'OSM. Dans ce cas `snapped_*` est
    /// la position exacte de la terrasse, pas une estimation.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    terrace_from_user: bool,
    /// Pseudo de qui a signalé la terrasse. Absent des contributions antérieures
    /// à l'authentification, et bien sûr des établissements sans contribution.
    #[serde(skip_serializing_if = "Option::is_none")]
    terrace_author: Option<String>,
    lat: f64,
    lng: f64,
    sunlit: bool,
    /// Distingue l'ombre portée de la nuit, ce que `sunlit` seul ne peut pas
    /// dire — les deux y valent `false`.
    state: SunState,
    /// Toute la journée locale en un mot : 144 tranches de 10 min, un bit
    /// par tranche (bit à 1 = au soleil), octets en hexadécimal, bit de la
    /// tranche `i` = octet `i/8`, bit `i%8` (LSB en premier). Permet au
    /// client de reclasser le lieu à chaque cran du slider sans requête —
    /// la nuit se déduit localement de l'élévation solaire, le bit ne
    /// distingue que soleil/ombre.
    sun_day: String,
    /// Ce qui bloque le soleil (absent si `sunlit`) — sert au debug visuel
    /// côté client : « c'est cet immeuble-là qui te met à l'ombre ».
    #[serde(skip_serializing_if = "Option::is_none")]
    blocker: Option<Blocker>,
    /// Absents si le nœud OSM était déjà sur du sol libre. Sinon : le point
    /// réellement classé, ramené hors de l'emprise du bâtiment hôte, et la
    /// distance parcourue pour y arriver.
    #[serde(skip_serializing_if = "Option::is_none")]
    snapped_lat: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    snapped_lng: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    snapped_distance_m: Option<f64>,
    /// Cap conseillé pour poser la caméra sur ce lieu, en degrés depuis le
    /// nord — et la distance libre, en mètres, disponible **derrière**
    /// l'objectif à ce cap.
    ///
    /// Mapbox place la caméra en retrait du point visé (`altitude × tan(pitch)`,
    /// ~220 m à z19.5/51°). Sans ce cap, viser une terrasse « de face » depuis
    /// la rue met l'objectif dans le pâté de maisons d'en face, et l'immeuble
    /// masque la terrasse dès que la rue est étroite. Ce cap regarde **le long**
    /// de la rue, où le vide existe.
    ///
    /// La distance sert au client à borner l'inclinaison :
    /// `pitch ≤ atan(distance / altitude)`. Dans une cour fermée elle vaut 0,
    /// et le cadrage se dégrade proprement vers une vue verticale.
    #[serde(skip_serializing_if = "Option::is_none")]
    view_bearing_deg: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    view_free_distance_m: Option<f64>,
    elevation_m: f32,
    /// Champs OSM optionnels — couverture très inégale selon les POI.
    website: Option<String>,
    phone: Option<String>,
    /// Libellé traduit de la catégorie — le client n'a plus à connaître les
    /// valeurs de tags OSM.
    category_label: Option<String>,
    /// Types de cuisine traduits. Le tag OSM est une liste séparée par `;` et
    /// remplie de clés techniques (`coffee_shop`), inutilisables telles quelles.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    cuisine_labels: Vec<String>,
    opening_hours: Option<OpeningHoursPayload>,
    cuisine: Option<String>,
    /// Identifiant Wikidata (ex. "Q123456") : présent sur ~15% des POI,
    /// utilisable côté client pour aller chercher une photo (propriété P18)
    /// via l'API Wikidata/Commons — OSM ne stocke pas de photos lui-même.
    wikidata: Option<String>,
    /// Mobilier urbain (bancs, tables de pique-nique) — absents partout
    /// ailleurs. `direction_deg` : où porte le regard assis, en degrés depuis
    /// le nord.
    #[serde(skip_serializing_if = "Option::is_none")]
    direction_deg: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    covered: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    backrest: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    seats: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    material: Option<String>,
}

#[derive(Deserialize)]
struct TerraceReportBody {
    /// Identifiant OSM de l'établissement ("node/123"). Dans le corps et non
    /// dans le chemin : il contient une barre oblique, qui casserait le
    /// routage.
    osm_id: String,
    has_terrace: bool,
    /// Position de la terrasse. Facultative : on peut signaler une terrasse
    /// sans la situer, ou signaler son absence.
    lat: Option<f64>,
    lng: Option<f64>,
}

#[derive(Serialize)]
struct TerraceReportResponse {
    osm_id: String,
    has_terrace: bool,
    located: bool,
}

/// Enregistre la terrasse signalée par un utilisateur : sa présence, et sa
/// position si elle a été pointée sur la carte.
///
/// Cette position vaut mieux que tout ce qu'on peut déduire : OSM place le nœud
/// d'un bar sur son bâtiment, et notre repli le ressort au jugé sur le sol
/// libre le plus proche, sans savoir de quel côté est la rue.
async fn report_terrace(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(body): Json<TerraceReportBody>,
) -> Result<Json<TerraceReportResponse>, (StatusCode, String)> {
    // Contribuer demande un compte : sans quoi n'importe qui écrase la
    // contribution de n'importe qui, `osm_id` étant la clé primaire.
    let identity = authenticate(&state, &headers).await?;
    if let (Some(lat), Some(lng)) = (body.lat, body.lng) {
        if !(-85.0..=85.0).contains(&lat) || !(-180.0..=180.0).contains(&lng) {
            return Err((StatusCode::BAD_REQUEST, "lat/lng hors bornes".into()));
        }
    }

    // Refuse les identifiants inconnus, sinon la table se remplit de lignes
    // orphelines qu'aucune requête ne rattachera jamais à un établissement.
    let exists = db::place_exists(&state.pool, &body.osm_id)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("PostGIS : {e}")))?;
    if !exists {
        return Err((
            StatusCode::NOT_FOUND,
            format!("établissement inconnu : {}", body.osm_id),
        ));
    }

    let report = db::TerraceReport {
        osm_id: body.osm_id.clone(),
        has_terrace: body.has_terrace,
        // Une terrasse absente n'a pas de position à retenir.
        author_uid: Some(identity.uid.clone()),
        author_username: None,
        lat: body.has_terrace.then_some(body.lat).flatten(),
        lng: body.has_terrace.then_some(body.lng).flatten(),
    };
    db::upsert_terrace_report(&state.pool, &report)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("PostGIS : {e}")))?;

    // Les classifications déjà calculées ignorent cette contribution : les
    // jeter force le recalcul au prochain appel, sinon l'utilisateur ne verrait
    // aucun effet à sa contribution.
    state.places_results.write().await.clear();

    // Remontée vers OSM, si le compte y est lié. Après l'écriture locale et
    // dans une tâche détachée : la carte doit être juste même si OSM est
    // indisponible, et le contributeur n'a pas à attendre son API.
    if osm_push::is_configured() {
        osm_push::enqueue_and_spawn(
            state.pool.clone(),
            state.http.clone(),
            identity.uid.clone(),
            report.osm_id.clone(),
            osm_push::PushPayload::Terrace { has_terrace: report.has_terrace },
        );
    }

    println!(
        "[terrace] {} → has_terrace={} {}",
        report.osm_id,
        report.has_terrace,
        match (report.lat, report.lng) {
            (Some(lat), Some(lng)) => format!("({lat:.6}, {lng:.6})"),
            _ => "sans position".to_string(),
        }
    );

    Ok(Json(TerraceReportResponse {
        located: report.lat.is_some() && report.lng.is_some(),
        osm_id: report.osm_id,
        has_terrace: report.has_terrace,
    }))
}

#[derive(Deserialize)]
struct FurnitureContributionBody {
    /// "bench" ou "picnic_table" — les deux seules catégories de mobilier
    /// que l'app sait poser en modèle 3D.
    category: String,
    lat: f64,
    lng: f64,
    /// Cap du meuble en degrés depuis le nord. Absent : le modèle se pose
    /// sans rotation, comme un meuble OSM sans tag `direction`.
    direction_deg: Option<f64>,
    /// Dossier ou non — n'a de sens que pour un banc, ignoré côté base pour
    /// une table. Absent plutôt que `false` par défaut : on ne veut pas
    /// affirmer une absence de dossier que l'app n'aurait pas demandée.
    backrest: Option<bool>,
}

#[derive(Serialize)]
struct FurnitureContributionResponse {
    id: String,
    category: String,
    lat: f64,
    lng: f64,
    direction_deg: Option<f64>,
    backrest: Option<bool>,
    /// Toujours `true` : conservé pour la forme, plus aucune soumission
    /// n'est refusée — cf. `db::submit_furniture_contribution`.
    applied: bool,
}

/// Ajoute un banc ou une table de pique-nique posé depuis l'app.
///
/// Contrairement à une terrasse, il n'y a rien à corriger dans OSM : le
/// mobilier ajouté ici est directement rangé dans `places`, sous un
/// `osm_id` synthétique qui ne rejouera jamais avec un réimport OSM.
async fn add_furniture(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(body): Json<FurnitureContributionBody>,
) -> Result<Json<FurnitureContributionResponse>, (StatusCode, String)> {
    let identity = authenticate(&state, &headers).await?;
    if !matches!(body.category.as_str(), "bench" | "picnic_table") {
        return Err((
            StatusCode::BAD_REQUEST,
            "catégorie inconnue : attendu bench ou picnic_table".into(),
        ));
    }
    if !(-85.0..=85.0).contains(&body.lat) || !(-180.0..=180.0).contains(&body.lng) {
        return Err((StatusCode::BAD_REQUEST, "lat/lng hors bornes".into()));
    }

    let osm_id = db::insert_user_furniture(
        &state.pool,
        &body.category,
        body.lat,
        body.lng,
        body.direction_deg,
        body.backrest,
        &identity.uid,
    )
    .await
    .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("PostGIS : {e}")))?;

    // Comme pour une terrasse : la classification déjà en cache ignore ce
    // nouveau point tant qu'on ne la jette pas.
    state.places_results.write().await.clear();

    // Le meuble n'existe que chez nous (identifiant `user/…`) : on le crée
    // dans OSM, on ne le modifie pas.
    if osm_push::is_configured() {
        let payload = if body.category == "bench" {
            osm_push::PushPayload::Bench {
                lat: body.lat,
                lng: body.lng,
                direction_deg: body.direction_deg,
                backrest: body.backrest,
            }
        } else {
            osm_push::PushPayload::PicnicTable {
                lat: body.lat,
                lng: body.lng,
                direction_deg: body.direction_deg,
            }
        };
        osm_push::enqueue_and_spawn(
            state.pool.clone(),
            state.http.clone(),
            identity.uid.clone(),
            osm_id.clone(),
            payload,
        );
    }

    println!("[furniture] {osm_id} ({}) ajouté par {}", body.category, identity.uid);

    Ok(Json(FurnitureContributionResponse {
        id: osm_id,
        category: body.category,
        lat: body.lat,
        lng: body.lng,
        direction_deg: body.direction_deg,
        backrest: body.backrest,
        applied: true,
    }))
}

#[derive(Deserialize)]
struct FurnitureEditBody {
    /// Identifiant du meuble à corriger — le sien, qu'il vienne d'une
    /// contribution (`user/…`) ou d'OSM. Dans le corps et non le chemin,
    /// comme pour `report_terrace` : un `osm_id` OSM contient une barre
    /// oblique, qui casserait le routage.
    id: String,
    lat: f64,
    lng: f64,
    direction_deg: Option<f64>,
    backrest: Option<bool>,
}

/// Corrige la position, l'orientation ou le dossier d'un banc ou d'une table
/// déjà en base — contribué depuis l'app ou importé d'OSM.
///
/// Toujours appliquée à `places` et journalisée : pas de verrou de propriété,
/// n'importe quel contributeur authentifié peut corriger n'importe quel
/// meuble — cf. `db::submit_furniture_contribution` pour pourquoi ça ne perd
/// pas les corrections des autres tant que l'écran d'édition préremplit son
/// formulaire depuis l'état courant.
///
/// Écrit directement dans `places`, sans passer par une table de contribution
/// séparée comme `place_terraces` : un banc, contrairement à un établissement,
/// n'a pas de nœud OSM ambigu à corriger par-dessus, sa position EST la donnée.
/// Contrepartie assumée : sur un meuble importé d'OSM, un réimport ultérieur
/// (`bin/import`) écrasera la correction avec la valeur du tag d'origine —
/// comme toute colonne de `places` hors contribution.
async fn edit_furniture(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(body): Json<FurnitureEditBody>,
) -> Result<Json<FurnitureContributionResponse>, (StatusCode, String)> {
    let identity = authenticate(&state, &headers).await?;
    if !(-85.0..=85.0).contains(&body.lat) || !(-180.0..=180.0).contains(&body.lng) {
        return Err((StatusCode::BAD_REQUEST, "lat/lng hors bornes".into()));
    }

    let category = db::submit_furniture_contribution(
        &state.pool,
        &body.id,
        body.lat,
        body.lng,
        body.direction_deg,
        body.backrest,
        &identity.uid,
    )
    .await
    .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("PostGIS : {e}")))?
    .ok_or((
        StatusCode::NOT_FOUND,
        format!("mobilier inconnu : {}", body.id),
    ))?;

    state.places_results.write().await.clear();

    println!("[furniture] {} ({category}) modifié par {}", body.id, identity.uid);

    Ok(Json(FurnitureContributionResponse {
        id: body.id,
        category,
        lat: body.lat,
        lng: body.lng,
        direction_deg: body.direction_deg,
        backrest: body.backrest,
        applied: true,
    }))
}

#[derive(Deserialize)]
struct FurnitureContributionsQuery {
    id: String,
}

#[derive(Serialize)]
struct FurnitureContributionEntry {
    /// Absent pour une contribution antérieure à l'authentification.
    #[serde(skip_serializing_if = "Option::is_none")]
    username: Option<String>,
    lat: f64,
    lng: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    direction_deg: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    backrest: Option<bool>,
    /// Version actuellement affichée sur la carte, parmi toutes celles
    /// listées ici.
    applied: bool,
    created_at: chrono::DateTime<chrono::Utc>,
}

/// Historique des contributions d'un banc ou d'une table — qui a proposé
/// quoi, et quand.
async fn furniture_contributions(
    State(state): State<Arc<AppState>>,
    Query(query): Query<FurnitureContributionsQuery>,
) -> Result<Json<Vec<FurnitureContributionEntry>>, (StatusCode, String)> {
    let rows = db::furniture_contributions(&state.pool, &query.id)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("PostGIS : {e}")))?;
    Ok(Json(
        rows.into_iter()
            .map(|r| FurnitureContributionEntry {
                username: r.username,
                lat: r.lat,
                lng: r.lng,
                direction_deg: r.direction_deg,
                backrest: r.backrest,
                applied: r.applied,
                created_at: r.created_at,
            })
            .collect(),
    ))
}

#[derive(Deserialize)]
struct TerraceContributionsQuery {
    id: String,
}

#[derive(Serialize)]
struct TerraceContributionEntry {
    #[serde(skip_serializing_if = "Option::is_none")]
    username: Option<String>,
    has_terrace: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    lat: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    lng: Option<f64>,
    created_at: chrono::DateTime<chrono::Utc>,
}

/// Historique des signalements de terrasse d'un établissement — qui a signalé
/// quoi, et quand.
async fn terrace_contributions(
    State(state): State<Arc<AppState>>,
    Query(query): Query<TerraceContributionsQuery>,
) -> Result<Json<Vec<TerraceContributionEntry>>, (StatusCode, String)> {
    let rows = db::terrace_contributions(&state.pool, &query.id)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("PostGIS : {e}")))?;
    Ok(Json(
        rows.into_iter()
            .map(|r| TerraceContributionEntry {
                username: r.username,
                has_terrace: r.has_terrace,
                lat: r.lat,
                lng: r.lng,
                created_at: r.created_at,
            })
            .collect(),
    ))
}

/// Horaires d'ouverture prêts à afficher.
///
/// `weekly` absent = chaîne non décodable : le client affiche `raw` tel quel.
/// Le décodage vit ici pour que les clients n'aient qu'à rendre, et pour
/// qu'Android n'ait pas à réécrire la grammaire `opening_hours`.
#[derive(Serialize, Clone)]
struct OpeningHoursPayload {
    /// Toujours présent, pour l'affichage de repli et pour le debug.
    raw: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    weekly: Option<Vec<OpeningHoursDay>>,
}

#[derive(Serialize, Clone)]
struct OpeningHoursDay {
    /// 0 = lundi.
    index: usize,
    label: String,
    /// Vide = fermé ce jour-là.
    ranges: Vec<String>,
}

fn opening_hours_payload(raw: &str, lang: Lang) -> OpeningHoursPayload {
    let weekly = opening_hours::parse(raw).map(|week| {
        let labels = lang.weekdays();
        week.iter()
            .enumerate()
            .map(|(index, ranges)| OpeningHoursDay {
                index,
                label: labels[index].to_string(),
                ranges: ranges
                    .iter()
                    .map(|r| {
                        if r.is_all_day() {
                            lang.all_day().to_string()
                        } else {
                            r.text()
                        }
                    })
                    .collect(),
            })
            .collect()
    });
    OpeningHoursPayload {
        raw: raw.to_string(),
        weekly,
    }
}

/// Bars/restaurants/cafés avec terrasse dans la bbox, classés soleil/ombre.
///
/// Limite assumée (POC) : classification binaire au centroïde — une terrasse
/// est un polygone, potentiellement mi-ombre mi-soleil. Prochaine étape :
/// échantillonner 3-5 points dans un buffer côté rue et renvoyer un %.
async fn places(
    State(state): State<Arc<AppState>>,
    Query(q): Query<PlacesQuery>,
) -> Result<Json<PlacesResponse>, (StatusCode, String)> {
    let request_start = std::time::Instant::now();
    let t = parse_time(q.t.as_deref())?;
    let h = q.observer_height.unwrap_or(1.5);
    let lang = Lang::parse(q.lang.as_deref());

    // bbox = min_lon,min_lat,max_lon,max_lat
    let parts: Vec<f64> = q
        .bbox
        .split(',')
        .filter_map(|s| s.trim().parse().ok())
        .collect();
    let [w, s, e, n] = parts[..] else {
        return Err((
            StatusCode::BAD_REQUEST,
            "bbox attendue : min_lon,min_lat,max_lon,max_lat".into(),
        ));
    };
    if s >= n || w >= e || !(-85.0..=85.0).contains(&s) || !(-85.0..=85.0).contains(&n) {
        return Err((StatusCode::BAD_REQUEST, "bbox invalide".into()));
    }

    // Journée locale du client, pour découper `sun_day` sur SES minuits.
    let utc_offset_minutes = q.utc_offset_minutes.unwrap_or(0);
    let (day_start, _) = day_bounds_local(t, utc_offset_minutes);

    // Cache de résultat : même bbox + même tranche de 5 min + même hauteur
    // d'observateur → renvoie directement la classification déjà calculée,
    // sans refaire tuiles/bâtiments/ray marching. La tranche fine reste dans
    // la clé malgré `sun_day` (qui ne dépend que du jour) : les champs à
    // l'instant `t` — `sunlit`, `state`, `blocker` — en dépendent, eux. Le
    // client muni du bitfield ne redemande de toute façon plus qu'une fois
    // par jour et par zone.
    let bucket = (t / 300.0).round() as i64;
    let result_key = format!(
        "{w:.4},{s:.4},{e:.4},{n:.4},{bucket},{h:.2},{lang:?},{utc_offset_minutes}"
    );
    if let Some(hit) = state.places_results.read().await.get(&result_key) {
        println!("[places] {result_key} → cache hit ({:.1} ms)",
                 request_start.elapsed().as_secs_f64() * 1000.0);
        return Ok(Json((**hit).clone()));
    }

    // Intervalle de tuiles couvrant la bbox + 1 tuile de marge (casters).
    let (wx0, wy0) = world_px(n, w); // coin nord-ouest
    let (wx1, wy1) = world_px(s, e); // coin sud-est
    let max_tile = (1u32 << ZOOM) - 1;
    let tx0 = ((wx0 / TILE_SIZE as f64) as u32).saturating_sub(1);
    let ty0 = ((wy0 / TILE_SIZE as f64) as u32).saturating_sub(1);
    let tx1 = ((wx1 / TILE_SIZE as f64) as u32 + 1).min(max_tile);
    let ty1 = ((wy1 / TILE_SIZE as f64) as u32 + 1).min(max_tile);
    if (tx1 - tx0 + 1) > 8 || (ty1 - ty0 + 1) > 8 {
        return Err((
            StatusCode::BAD_REQUEST,
            "bbox trop grande (max ~3 km de côté à ce zoom)".into(),
        ));
    }

    let phase = std::time::Instant::now();
    let pois = db::places_in_bbox(&state.pool, s, w, n, e)
        .await
        .map_err(|err| (StatusCode::INTERNAL_SERVER_ERROR, format!("PostGIS : {err}")))?;
    let reports = db::terrace_reports_in_bbox(&state.pool, s, w, n, e)
        .await
        .map_err(|err| (StatusCode::INTERNAL_SERVER_ERROR, format!("PostGIS : {err}")))?;
    let t_pois_ms = phase.elapsed().as_secs_f64() * 1000.0;

    let phase = std::time::Instant::now();
    let (mut dsm, origin_x, origin_y) = assemble_grid(&state, tx0, ty0, tx1, ty1, (s + n) / 2.0).await?;
    let t_dsm_ms = phase.elapsed().as_secs_f64() * 1000.0;
    // Relief seul, avant stamping des bâtiments : sert de sol pour chaque
    // terrasse (cf. commentaire dans classify() — un POI mal placé dans un
    // bâtiment côté OSM ne doit pas hériter de l'altitude du toit).
    let terrain_only = dsm.clone();
    let phase = std::time::Instant::now();
    let (buildings, owner) = add_buildings(&state, &mut dsm, origin_x, origin_y).await?;
    let t_buildings_ms = phase.elapsed().as_secs_f64() * 1000.0;

    let mid_lat = (s + n) / 2.0;
    let mid_lng = (w + e) / 2.0;
    let sun = sun_position(t, mid_lat, mid_lng);
    let params = ShadowParams {
        max_distance_m: 5_000.0,
        observer_height_m: h,
        step_px: 1.0,
        ..ShadowParams::default()
    };
    // Calculé une fois pour toute la classification, pas par établissement :
    // `Dsm::max_elevation` scanne toute la grille, et le refaire par POI a
    // fait passer une zone dense (1074 lieux) de ~150 ms à ~2 s.
    let dsm_max_elevation = dsm.max_elevation();

    let phase = std::time::Instant::now();
    let places: Vec<Place> = pois
        .iter()
        .map(|p| {
            let report = reports.get(&p.osm_id);
            let terrace = report.and_then(|r| Some((r.lat?, r.lng?)));
            let (osm_wx, osm_wy) = world_px(p.lat, p.lng);

            // Une terrasse située par un utilisateur est prise telle quelle :
            // il l'a pointée sur la carte, donc elle est déjà au bon endroit.
            // La rectifier avec `nudge_out_of_building` détruirait justement la
            // précision qu'on lui a demandée — une terrasse sous arcade ou
            // adossée à la façade se retrouverait déplacée au milieu de la rue.
            // Un banc ou une table de pique-nique est cartographié à sa
            // position réelle : le recalage hors bâtiment, pensé pour des
            // nœuds d'établissement posés sur leur immeuble, ne ferait que
            // déplacer un meuble déjà bien placé.
            let is_furniture =
                matches!(p.amenity.as_deref(), Some("bench" | "picnic_table"));
            let (px, py, moved_m) = match terrace {
                Some((lat, lng)) => {
                    let (tx, ty) = world_px(lat, lng);
                    let moved = ((tx - osm_wx).hypot(ty - osm_wy)) * dsm.meters_per_pixel;
                    (tx - origin_x, ty - origin_y, moved)
                }
                None if is_furniture => (osm_wx - origin_x, osm_wy - origin_y, 0.0),
                // Sinon le nœud OSM est presque toujours posé sur le bâtiment
                // et non sur la terrasse : on le ramène sur le sol libre
                // voisin (12 m max).
                None => nudge_out_of_building(&dsm, &owner, osm_wx - origin_x,
                                              osm_wy - origin_y, 8),
            };
            let ground = terrain_only.sample(px, py).unwrap_or(0.0);
            let hit = sun
                .is_up()
                .then(|| shadow_hit_from_ground(&dsm, &sun, px, py, ground, &params, dsm_max_elevation))
                .flatten();

            // Toute la journée du même point, sur la même DSM : 144 tranches
            // de 10 min, un rayon par tranche de jour — la nuit se tranche
            // sur la seule élévation solaire, sans rayon. Échantillonné au
            // milieu de chaque tranche, pour que le bit représente la
            // tranche entière plutôt que son bord.
            let mut day_bits = [0u8; SUN_DAY_SLOTS / 8];
            for slot in 0..SUN_DAY_SLOTS {
                let slot_t = day_start + slot as f64 * SUN_DAY_STEP_S + SUN_DAY_STEP_S / 2.0;
                let slot_sun = sun_position(slot_t, p.lat, p.lng);
                if !slot_sun.is_up() {
                    continue;
                }
                let slot_hit = shadow_hit_from_ground(
                    &dsm, &slot_sun, px, py, ground, &params, dsm_max_elevation,
                );
                if slot_hit.is_none() {
                    day_bits[slot / 8] |= 1 << (slot % 8);
                }
            }
            let sun_day: String = day_bits.iter().map(|b| format!("{b:02x}")).collect();

            let (snapped_lat, snapped_lng) =
                latlon_of_world_px(origin_x + px, origin_y + py);

            // Cap de caméra conseillé, mobilier compris : un banc souffre du
            // même retrait de caméra qu'une terrasse, et le client vole vers
            // les deux.
            //
            // Le cadrage idéal diffère : pour un établissement c'est sa façade
            // en toile de fond, donc le cap du point analysé vers le nœud OSM ;
            // pour un banc orienté, c'est le voir de face, donc l'opposé de son
            // `direction`. Ce n'est qu'une préférence — elle départage des
            // directions également dégagées, elle ne les impose pas.
            let preferred = if is_furniture {
                p.direction_deg
                    .map(|d| (d + 180.0).rem_euclid(360.0))
                    .unwrap_or(0.0)
            } else {
                bearing_deg(snapped_lat, snapped_lng, p.lat, p.lng)
            };
            let view = Some(open_view_bearing(
                &dsm, &owner, px, py, preferred, VIEW_MAX_DISTANCE_M,
            ));

            Place {
                id: p.osm_id.clone(),
                name: p.name.clone(),
                amenity: p.amenity.clone(),
                // La contribution prime sur le tag OSM, y compris pour le
                // contredire : quelqu'un sur place en sait plus que le tag.
                outdoor_seating: report.map(|r| r.has_terrace).or(p.outdoor_seating),
                terrace_from_user: report.is_some(),
                terrace_author: report.and_then(|r| r.author_username.clone()),
                lat: p.lat,
                lng: p.lng,
                sunlit: sun.is_up() && hit.is_none(),
                state: if !sun.is_up() {
                    SunState::Night
                } else if hit.is_none() {
                    SunState::Sunlit
                } else {
                    SunState::Shadow
                },
                sun_day,
                blocker: hit
                    .map(|h| describe_blocker(&h, &dsm, &owner, &buildings, origin_x, origin_y)),
                snapped_lat: (moved_m > 0.0).then_some(snapped_lat),
                snapped_lng: (moved_m > 0.0).then_some(snapped_lng),
                snapped_distance_m: (moved_m > 0.0).then_some(moved_m),
                view_bearing_deg: view.map(|(bearing, _)| bearing),
                view_free_distance_m: view.map(|(_, free)| free),
                elevation_m: ground,
                website: p.website.clone(),
                phone: p.phone.clone(),
                category_label: p.amenity.as_deref().map(|a| i18n::amenity_label(a, lang)),
                cuisine_labels: p
                    .cuisine
                    .as_deref()
                    .map(|c| i18n::cuisine_labels(c, lang))
                    .unwrap_or_default(),
                opening_hours: p
                    .opening_hours
                    .as_deref()
                    .map(|raw| opening_hours_payload(raw, lang)),
                cuisine: p.cuisine.clone(),
                wikidata: p.wikidata.clone(),
                direction_deg: p.direction_deg,
                covered: p.covered,
                backrest: p.backrest,
                seats: p.seats,
                material: p.material.clone(),
            }
        })
        .collect();

    let t_classify_ms = phase.elapsed().as_secs_f64() * 1000.0;

    let response = PlacesResponse {
        t_unix: t,
        sun_azimuth_deg: sun.azimuth_deg,
        sun_elevation_deg: sun.elevation_deg,
        day_start_unix: day_start,
        day_step_s: SUN_DAY_STEP_S,
        count: places.len(),
        places,
    };
    state
        .places_results
        .write()
        .await
        .insert(result_key.clone(), Arc::new(response.clone()));
    println!("[places] {result_key} → {} lieux — pois/reports {t_pois_ms:.1} ms, \
              tuiles DEM {t_dsm_ms:.1} ms, bâtiments+canopée {t_buildings_ms:.1} ms, \
              classification {t_classify_ms:.1} ms, TOTAL {:.1} ms",
             response.count, request_start.elapsed().as_secs_f64() * 1000.0);
    Ok(Json(response))
}

// ---------------------------------------------------------------- arbres

#[derive(Deserialize)]
struct TreesQuery {
    /// `min_lon,min_lat,max_lon,max_lat`
    bbox: String,
}

#[derive(Serialize)]
struct TreesResponse {
    count: usize,
    trees: Vec<Tree>,
}

#[derive(Serialize, Clone)]
struct Tree {
    /// Identifiant OSM ("node/123"). Sert au client à dédoublonner les arbres
    /// entre deux bbox qui se recouvrent, et à cibler leur suppression quand
    /// il purge son cache.
    id: String,
    lat: f64,
    lng: f64,
    height_m: f64,
    crown_radius_m: f64,
}

/// Tuile de canopée pour le masque d'ombre client (PNG RGB : sommet/base de
/// couronne en demi-mètres au-dessus du sol — cf. `canopy_tiles`).
///
/// Les emprises sont requêtées avec une marge d'un rayon de couronne (~30 m) :
/// un arbre dont le tronc est dans la tuile voisine peut déborder ici.
async fn canopy_tile(
    State(state): State<Arc<AppState>>,
    Path((z, x, y)): Path<(u32, u32, u32)>,
) -> Result<impl axum::response::IntoResponse, (StatusCode, String)> {
    if !(canopy_tiles::MIN_Z..=canopy_tiles::MAX_Z).contains(&z) {
        return Err((
            StatusCode::NOT_FOUND,
            format!("zoom {z} hors plage {}-{}", canopy_tiles::MIN_Z, canopy_tiles::MAX_Z),
        ));
    }
    let (s, w, n, e) = canopy_tiles::tile_bounds(z, x, y);
    let pad_lat = 30.0 / 111_320.0;
    let pad_lon = pad_lat / ((s + n) / 2.0).to_radians().cos();
    let (woods, trees) = load_canopy(&state, s - pad_lat, w - pad_lon, n + pad_lat, e + pad_lon)
        .map_err(|err| (StatusCode::INTERNAL_SERVER_ERROR, err))?;

    let tile = canopy_tiles::rasterize(z, x, y, &woods, &trees);
    let png = canopy_tiles::encode_png(&tile)
        .map_err(|err| (StatusCode::INTERNAL_SERVER_ERROR, format!("PNG : {err}")))?;

    Ok((
        [
            (axum::http::header::CONTENT_TYPE, "image/png"),
            // La canopée ne bouge qu'au réimport OSM : cacheable longtemps.
            (axum::http::header::CACHE_CONTROL, "public, max-age=86400"),
        ],
        png,
    ))
}

/// Arbres OSM (`natural=tree`) de la zone — aucun calcul soleil/ombre ici
/// (le rendu/l'extrusion restent côté client), juste la géométrie servie
/// depuis PostGIS.
async fn trees(
    State(state): State<Arc<AppState>>,
    Query(q): Query<TreesQuery>,
) -> Result<Json<TreesResponse>, (StatusCode, String)> {
    let parts: Vec<f64> = q
        .bbox
        .split(',')
        .filter_map(|s| s.trim().parse().ok())
        .collect();
    let [w, s, e, n] = parts[..] else {
        return Err((
            StatusCode::BAD_REQUEST,
            "bbox attendue : min_lon,min_lat,max_lon,max_lat".into(),
        ));
    };
    if s >= n || w >= e || !(-85.0..=85.0).contains(&s) || !(-85.0..=85.0).contains(&n) {
        return Err((StatusCode::BAD_REQUEST, "bbox invalide".into()));
    }

    let raw = state
        .vstore
        .trees(s, w, n, e)
        .map_err(|err| (StatusCode::INTERNAL_SERVER_ERROR, format!("vtiles : {err}")))?;
    let trees: Vec<Tree> = raw
        .into_iter()
        .map(|t| Tree {
            id: t.osm_id,
            lat: t.lat,
            lng: t.lng,
            height_m: t.height_m,
            crown_radius_m: t.crown_radius_m,
        })
        .collect();
    Ok(Json(TreesResponse {
        count: trees.len(),
        trees,
    }))
}


fn parse_time(raw: Option<&str>) -> Result<f64, (StatusCode, String)> {
    let Some(raw) = raw else {
        return Ok(chrono::Utc::now().timestamp() as f64);
    };
    if let Ok(unix) = raw.parse::<f64>() {
        return Ok(unix);
    }
    chrono::DateTime::parse_from_rfc3339(raw)
        .map(|d| d.timestamp() as f64)
        .map_err(|_| {
            (
                StatusCode::BAD_REQUEST,
                format!("t invalide : {raw} (RFC3339 ou secondes Unix)"),
            )
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// L'arbitrage entre natures d'ombre suit l'ordre déclaré, du plus anodin
    /// au plus couvrant — c'est `Ord` dérivé qui le porte, donc l'ordre des
    /// variantes de `BlockerKind` est du code, pas de la documentation.
    #[test]
    fn blocker_kind_priority_order() {
        use BlockerKind::*;
        assert!(Arbre < Bois && Bois < Batiment && Batiment < Relief);

        // Le cas cité en exemple : à l'ombre d'un arbre ET d'un bâtiment, on
        // annonce le bâtiment.
        assert_eq!([Arbre, Batiment].into_iter().max(), Some(Batiment));
        // Une crête derrière un mur l'emporte sur le mur.
        assert_eq!([Batiment, Relief].into_iter().max(), Some(Relief));
        // Sous les arbres d'un bois, l'emprise prime sur le sujet isolé.
        assert_eq!([Arbre, Bois].into_iter().max(), Some(Bois));
        assert_eq!(Vec::<BlockerKind>::new().into_iter().max(), None);
    }

    /// Les quatre natures se sérialisent en minuscules, sans accent : ce sont
    /// des valeurs d'API, lues telles quelles par le client.
    #[test]
    fn blocker_kind_serializes_lowercase() {
        let json = serde_json::to_string(&[
            BlockerKind::Arbre,
            BlockerKind::Bois,
            BlockerKind::Batiment,
            BlockerKind::Relief,
        ])
        .unwrap();
        assert_eq!(json, r#"["arbre","bois","batiment","relief"]"#);
    }

    /// Une emprise boisée en PENTE doit recevoir sa canopée sur toute sa
    /// hauteur, pas seulement autour de l'altitude de son centre.
    ///
    /// Régression réelle : la Forêt Domaniale de Vallorcine était calée sur le
    /// sol de son centre de bbox (1 633 m), si bien que ses parties hautes
    /// (1 694 m) se retrouvaient 43 m au-dessus de leur propre canopée. La
    /// condition d'écriture (`dsm.data[idx] < target`) ne passait plus, aucune
    /// canopée n'y était posée, et un point en pleine forêt ressortait « au
    /// soleil » toute la journée.
    #[test]
    fn sloped_wood_gets_canopy_everywhere() {
        // Relief qui monte de 100 m d'ouest en est sur la largeur de la grille.
        let (w, h) = (64usize, 8usize);
        let mut terrain = Dsm::flat(w, h, 1.6, 0.0);
        for y in 0..h {
            for x in 0..w {
                terrain.data[y * w + x] = x as f32 * 100.0 / w as f32;
            }
        }

        // Emprise couvrant toute la grille, en (lat, lon) autour d'un point
        // arbitraire — les anneaux sont reprojetés en pixels par `world_px`.
        let (origin_x, origin_y) = world_px(46.03, 6.89);
        let corner = |dx: f64, dy: f64| {
            latlon_of_world_px(origin_x + dx, origin_y + dy)
        };
        let rings = vec![vec![
            corner(0.0, 0.0),
            corner(w as f64 - 1.0, 0.0),
            corner(w as f64 - 1.0, h as f64 - 1.0),
            corner(0.0, h as f64 - 1.0),
        ]];
        let wood = Building {
            osm_id: "relation/1".into(),
            name: Some("Forêt en pente".into()),
            rings,
            height_m: 18.0,
            height_from_osm: false,
            leaf_type: Some(helios_server::osm::LeafType::Broadleaved),
        };

        let mut dsm = terrain.clone();
        let mut owner = vec![OWNER_TERRAIN; w * h];
        stamp_canopy(&mut dsm, &terrain, &mut owner, origin_x, origin_y, &[wood], &[]);

        // Au bas comme au sommet de la pente, la couronne doit couvrir
        // exactement les 18 m au-dessus du sol LOCAL.
        for x in [2usize, w / 2, w - 3] {
            let i = (h / 2) * w + x;
            let ground = terrain.data[i];
            let (base, top) = dsm
                .canopy_at(x as f64, (h / 2) as f64)
                .unwrap_or_else(|| panic!("aucune canopée en x={x} (sol {ground} m)"));
            assert!(
                (base - ground).abs() < 0.5,
                "base de couronne calée sur le sol local en x={x} : {base} vs {ground}"
            );
            assert!(
                (top - (ground + 18.0)).abs() < 0.5,
                "sommet à sol+18 m en x={x} : {top} vs {}",
                ground + 18.0
            );
        }
    }

    /// Grille jouet : une rue nord-sud dégagée, du bâti de part et d'autre.
    ///
    /// ```
    ///   x → 0 1 2 3 4
    ///        # . # # #      y = 0
    ///        # . # # #      …
    ///        # . # # #
    /// ```
    /// La colonne 1 est libre, tout le reste est bâti.
    fn street_grid(width: usize, height: usize, free_column: usize) -> (Dsm, Vec<u32>) {
        let dsm = Dsm::flat(width, height, 1.0, 0.0);
        let mut owner = vec![1u32; width * height]; // 1 = un bâtiment
        for y in 0..height {
            owner[y * width + free_column] = OWNER_TERRAIN;
        }
        (dsm, owner)
    }

    #[test]
    fn vise_le_long_de_la_rue_et_non_a_travers() {
        let (dsm, owner) = street_grid(5, 41, 1);
        // Au milieu de la rue, avec un cadrage voulu vers l'est (90°) —
        // c'est-à-dire pile vers le mur d'en face.
        let (bearing, free_m) = open_view_bearing(&dsm, &owner, 1.0, 20.0, 90.0, 100.0);

        // La rue court nord-sud : la caméra doit regarder le long, donc au nord
        // (0°) ou au sud (180°), jamais vers le mur qu'on lui proposait.
        assert!(bearing == 0.0 || bearing == 180.0, "cap retenu : {bearing}");
        assert!(free_m > 15.0, "distance libre : {free_m}");
    }

    #[test]
    fn le_cap_rendu_est_celui_de_la_camera_pas_du_vide() {
        // Rue nord-sud, point collé au bord SUD : le vide est au nord, donc la
        // caméra s'y place et regarde vers le sud (180°).
        let (dsm, owner) = street_grid(5, 41, 1);
        let (bearing, _) = open_view_bearing(&dsm, &owner, 1.0, 39.0, 180.0, 100.0);
        assert_eq!(bearing, 180.0, "la caméra doit regarder à l'opposé du vide");
    }

    #[test]
    fn departage_par_le_cadrage_voulu() {
        // Rue symétrique : les deux sens sont aussi dégagés. Le cap voulu doit
        // trancher, sinon le cadrage sauterait d'un tap à l'autre.
        let (dsm, owner) = street_grid(5, 41, 1);
        let (vers_nord, _) = open_view_bearing(&dsm, &owner, 1.0, 20.0, 10.0, 100.0);
        let (vers_sud, _) = open_view_bearing(&dsm, &owner, 1.0, 20.0, 170.0, 100.0);
        assert_eq!(vers_nord, 0.0);
        assert_eq!(vers_sud, 180.0);
    }

    #[test]
    fn enferme_de_toutes_parts_rend_le_cap_voulu_et_zero() {
        let dsm = Dsm::flat(5, 5, 1.0, 0.0);
        let owner = vec![1u32; 25]; // que du bâti
        let (bearing, free_m) = open_view_bearing(&dsm, &owner, 2.0, 2.0, 42.0, 50.0);
        assert_eq!(bearing, 42.0);
        assert_eq!(free_m, 0.0, "sans dégagement, l'appelant doit borner le pitch");
    }

    #[test]
    fn azimut_degres_depuis_le_nord_sens_horaire() {
        // Depuis Paris : plein nord, plein est, plein sud, plein ouest.
        let (lat, lng) = (48.86, 2.35);
        assert!((bearing_deg(lat, lng, lat + 0.01, lng) - 0.0).abs() < 0.5);
        assert!((bearing_deg(lat, lng, lat, lng + 0.01) - 90.0).abs() < 0.5);
        assert!((bearing_deg(lat, lng, lat - 0.01, lng) - 180.0).abs() < 0.5);
        assert!((bearing_deg(lat, lng, lat, lng - 0.01) - 270.0).abs() < 0.5);
    }

    #[test]
    fn ecart_angulaire_prend_le_plus_court_chemin() {
        assert_eq!(angular_distance_deg(10.0, 350.0), 20.0);
        assert_eq!(angular_distance_deg(350.0, 10.0), 20.0);
        assert_eq!(angular_distance_deg(0.0, 180.0), 180.0);
    }

    /// Une journée doit commencer à minuit LOCAL. Le bug d'origine découpait
    /// sur UTC : à Paris en été, un instant à 00h30 locale (22:30Z la veille)
    /// renvoyait la journée précédente, bornée à 02:00 → 02:00.
    #[test]
    fn day_bounds_follow_local_midnight() {
        // 2026-07-25T22:30:00Z = 2026-07-26 00:30 à Paris (UTC+2).
        let t = 1785018600.0;
        let (start, end) = day_bounds_local(t, 120);

        // Minuit local = 22:00Z la veille.
        assert_eq!(start, 1785016800.0);
        assert_eq!(end - start, 86_400.0);
        // L'instant demandé tombe bien dans la journée renvoyée.
        assert!(start <= t && t < end);
    }

    /// Sans décalage, on retombe sur la journée UTC — l'ancien comportement,
    /// conservé comme défaut pour ne pas casser un appel sans paramètre.
    #[test]
    fn zero_offset_is_utc_day() {
        let t = 1785018600.0; // 2026-07-25T22:30:00Z
        let (start, _) = day_bounds_local(t, 0);
        assert_eq!(start, 1784937600.0); // 2026-07-25T00:00:00Z
    }

    /// Fuseaux négatifs (Amériques) : même règle, minuit local.
    #[test]
    fn negative_offset_works() {
        // 2026-07-26T03:00:00Z = 2026-07-25 23:00 à New York (UTC-4).
        let t = 1785034800.0;
        let (start, end) = day_bounds_local(t, -240);
        assert!(start <= t && t < end);
        // Minuit à New York le 25 = 04:00Z le 25.
        assert_eq!(start, 1784952000.0);
    }
}

// -------------------------------------------------------------- comptes

/// Vérifie le jeton porté par la requête, ou refuse.
async fn authenticate(
    state: &Arc<AppState>,
    headers: &HeaderMap,
) -> Result<auth::Identity, (StatusCode, String)> {
    let header = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok());
    state
        .auth
        .verify_header(header)
        .await
        .map_err(|e| (e.status(), e.message()))
}

#[derive(Serialize)]
struct UserResponse {
    uid: String,
    /// Absent tant que l'utilisateur n'a pas choisi son pseudo.
    #[serde(skip_serializing_if = "Option::is_none")]
    username: Option<String>,
}

async fn current_user(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Result<Json<UserResponse>, (StatusCode, String)> {
    let identity = authenticate(&state, &headers).await?;
    let record = db::user_by_uid(&state.pool, &identity.uid)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("PostGIS : {e}")))?;
    Ok(Json(UserResponse {
        uid: identity.uid,
        username: record.map(|r| r.username),
    }))
}

// MARK: - Liaison OpenStreetMap

#[derive(Serialize)]
struct OsmLinkResponse {
    linked: bool,
    /// Pseudo OSM, pour que l'app puisse dire à qui elle est reliée.
    #[serde(skip_serializing_if = "Option::is_none")]
    display_name: Option<String>,
    /// `client_id` et point d'autorisation, pour que l'app monte l'URL de
    /// consentement sans les embarquer en dur — les changer ne doit pas exiger
    /// une mise à jour App Store.
    #[serde(skip_serializing_if = "Option::is_none")]
    client_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    authorize_url: Option<String>,
    /// Vise-t-on le bac à sable plutôt que la vraie carte ? L'app le dit à
    /// l'écran : on ne doit jamais croire qu'on corrige OpenStreetMap quand on
    /// écrit dans une instance de test, ni l'inverse.
    sandbox: bool,
}

/// Où en est la liaison OSM du compte, et de quoi la démarrer.
async fn osm_link_status(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Result<Json<OsmLinkResponse>, (StatusCode, String)> {
    let identity = authenticate(&state, &headers).await?;
    let link = db::osm_link(&state.pool, &identity.uid)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("PostGIS : {e}")))?;

    Ok(Json(OsmLinkResponse {
        linked: link.is_some(),
        display_name: link.map(|l| l.display_name),
        client_id: osm_api::client_id(),
        authorize_url: osm_api::client_id()
            .map(|_| format!("{}/oauth2/authorize", osm_api::web_base())),
        sandbox: osm_api::is_sandbox(),
    }))
}

#[derive(Deserialize)]
struct OsmLinkBody {
    /// Code d'autorisation rendu par la page de consentement OSM.
    code: String,
    /// Pendant PKCE du `code_challenge` : c'est lui qui prouve que l'appareil
    /// qui termine l'échange est celui qui l'a commencé.
    code_verifier: String,
    /// Doit être identique à celui de la demande d'autorisation, l'API le
    /// revérifie.
    redirect_uri: String,
}

/// Termine la liaison : échange le code contre un jeton et le range.
///
/// L'échange se fait **ici** et pas sur l'appareil : le jeton d'écriture OSM
/// reste ainsi côté serveur, révocable d'un seul endroit, et c'est lui qui
/// pousse — y compris en différé, après un échec réseau.
async fn osm_link_account(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(body): Json<OsmLinkBody>,
) -> Result<Json<OsmLinkResponse>, (StatusCode, String)> {
    let identity = authenticate(&state, &headers).await?;
    if osm_api::client_id().is_none() {
        return Err((
            StatusCode::SERVICE_UNAVAILABLE,
            "liaison OSM non configurée sur ce serveur".into(),
        ));
    }

    let account = osm_api::exchange_code(
        &state.http,
        &body.code,
        &body.code_verifier,
        &body.redirect_uri,
    )
    .await
    .map_err(|e| (StatusCode::BAD_GATEWAY, e.to_string()))?;

    let link = db::OsmLink {
        user_id: account.user_id,
        display_name: account.display_name.clone(),
        access_token: account.access_token,
        refresh_token: account.refresh_token,
        expires_at: account
            .expires_in
            .map(|s| chrono::Utc::now() + chrono::Duration::seconds(s)),
    };
    let stored = db::link_osm_account(&state.pool, &identity.uid, &link)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("PostGIS : {e}")))?;
    if !stored {
        return Err((
            StatusCode::CONFLICT,
            "ce compte OpenStreetMap est déjà lié à un autre compte SunMap".into(),
        ));
    }

    println!("[osm] {} lié à {}", identity.uid, account.display_name);

    // Ce qui attendait un compte lié peut partir maintenant.
    let pool = state.pool.clone();
    let http = state.http.clone();
    tokio::spawn(async move { osm_push::drain(&pool, &http, 50).await });

    Ok(Json(OsmLinkResponse {
        linked: true,
        display_name: Some(account.display_name),
        client_id: osm_api::client_id(),
        authorize_url: None,
        sandbox: osm_api::is_sandbox(),
    }))
}

/// Détache le compte OSM. Le jeton part avec.
async fn osm_unlink_account(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Result<StatusCode, (StatusCode, String)> {
    let identity = authenticate(&state, &headers).await?;
    db::unlink_osm_account(&state.pool, &identity.uid)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("PostGIS : {e}")))?;
    println!("[osm] {} délié", identity.uid);
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
struct UsernameBody {
    username: String,
}

async fn set_username(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(body): Json<UsernameBody>,
) -> Result<Json<UserResponse>, (StatusCode, String)> {
    let identity = authenticate(&state, &headers).await?;
    let username = username::validate(&body.username)
        .map_err(|e| (StatusCode::BAD_REQUEST, e.message()))?;

    match db::set_username(&state.pool, &identity.uid, &username).await {
        Ok(Ok(record)) => Ok(Json(UserResponse {
            uid: record.uid,
            username: Some(record.username),
        })),
        // 409 et non 400 : la demande est bien formée, c'est l'état du monde qui
        // s'y oppose — et il peut s'y opposer entre deux tentatives identiques.
        Ok(Err(_)) => Err((StatusCode::CONFLICT, "pseudo déjà pris".into())),
        Err(e) => Err((StatusCode::INTERNAL_SERVER_ERROR, format!("PostGIS : {e}"))),
    }
}

#[derive(Deserialize)]
struct UsernameAvailabilityQuery {
    username: String,
}

#[derive(Serialize)]
struct UsernameAvailabilityResponse {
    available: bool,
    /// Pourquoi le pseudo est refusé — forme invalide ou déjà pris. `None`
    /// quand il est libre.
    #[serde(skip_serializing_if = "Option::is_none")]
    reason: Option<String>,
}

/// Le pseudo est-il libre ?
///
/// **Indicatif, jamais une réservation.** Entre cette réponse et le `PUT`,
/// quelqu'un d'autre peut prendre le même pseudo : c'est la contrainte
/// d'unicité en base qui tranche, et le 409 de `PUT /users/username` qui fait
/// foi. Cet endpoint n'existe que pour le confort de saisie — dire « déjà
/// pris » pendant qu'on tape vaut mieux que de le découvrir en validant.
///
/// Le compte connecté n'est pas exclu du test : redemander son propre pseudo
/// répond donc `available: false`. C'est voulu — l'app désactive de toute façon
/// la validation quand rien n'a changé, et prétendre le contraire ferait
/// croire à un renommage qui n'en est pas un.
async fn username_availability(
    State(state): State<Arc<AppState>>,
    Query(q): Query<UsernameAvailabilityQuery>,
) -> Result<Json<UsernameAvailabilityResponse>, (StatusCode, String)> {
    let username = match username::validate(&q.username) {
        Ok(name) => name,
        // 200 et non 400 : c'est une question, et « non, parce que la forme
        // est invalide » en est une réponse valable. Un 400 obligerait l'app à
        // traiter la saisie en cours comme une erreur réseau.
        Err(e) => {
            return Ok(Json(UsernameAvailabilityResponse {
                available: false,
                reason: Some(e.message()),
            }))
        }
    };

    let taken = db::user_by_username(&state.pool, &username)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("PostGIS : {e}")))?
        .is_some();

    Ok(Json(UsernameAvailabilityResponse {
        available: !taken,
        reason: taken.then(|| "pseudo déjà pris".to_string()),
    }))
}

#[derive(Serialize)]
struct SuggestionsResponse {
    suggestions: Vec<String>,
}

/// Quatre pseudos libres à proposer à la saisie.
///
/// Le nom d'affichage du fournisseur passe en premier quand il donne quelque
/// chose d'exploitable : « KarlGochgarian » se reconnaît mieux que
/// « SunLover3448 ». Le reste vient de la liste thématique.
async fn username_suggestions(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Result<Json<SuggestionsResponse>, (StatusCode, String)> {
    const WANTED: usize = 4;

    let identity = authenticate(&state, &headers).await?;

    let mut pool_of_candidates = Vec::new();
    if let Some(from_name) = identity.display_name.as_deref().and_then(username::from_display_name) {
        pool_of_candidates.push(from_name);
    }
    // Large marge : les pseudos pris sont écartés ensuite, et repartir en base
    // pour compléter coûterait un aller-retour de plus.
    let seed = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(1);
    pool_of_candidates.extend(username::candidates(seed, WANTED * 4));

    let taken = db::taken_usernames(&state.pool, &pool_of_candidates)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("PostGIS : {e}")))?;

    let suggestions: Vec<String> = pool_of_candidates
        .into_iter()
        .filter(|c| !taken.contains(&c.to_lowercase()))
        .take(WANTED)
        .collect();
    Ok(Json(SuggestionsResponse { suggestions }))
}

// ------------------------------------------------------------------ profils

/// Contributions jointes au profil : un aperçu, pas la liste.
///
/// Le profil sert à situer quelqu'un — son palier, son avancement — et quelques
/// lignes suffisent à l'illustrer. La liste complète a son propre écran et son
/// propre endpoint paginé (`/users/{…}/contributions`) : la charger d'office
/// ferait payer à chaque ouverture de profil un volume que presque personne ne
/// déroule.
const PROFILE_CONTRIBUTIONS_PREVIEW: i64 = 5;

/// Taille de page par défaut de la liste complète.
const CONTRIBUTIONS_PAGE_SIZE: i64 = 25;

/// Plafond dur d'une page, quoi que demande le client : au-delà, une requête
/// suffit à mobiliser la base pour un écran que personne ne lira d'un coup.
const CONTRIBUTIONS_MAX_PAGE_SIZE: i64 = 100;

#[derive(Serialize)]
struct ProfileResponse {
    username: String,
    /// Total réel, jamais plafonné : c'est lui qui décide du palier.
    contribution_count: i64,
    tier: TierPayload,
    /// Absent au sommet du barème — il n'y a alors plus rien à viser.
    #[serde(skip_serializing_if = "Option::is_none")]
    next_tier: Option<TierPayload>,
    /// Contributions restantes avant `next_tier`. `0` au sommet.
    remaining_to_next: i64,
    /// Avancement dans le palier courant, de 0 à 1.
    progress: f64,
    /// Aperçu seulement — `PROFILE_CONTRIBUTIONS_PREVIEW` lignes au plus. La
    /// suite se demande à `/users/{…}/contributions`.
    contributions: Vec<ContributionPayload>,
    /// Total **affichable**, celui que la liste paginée sait réellement
    /// atteindre. Il diffère de `contribution_count` quand un établissement a
    /// disparu d'OSM depuis la contribution : le palier compte celle-ci, la
    /// liste ne peut pas la montrer. C'est ce nombre-là qui décide d'afficher
    /// « Voir plus », sinon le bouton mènerait à une liste plus courte que
    /// promis.
    listable_count: i64,
}

#[derive(Serialize)]
struct TierPayload {
    /// Clé stable ("novice", "budding"…) : le client y accroche son icône et sa
    /// couleur sans dépendre d'un libellé traduit.
    key: &'static str,
    label: &'static str,
    /// Contributions nécessaires pour l'atteindre.
    threshold: i64,
}

impl TierPayload {
    fn of(tier: tiers::Tier, lang: Lang) -> TierPayload {
        TierPayload {
            key: tier.key(),
            label: tier.label(lang),
            threshold: tier.threshold(),
        }
    }
}

#[derive(Serialize)]
struct ContributionPayload {
    osm_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    amenity: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    category_label: Option<String>,
    /// Ce qui a été contribué : `terrace` ou `furniture`. Clé stable, à laquelle
    /// le client accroche son icône et son libellé.
    kind: &'static str,
    /// L'utilisateur a-t-il signalé une terrasse, ou son absence ? Les deux sont
    /// des contributions : dire « pas de terrasse » corrige la carte autant que
    /// l'inverse.
    ///
    /// Toujours sérialisé, `false` pour du mobilier, alors que le domaine le
    /// laisse absent dans ce cas : les clients d'avant `kind` le décodent en
    /// booléen obligatoire, et l'omettre ferait échouer chez eux le décodage de
    /// **tout** le profil. Ils liront « pas de terrasse » sur une ligne de
    /// mobilier — inexact, mais lisible, là où l'écran serait sinon vide.
    has_terrace: bool,
    lat: f64,
    lng: f64,
    updated_at: String,
}

#[derive(Deserialize)]
struct ProfileQuery {
    lang: Option<String>,
}

/// Assemble le profil d'un compte déjà identifié.
///
/// Partagé par le profil personnel et le profil public : les deux montrent
/// exactement la même chose. Rien de privé n'est en jeu — ni e-mail (le serveur
/// ne le stocke pas, il vit dans Firebase) ni identifiant Firebase, qui n'a
/// aucune raison de sortir. Ce qu'on publie, ce sont des contributions faites
/// pour être vues.
async fn build_profile(
    state: &Arc<AppState>,
    user: db::UserRecord,
    lang: Lang,
) -> Result<ProfileResponse, (StatusCode, String)> {
    let count = db::contribution_count(&state.pool, &user.uid)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("PostGIS : {e}")))?;
    let listable_count = db::listable_contribution_count(&state.pool, &user.uid)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("PostGIS : {e}")))?;
    let records =
        db::contributions_by_user(&state.pool, &user.uid, PROFILE_CONTRIBUTIONS_PREVIEW, 0)
            .await
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("PostGIS : {e}")))?;

    let progress = tiers::Progress::of(count);
    Ok(ProfileResponse {
        username: user.username,
        contribution_count: progress.count,
        tier: TierPayload::of(progress.tier, lang),
        next_tier: progress.next.map(|t| TierPayload::of(t, lang)),
        remaining_to_next: progress.remaining,
        progress: progress.fraction,
        contributions: records
            .into_iter()
            .map(|r| contribution_payload(r, lang))
            .collect(),
        listable_count,
    })
}

/// Conversion enregistrement → charge utile, partagée par le profil et la liste
/// paginée : deux formes différentes de la même ligne divergeraient au premier
/// champ ajouté.
fn contribution_payload(r: db::ContributionRecord, lang: Lang) -> ContributionPayload {
    ContributionPayload {
        category_label: r.amenity.as_deref().map(|a| i18n::amenity_label(a, lang)),
        osm_id: r.osm_id,
        name: r.name,
        amenity: r.amenity,
        kind: r.kind.key(),
        has_terrace: r.has_terrace.unwrap_or(false),
        lat: r.lat,
        lng: r.lng,
        updated_at: r.updated_at.to_rfc3339(),
    }
}

#[derive(Deserialize)]
struct ContributionsQuery {
    lang: Option<String>,
    /// Numéro de page, à partir de 1. Une valeur absurde (0, négative) est
    /// ramenée à la première page plutôt que refusée : c'est une liste, pas un
    /// virement.
    page: Option<i64>,
    per_page: Option<i64>,
}

impl ContributionsQuery {
    /// `(limit, offset)` assainis.
    fn window(&self) -> (i64, i64) {
        let per_page = self
            .per_page
            .unwrap_or(CONTRIBUTIONS_PAGE_SIZE)
            .clamp(1, CONTRIBUTIONS_MAX_PAGE_SIZE);
        let page = self.page.unwrap_or(1).max(1);
        (per_page, (page - 1) * per_page)
    }
}

#[derive(Serialize)]
struct ContributionsPage {
    items: Vec<ContributionPayload>,
    /// Total affichable, pour que le client sache dimensionner sans avoir à
    /// deviner d'après la dernière page reçue.
    total: i64,
    /// Épargne au client le calcul `offset + items.len() < total`, et surtout
    /// le fait de devoir le refaire juste s'il change de taille de page.
    has_more: bool,
}

/// Liste paginée des contributions d'un compte, du plus récent au plus ancien.
async fn contributions_page(
    state: &Arc<AppState>,
    uid: &str,
    q: &ContributionsQuery,
) -> Result<ContributionsPage, (StatusCode, String)> {
    let (limit, offset) = q.window();
    let lang = Lang::parse(q.lang.as_deref());
    let total = db::listable_contribution_count(&state.pool, uid)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("PostGIS : {e}")))?;
    let records = db::contributions_by_user(&state.pool, uid, limit, offset)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("PostGIS : {e}")))?;

    Ok(ContributionsPage {
        has_more: (offset + records.len() as i64) < total,
        items: records
            .into_iter()
            .map(|r| contribution_payload(r, lang))
            .collect(),
        total,
    })
}

/// Profil du compte connecté.
///
/// Distinct de `GET /users/me`, qui reste le strict nécessaire à l'amorçage de
/// la session (uid, pseudo) et que l'app appelle à chaque restauration : y
/// greffer deux requêtes de contributions ralentirait tous les lancements pour
/// un écran qu'on n'ouvre qu'à la demande.
async fn current_profile(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(q): Query<ProfileQuery>,
) -> Result<Json<ProfileResponse>, (StatusCode, String)> {
    let identity = authenticate(&state, &headers).await?;
    let user = db::user_by_uid(&state.pool, &identity.uid)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("PostGIS : {e}")))?
        .ok_or((
            StatusCode::NOT_FOUND,
            "aucun pseudo choisi pour ce compte".to_string(),
        ))?;
    let profile = build_profile(&state, user, Lang::parse(q.lang.as_deref())).await?;
    Ok(Json(profile))
}

/// Profil public d'un contributeur, désigné par son pseudo.
///
/// Sans authentification : c'est le pseudo affiché sous une terrasse signalée
/// qui y mène, et consulter la carte n'a jamais demandé de compte — exiger d'en
/// créer un pour savoir qui a contribué prendrait l'attribution à rebours.
async fn user_profile(
    State(state): State<Arc<AppState>>,
    axum::extract::Path(username): axum::extract::Path<String>,
    Query(q): Query<ProfileQuery>,
) -> Result<Json<ProfileResponse>, (StatusCode, String)> {
    let user = db::user_by_username(&state.pool, &username)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("PostGIS : {e}")))?
        .ok_or((StatusCode::NOT_FOUND, "pseudo inconnu".to_string()))?;
    let profile = build_profile(&state, user, Lang::parse(q.lang.as_deref())).await?;
    Ok(Json(profile))
}

/// Liste paginée des contributions du compte connecté.
async fn current_contributions(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(q): Query<ContributionsQuery>,
) -> Result<Json<ContributionsPage>, (StatusCode, String)> {
    let identity = authenticate(&state, &headers).await?;
    let page = contributions_page(&state, &identity.uid, &q).await?;
    Ok(Json(page))
}

/// Liste paginée des contributions d'un contributeur, désigné par son pseudo.
///
/// Sans authentification, comme le profil public qui y mène : ce sont les mêmes
/// contributions, faites pour être vues.
async fn user_contributions(
    State(state): State<Arc<AppState>>,
    axum::extract::Path(username): axum::extract::Path<String>,
    Query(q): Query<ContributionsQuery>,
) -> Result<Json<ContributionsPage>, (StatusCode, String)> {
    let user = db::user_by_username(&state.pool, &username)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("PostGIS : {e}")))?
        .ok_or((StatusCode::NOT_FOUND, "pseudo inconnu".to_string()))?;
    let page = contributions_page(&state, &user.uid, &q).await?;
    Ok(Json(page))
}

/// Supprime le compte connecté.
///
/// Ne touche qu'à **notre** base : l'identité Firebase, elle, est supprimée par
/// le client, seul à pouvoir le faire — le serveur ne fait que vérifier des
/// jetons signés (cf. `auth.rs`), il n'a pas de SDK Admin et n'appelle jamais
/// Firebase. L'ordre côté client est donc : cet appel d'abord, tant que le
/// jeton est valable, la suppression Firebase ensuite.
///
/// Idempotent : supprimer un compte déjà parti renvoie 204, pas 404. Un client
/// qui réessaie après une coupure réseau ne doit pas se voir refuser un état
/// qu'il a justement atteint.
async fn delete_current_user(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Result<StatusCode, (StatusCode, String)> {
    let identity = authenticate(&state, &headers).await?;
    let existed = db::delete_user(&state.pool, &identity.uid)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("PostGIS : {e}")))?;
    println!(
        "[account] suppression {} ({})",
        identity.uid,
        if existed { "compte effacé" } else { "déjà absent" }
    );
    Ok(StatusCode::NO_CONTENT)
}

// ---------------------------------------------------------------- geocode

/// Durée de vie d'une réponse de géocodage en cache. Longue à dessein : une
/// même saisie renvoie la même chose d'un jour à l'autre, et la politique
/// Nominatim demande expressément de mettre les résultats en cache.
const GEOCODE_CACHE_TTL: std::time::Duration = std::time::Duration::from_secs(24 * 3600);
/// Au-delà de cette taille, les entrées périmées sont purgées à l'insertion.
/// Borne molle : elle limite la mémoire sans stratégie d'éviction savante —
/// une réponse Nominatim pèse quelques Ko, le plafond vaut quelques Mo.
const GEOCODE_CACHE_MAX: usize = 4096;
/// Espacement minimal entre deux appels sortants vers Nominatim : la
/// politique impose « une requête par seconde au maximum », tous nos
/// utilisateurs confondus.
const GEOCODE_MIN_INTERVAL: std::time::Duration = std::time::Duration::from_secs(1);

#[derive(Deserialize)]
struct GeocodeQuery {
    q: String,
    /// `x1,y1,x2,y2` (lon,lat) — transmis tel quel à Nominatim, qui préfère
    /// alors les résultats dans cette emprise sans s'y restreindre.
    viewbox: Option<String>,
    /// Langues des libellés, au format `Accept-Language`.
    #[serde(rename = "accept-language")]
    accept_language: Option<String>,
    limit: Option<u32>,
}

/// `GET /geocode` — recherche libre d'adresse ou d'établissement, en
/// passe-plat vers Nominatim.
///
/// Un proxy et non un appel direct depuis l'app, pour trois raisons :
///
/// - **couper ou remplacer le service sans mise à jour de l'app** :
///   `GEOCODE_DISABLED=1` répond 503 à tout le monde ; `GEOCODE_UPSTREAM`
///   vise un autre service compatible (Nominatim auto-hébergé, Photon…) si le
///   volume dépasse un jour ce que le service public tolère ;
/// - **honorer la politique d'usage** (operations.osmfoundation.org/policies/
///   nominatim/) : l'espacement d'une seconde entre appels sortants se
///   garantit ici et nulle part ailleurs — pas dans une app distribuée —,
///   le User-Agent identifiant est celui du serveur, et les réponses sont
///   mises en cache comme demandé ;
/// - **garder le contrat client stable** si l'amont change.
///
/// La réponse est le JSON Nominatim (jsonv2) tel quel : le serveur ne fait
/// que transporter, le client porte le décodage. Le jour où l'amont change
/// de format, un mapping naîtra ici pour préserver ce contrat.
async fn geocode(
    State(state): State<Arc<AppState>>,
    Query(q): Query<GeocodeQuery>,
) -> Result<impl axum::response::IntoResponse, (StatusCode, String)> {
    use axum::http::header::CONTENT_TYPE;

    // Coupe-circuit : lu à chaque requête pour rester honnête avec la doc —
    // en pratique l'environnement d'un conteneur ne change qu'au redéploiement.
    if std::env::var("GEOCODE_DISABLED").is_ok_and(|v| v == "1") {
        return Err((StatusCode::SERVICE_UNAVAILABLE, "géocodage désactivé".into()));
    }

    let query = q.q.trim().to_string();
    if query.is_empty() {
        return Err((StatusCode::BAD_REQUEST, "q vide".into()));
    }
    let limit = q.limit.unwrap_or(10).clamp(1, 10);
    let viewbox = q.viewbox.unwrap_or_default();
    let lang = q.accept_language.unwrap_or_default();

    // La casse de la saisie ne change pas le géocodage : la clé la normalise
    // pour que « Café de Flore » et « café de flore » partagent l'entrée.
    let cache_key = format!("{}|{viewbox}|{lang}|{limit}", query.to_lowercase());
    if let Some((at, body)) = state.geocode_cache.read().await.get(&cache_key) {
        if at.elapsed() < GEOCODE_CACHE_TTL {
            println!("[geocode] « {query} » → cache hit");
            return Ok(([(CONTENT_TYPE, "application/json")], body.to_string()));
        }
    }

    let upstream = std::env::var("GEOCODE_UPSTREAM")
        .ok()
        .filter(|u| !u.is_empty())
        .unwrap_or_else(|| "https://nominatim.openstreetmap.org/search".into());

    let mut params: Vec<(&str, String)> = vec![
        ("q", query.clone()),
        ("format", "jsonv2".into()),
        ("addressdetails", "1".into()),
        ("dedupe", "1".into()),
        ("limit", limit.to_string()),
    ];
    if !viewbox.is_empty() {
        params.push(("viewbox", viewbox));
    }
    if !lang.is_empty() {
        params.push(("accept-language", lang));
    }

    // Le verrou se tient pendant l'attente ET l'appel : deux requêtes
    // concurrentes se sérialisent, l'espacement d'une seconde vaut donc pour
    // le serveur entier — c'est tout l'objet du proxy.
    let body = {
        let mut gate = state.geocode_gate.lock().await;
        if let Some(last) = *gate {
            let elapsed = last.elapsed();
            if elapsed < GEOCODE_MIN_INTERVAL {
                tokio::time::sleep(GEOCODE_MIN_INTERVAL - elapsed).await;
            }
        }
        *gate = Some(std::time::Instant::now());

        let resp = state
            .http
            .get(&upstream)
            .query(&params)
            .send()
            .await
            .map_err(|e| {
                (StatusCode::BAD_GATEWAY, format!("géocodage amont injoignable : {e}"))
            })?;
        if resp.status() != reqwest::StatusCode::OK {
            return Err((
                StatusCode::BAD_GATEWAY,
                format!("géocodage amont : HTTP {}", resp.status()),
            ));
        }
        resp.text().await.map_err(|e| {
            (StatusCode::BAD_GATEWAY, format!("géocodage amont : lecture — {e}"))
        })?
    };

    {
        let mut cache = state.geocode_cache.write().await;
        if cache.len() >= GEOCODE_CACHE_MAX {
            cache.retain(|_, (at, _)| at.elapsed() < GEOCODE_CACHE_TTL);
        }
        cache.insert(
            cache_key,
            (std::time::Instant::now(), Arc::new(body.clone())),
        );
    }
    println!("[geocode] « {query} » → amont, {} octets", body.len());
    Ok(([(CONTENT_TYPE, "application/json")], body))
}
