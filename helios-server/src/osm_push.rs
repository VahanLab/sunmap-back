//! Traduction d'une contribution SunMap en modification OpenStreetMap, et
//! vidage de la file d'attente.
//!
//! Les tags écrits suivent l'usage OSM établi :
//! - terrasse → `outdoor_seating=yes|no` sur l'établissement existant ;
//! - banc → nœud `amenity=bench`, avec `backrest` et `direction` ;
//! - table → nœud `leisure=picnic_table`.
//!
//! `direction` est écrit en **degrés depuis le nord**, sens horaire : c'est la
//! convention OSM, et c'est déjà celle de l'app (cf. la contribution de
//! mobilier). Aucune conversion, donc aucune occasion de se tromper de repère.

use std::sync::Arc;

use serde::{Deserialize, Serialize};
use sqlx::PgPool;

use crate::db;
use crate::osm_api::{self, OsmError};

/// Ce qu'on a promis d'envoyer, figé au moment de la contribution.
///
/// Rejouer un envoi doit rejouer **ce qui a été demandé**, pas l'état courant
/// de la base : entre-temps quelqu'un d'autre a pu corriger la même terrasse,
/// et renvoyer sa valeur au nom du premier contributeur serait un faux.
#[derive(Serialize, Deserialize, Debug)]
#[serde(tag = "kind")]
pub enum PushPayload {
    #[serde(rename = "terrace")]
    Terrace { has_terrace: bool },
    #[serde(rename = "bench")]
    Bench {
        lat: f64,
        lng: f64,
        direction_deg: Option<f64>,
        backrest: Option<bool>,
    },
    #[serde(rename = "picnic_table")]
    PicnicTable {
        lat: f64,
        lng: f64,
        direction_deg: Option<f64>,
    },
}

impl PushPayload {
    /// Nom de la variante, tel qu'il est rangé en base.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Terrace { .. } => "terrace",
            Self::Bench { .. } => "bench",
            Self::PicnicTable { .. } => "picnic_table",
        }
    }

    fn changeset_comment(&self) -> &'static str {
        match self {
            Self::Terrace { has_terrace: true } => "Terrasse signalée (SunMap)",
            Self::Terrace { has_terrace: false } => "Absence de terrasse signalée (SunMap)",
            Self::Bench { .. } => "Ajout d'un banc (SunMap)",
            Self::PicnicTable { .. } => "Ajout d'une table de pique-nique (SunMap)",
        }
    }
}

/// Envoie une contribution. Rend `(changeset, élément)`.
pub async fn push(
    http: &reqwest::Client,
    access_token: &str,
    place_id: &str,
    payload: &PushPayload,
) -> Result<(i64, String), OsmError> {
    let comment = payload.changeset_comment();
    match payload {
        PushPayload::Terrace { has_terrace } => {
            // Un meuble ajouté depuis l'app porte un identifiant synthétique
            // `user/…` : il n'existe pas dans OSM, il n'y a rien à modifier.
            let tags = vec![(
                "outdoor_seating".to_string(),
                if *has_terrace { "yes" } else { "no" }.to_string(),
            )];
            osm_api::update_tags(http, access_token, place_id, &tags, comment).await
        }
        PushPayload::Bench {
            lat,
            lng,
            direction_deg,
            backrest,
        } => {
            let mut tags = vec![("amenity".to_string(), "bench".to_string())];
            if let Some(backrest) = backrest {
                tags.push((
                    "backrest".to_string(),
                    if *backrest { "yes" } else { "no" }.to_string(),
                ));
            }
            if let Some(direction) = direction_deg {
                tags.push(("direction".to_string(), format_direction(*direction)));
            }
            osm_api::create_node(http, access_token, *lat, *lng, &tags, comment).await
        }
        PushPayload::PicnicTable {
            lat,
            lng,
            direction_deg,
        } => {
            let mut tags = vec![("leisure".to_string(), "picnic_table".to_string())];
            if let Some(direction) = direction_deg {
                tags.push(("direction".to_string(), format_direction(*direction)));
            }
            osm_api::create_node(http, access_token, *lat, *lng, &tags, comment).await
        }
    }
}

/// Cap ramené dans [0, 360[ et arrondi au degré.
///
/// OSM accepte le décimal, mais un cap relevé au doigt sur un curseur n'a pas
/// la précision qui justifierait des décimales — les écrire donnerait une
/// fausse impression d'exactitude à qui relit la donnée.
fn format_direction(degrees: f64) -> String {
    let normalized = degrees.rem_euclid(360.0).round() as i64 % 360;
    normalized.to_string()
}

