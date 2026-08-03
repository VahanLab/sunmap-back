//! Code partagé entre le serveur (`main.rs`) et le binaire d'ingestion
//! (`bin/ingest.rs`).
//!
//! La séparation matérialise le choix d'architecture : Overpass n'est plus sur
//! le chemin d'une requête client (`osm`), il ne sert qu'à remplir PostGIS, et
//! le serveur ne lit plus que la base (`db`).

pub mod auth;
pub mod btiles;
pub mod canopy_tiles;
pub mod db;
pub mod dem;
pub mod i18n;
pub mod opening_hours;
pub mod osm;
pub mod osm_api;
pub mod osm_push;
pub mod pbf;
pub mod tiers;
pub mod username;
pub mod vtiles;
