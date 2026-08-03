//! Code partagé entre le serveur (`main.rs`) et les binaires du pipeline
//! d'import (`bin/tilegen`, `bin/import`).
//!
//! La séparation matérialise le choix d'architecture : la géométrie va de
//! l'extrait OSM (`pbf`, règles `osm`) à l'archive vectorielle (`vtiles`),
//! le serveur la lit là — PostgreSQL (`db`) ne porte que le métier.

pub mod auth;
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