/// Vide la file d'attente : chaque envoi en attente est retenté une fois.
///
/// Appelée après chaque contribution et au démarrage. Les échecs restent en
/// file — l'API OSM peut être en maintenance, et une contribution ne doit pas
/// se perdre pour autant.
pub async fn drain(pool: &PgPool, http: &reqwest::Client, limit: i64) {
    let pending = match db::pending_osm_pushes(pool, limit).await {
        Ok(rows) => rows,
        Err(e) => {
            eprintln!("[osm] file illisible : {e}");
            return;
        }
    };
    if pending.is_empty() {
        return;
    }

    for item in pending {
        let payload: PushPayload = match serde_json::from_value(item.payload.clone()) {
            Ok(payload) => payload,
            Err(e) => {
                // Charge utile illisible : inutile de réessayer, elle ne se
                // réparera pas.
                let _ = db::mark_osm_push_failed(pool, item.id, &format!("payload : {e}")).await;
                continue;
            }
        };

        let token = match access_token(pool, http, &item.user_uid).await {
            Ok(Some(token)) => token,
            Ok(None) => {
                let _ = db::mark_osm_push_failed(pool, item.id, "compte OSM délié").await;
                continue;
            }
            Err(e) => {
                let _ = db::mark_osm_push_failed(pool, item.id, &e.to_string()).await;
                continue;
            }
        };

        match push(http, &token, &item.place_id, &payload).await {
            Ok((changeset, element)) => {
                println!(
                    "[osm] {} {} → {element} (changeset {changeset})",
                    item.kind, item.place_id
                );
                let _ = db::mark_osm_push_sent(pool, item.id, changeset, &element).await;
            }
            Err(e) => {
                eprintln!("[osm] échec {} {} : {e}", item.kind, item.place_id);
                let _ = db::mark_osm_push_failed(pool, item.id, &e.to_string()).await;
            }
        }
    }
}

/// Jeton d'écriture du compte, rafraîchi si nécessaire.
async fn access_token(
    pool: &PgPool,
    http: &reqwest::Client,
    uid: &str,
) -> Result<Option<String>, OsmError> {
    let link = db::osm_link(pool, uid)
        .await
        .map_err(|e| OsmError::Network(e.to_string()))?;
    let Some(link) = link else { return Ok(None) };

    let expired = link
        .expires_at
        .is_some_and(|at| at <= chrono::Utc::now() + chrono::Duration::minutes(1));
    if !expired {
        return Ok(Some(link.access_token));
    }

    let Some(refresh) = link.refresh_token.clone() else {
        return Err(OsmError::NotLinked);
    };
    let renewed = osm_api::refresh_token(http, &refresh).await?;
    let stored = db::OsmLink {
        user_id: renewed.user_id,
        display_name: renewed.display_name,
        access_token: renewed.access_token.clone(),
        refresh_token: renewed.refresh_token,
        expires_at: renewed
            .expires_in
            .map(|s| chrono::Utc::now() + chrono::Duration::seconds(s)),
    };
    db::update_osm_token(pool, uid, &stored)
        .await
        .map_err(|e| OsmError::Network(e.to_string()))?;
    Ok(Some(renewed.access_token))
}

/// Met une contribution en file, puis tente de la faire partir tout de suite.
///
/// L'envoi part dans une tâche détachée : la réponse HTTP au contributeur ne
/// doit pas attendre l'API OSM, qui met parfois plusieurs secondes à ouvrir un
/// changeset. En cas d'échec, l'envoi reste en file.
pub fn enqueue_and_spawn(
    pool: PgPool,
    http: reqwest::Client,
    uid: String,
    place_id: String,
    payload: PushPayload,
) {
    tokio::spawn(async move {
        let kind = payload.kind();
        let value = match serde_json::to_value(&payload) {
            Ok(value) => value,
            Err(e) => {
                eprintln!("[osm] payload insérialisable : {e}");
                return;
            }
        };
        if let Err(e) = db::enqueue_osm_push(&pool, &uid, kind, &place_id, value).await {
            eprintln!("[osm] mise en file impossible : {e}");
            return;
        }
        drain(&pool, &http, 10).await;
    });
}

/// Y a-t-il de quoi pousser ? Sert à ne rien mettre en file quand la
/// fonctionnalité n'est pas configurée côté serveur.
pub fn is_configured() -> bool {
    osm_api::client_id().is_some()
}

/// Boucle de rattrapage : reprend périodiquement ce qui a échoué.
pub fn spawn_retry_loop(pool: PgPool, http: reqwest::Client) {
    if !is_configured() {
        return;
    }
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(std::time::Duration::from_secs(300));
        loop {
            ticker.tick().await;
            drain(&pool, &http, 50).await;
        }
    });
}

/// Sert au partage du client HTTP sans dupliquer l'`AppState`.
pub type Shared<T> = Arc<T>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalise_le_cap() {
        assert_eq!(format_direction(0.0), "0");
        assert_eq!(format_direction(181.4), "181");
        assert_eq!(format_direction(359.6), "0");
        // Un cap négatif est un cap : -90° vaut 270°, pas une erreur.
        assert_eq!(format_direction(-90.0), "270");
        assert_eq!(format_direction(450.0), "90");
    }

    #[test]
    fn le_payload_survit_a_un_aller_retour_json() {
        let payload = PushPayload::Bench {
            lat: 48.85,
            lng: 2.35,
            direction_deg: Some(90.0),
            backrest: Some(false),
        };
        let value = serde_json::to_value(&payload).unwrap();
        assert_eq!(value["kind"], "bench");
        let back: PushPayload = serde_json::from_value(value).unwrap();
        match back {
            PushPayload::Bench { backrest, .. } => assert_eq!(backrest, Some(false)),
            other => panic!("variante inattendue : {other:?}"),
        }
    }

    #[test]
    fn le_commentaire_distingue_presence_et_absence() {
        let yes = PushPayload::Terrace { has_terrace: true };
        let no = PushPayload::Terrace { has_terrace: false };
        assert_ne!(yes.changeset_comment(), no.changeset_comment());
        assert_eq!(yes.kind(), "terrace");
    }
}
