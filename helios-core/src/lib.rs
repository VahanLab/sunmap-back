//! helios-core — moteur soleil/ombre.
//!
//! Trois briques, zéro dépendance :
//! - [`sun`]    : position solaire (algorithme NOAA), précision ~0.01°
//! - [`dsm`]    : Digital Surface Model = terrain (DEM Terrarium) + bâtiments rasterisés
//! - [`shadow`] : ray marching sur la DSM — rendu de masque ET query ponctuelle
//!
//! Le même `is_shadowed` sert aux trois usages produit :
//! 1. rendu d'une tuile d'ombre (boucle sur les pixels)
//! 2. query "cette terrasse est-elle au soleil à 17h ?" (un seul rayon)
//! 3. cumul d'heures d'ensoleillement (boucle sur la journée)

pub mod dsm;
pub mod shadow;
pub mod sun;

pub use dsm::Dsm;
pub use shadow::{is_shadowed, render_mask, ShadowParams};
pub use sun::{sun_position, SunPosition};
