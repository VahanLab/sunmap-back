//! Écriture dans OpenStreetMap au nom d'un contributeur.
//!
//! Deux moitiés :
//! - **OAuth 2.0** (échange du code d'autorisation, rafraîchissement). L'app
//!   ouvre la page de consentement et récupère le `code` ; c'est le serveur qui
//!   l'échange, pour que le jeton d'écriture ne transite jamais par l'appareil
//!   et reste révocable d'un seul endroit.
//! - **API 0.6** : ouvrir un changeset, y déposer un `osmChange`, le refermer.
//!
//! ## Ce qu'on écrit, et ce qu'on n'écrit pas
//!
//! Un compte OSM engage son porteur : chaque envoi porte un commentaire de
//! changeset explicite (`created_by`, `source`) pour que la communauté sache
//! d'où vient la modification et à qui s'adresser. On ne touche **jamais** aux
//! tags qu'on n'a pas demandés : la modification d'un élément existant relit sa
//! version courante et n'y ajoute que le tag concerné.

use std::collections::BTreeMap;

use serde::Deserialize;

/// Base de l'API. Surchargeable pour viser le bac à sable
/// (`https://api.dev.openstreetmap.org`), qui est **le** bon endroit pour
/// essayer : une erreur sur l'instance de production salit une base que des
/// milliers de gens relisent à la main.
pub fn api_base() -> String {
    std::env::var("OSM_API_BASE").unwrap_or_else(|_| "https://api.openstreetmap.org".to_string())
}

/// Base du site, qui sert les pages OAuth (distincte de l'API).
pub fn web_base() -> String {
    std::env::var("OSM_WEB_BASE").unwrap_or_else(|_| "https://www.openstreetmap.org".to_string())
}

pub fn client_id() -> Option<String> {
    std::env::var("OSM_CLIENT_ID").ok().filter(|s| !s.is_empty())
}

/// Secret de l'application OAuth. **Facultatif** : une application déclarée
/// « confidentielle » côté OSM en a un, une application publique s'en passe et
/// s'appuie sur PKCE. Les deux marchent, l'échange l'inclut s'il existe.
pub fn client_secret() -> Option<String> {
    std::env::var("OSM_CLIENT_SECRET").ok().filter(|s| !s.is_empty())
}

#[derive(Debug)]
pub enum OsmError {
    /// Aucun `OSM_CLIENT_ID` : la fonctionnalité est éteinte, pas cassée.
    NotConfigured,
    /// Le compte n'a pas lié OSM, ou son jeton a été révoqué de l'autre côté.
    NotLinked,
    Network(String),
    /// Réponse de l'API OSM hors 2xx : code et corps, tels quels.
    Api(u16, String),
}

impl std::fmt::Display for OsmError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotConfigured => write!(f, "OSM non configuré côté serveur"),
            Self::NotLinked => write!(f, "compte OSM non lié"),
            Self::Network(e) => write!(f, "réseau OSM : {e}"),
            Self::Api(code, body) => write!(f, "OSM {code} : {body}"),
        }
    }
}

/// Ce que l'échange OAuth rend.
pub struct LinkedAccount {
    pub access_token: String,
    pub refresh_token: Option<String>,
    pub expires_in: Option<i64>,
    pub user_id: i64,
    pub display_name: String,
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    refresh_token: Option<String>,
    expires_in: Option<i64>,
}

#[derive(Deserialize)]
struct UserDetails {
    user: UserDetailsUser,
}

#[derive(Deserialize)]
struct UserDetailsUser {
    id: i64,
    display_name: String,
}

/// Échange le code d'autorisation contre un jeton, puis lit qui vient de se
/// connecter.
///
/// `code_verifier` est le pendant PKCE du `code_challenge` envoyé par l'app :
/// il prouve que c'est bien le même appareil qui termine l'échange, et rend
/// inutile un code intercepté au passage du navigateur.
pub async fn exchange_code(
    http: &reqwest::Client,
    code: &str,
    code_verifier: &str,
    redirect_uri: &str,
) -> Result<LinkedAccount, OsmError> {
    let client_id = client_id().ok_or(OsmError::NotConfigured)?;

    let mut form: Vec<(&str, String)> = vec![
        ("grant_type", "authorization_code".into()),
        ("code", code.into()),
        ("redirect_uri", redirect_uri.into()),
        ("client_id", client_id),
        ("code_verifier", code_verifier.into()),
    ];
    if let Some(secret) = client_secret() {
        form.push(("client_secret", secret));
    }

    let token: TokenResponse = post_form(http, &format!("{}/oauth2/token", web_base()), &form).await?;
    let details = user_details(http, &token.access_token).await?;

    Ok(LinkedAccount {
        access_token: token.access_token,
        refresh_token: token.refresh_token,
        expires_in: token.expires_in,
        user_id: details.user.id,
        display_name: details.user.display_name,
    })
}

/// Renouvelle un jeton expiré. OSM délivre aujourd'hui des jetons sans
/// expiration, mais rien ne l'y oblige durablement.
pub async fn refresh_token(
    http: &reqwest::Client,
    refresh_token: &str,
) -> Result<LinkedAccount, OsmError> {
    let client_id = client_id().ok_or(OsmError::NotConfigured)?;
    let mut form: Vec<(&str, String)> = vec![
        ("grant_type", "refresh_token".into()),
        ("refresh_token", refresh_token.into()),
        ("client_id", client_id),
    ];
    if let Some(secret) = client_secret() {
        form.push(("client_secret", secret));
    }

    let token: TokenResponse = post_form(http, &format!("{}/oauth2/token", web_base()), &form).await?;
    let details = user_details(http, &token.access_token).await?;
    Ok(LinkedAccount {
        access_token: token.access_token,
        refresh_token: token.refresh_token,
        expires_in: token.expires_in,
        user_id: details.user.id,
        display_name: details.user.display_name,
    })
}

async fn user_details(
    http: &reqwest::Client,
    access_token: &str,
) -> Result<UserDetails, OsmError> {
    let url = format!("{}/api/0.6/user/details.json", api_base());
    let response = http
        .get(&url)
        .bearer_auth(access_token)
        .send()
        .await
        .map_err(|e| OsmError::Network(e.to_string()))?;
    let status = response.status();
    let body = response
        .text()
        .await
        .map_err(|e| OsmError::Network(e.to_string()))?;
    if !status.is_success() {
        return Err(OsmError::Api(status.as_u16(), body));
    }
    serde_json::from_str(&body).map_err(|e| OsmError::Api(status.as_u16(), e.to_string()))
}

async fn post_form<T: for<'de> Deserialize<'de>>(
    http: &reqwest::Client,
    url: &str,
    form: &[(&str, String)],
) -> Result<T, OsmError> {
    let response = http
        .post(url)
        .form(form)
        .send()
        .await
        .map_err(|e| OsmError::Network(e.to_string()))?;
    let status = response.status();
    let body = response
        .text()
        .await
        .map_err(|e| OsmError::Network(e.to_string()))?;
    if !status.is_success() {
        return Err(OsmError::Api(status.as_u16(), body));
    }
    serde_json::from_str(&body).map_err(|e| OsmError::Api(status.as_u16(), e.to_string()))
}

// MARK: - Écriture

/// Un élément OSM tel qu'on a besoin de le connaître pour le modifier :
/// sa version (l'API refuse une écriture sans elle), sa position, ses tags.
pub struct Element {
    pub kind: String,
    pub id: i64,
    pub version: i64,
    pub lat: Option<f64>,
    pub lon: Option<f64>,
    pub tags: BTreeMap<String, String>,
}

/// Lit un élément par son identifiant SunMap (`node/123`, `way/456`).
pub async fn fetch_element(
    http: &reqwest::Client,
    access_token: &str,
    osm_id: &str,
) -> Result<Element, OsmError> {
    let (kind, id) = split_osm_id(osm_id)?;
    let url = format!("{}/api/0.6/{kind}/{id}.json", api_base());
    let response = http
        .get(&url)
        .bearer_auth(access_token)
        .send()
        .await
        .map_err(|e| OsmError::Network(e.to_string()))?;
    let status = response.status();
    let body = response
        .text()
        .await
        .map_err(|e| OsmError::Network(e.to_string()))?;
    if !status.is_success() {
        return Err(OsmError::Api(status.as_u16(), body));
    }

    let parsed: serde_json::Value =
        serde_json::from_str(&body).map_err(|e| OsmError::Api(status.as_u16(), e.to_string()))?;
    let element = parsed["elements"]
        .get(0)
        .ok_or_else(|| OsmError::Api(status.as_u16(), "élément absent de la réponse".into()))?;

    let tags = element["tags"]
        .as_object()
        .map(|map| {
            map.iter()
                .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                .collect()
        })
        .unwrap_or_default();

    Ok(Element {
        kind: kind.to_string(),
        id,
        version: element["version"].as_i64().unwrap_or(1),
        lat: element["lat"].as_f64(),
        lon: element["lon"].as_f64(),
        tags,
    })
}

/// Ouvre un changeset et rend son identifiant.
///
/// Un changeset par contribution, et non un gros changeset par jour : c'est ce
/// que la communauté attend d'un éditeur tiers — une modification isolée, avec
/// son commentaire, se relit et se révoque sans toucher au reste.
pub async fn create_changeset(
    http: &reqwest::Client,
    access_token: &str,
    comment: &str,
) -> Result<i64, OsmError> {
    let xml = format!(
        r#"<osm><changeset>
  <tag k="created_by" v="SunMap"/>
  <tag k="comment" v="{}"/>
  <tag k="source" v="survey"/>
</changeset></osm>"#,
        escape_xml(comment)
    );
    let url = format!("{}/api/0.6/changeset/create", api_base());
    let response = http
        .put(&url)
        .bearer_auth(access_token)
        .header("Content-Type", "text/xml")
        .body(xml)
        .send()
        .await
        .map_err(|e| OsmError::Network(e.to_string()))?;
    let status = response.status();
    let body = response
        .text()
        .await
        .map_err(|e| OsmError::Network(e.to_string()))?;
    if !status.is_success() {
        return Err(OsmError::Api(status.as_u16(), body));
    }
    body.trim()
        .parse()
        .map_err(|_| OsmError::Api(status.as_u16(), format!("changeset illisible : {body}")))
}

pub async fn close_changeset(
    http: &reqwest::Client,
    access_token: &str,
    changeset: i64,
) -> Result<(), OsmError> {
    let url = format!("{}/api/0.6/changeset/{changeset}/close", api_base());
    let response = http
        .put(&url)
        .bearer_auth(access_token)
        .send()
        .await
        .map_err(|e| OsmError::Network(e.to_string()))?;
    if !response.status().is_success() {
        let status = response.status().as_u16();
        let body = response.text().await.unwrap_or_default();
        return Err(OsmError::Api(status, body));
    }
    Ok(())
}

/// Dépose un `osmChange` dans un changeset ouvert. Rend l'identifiant de
/// l'élément créé ou modifié.
async fn upload(
    http: &reqwest::Client,
    access_token: &str,
    changeset: i64,
    osm_change: &str,
) -> Result<String, OsmError> {
    let url = format!("{}/api/0.6/changeset/{changeset}/upload", api_base());
    let response = http
        .post(&url)
        .bearer_auth(access_token)
        .header("Content-Type", "text/xml")
        .body(osm_change.to_string())
        .send()
        .await
        .map_err(|e| OsmError::Network(e.to_string()))?;
    let status = response.status();
    let body = response
        .text()
        .await
        .map_err(|e| OsmError::Network(e.to_string()))?;
    if !status.is_success() {
        return Err(OsmError::Api(status.as_u16(), body));
    }
    Ok(body)
}

/// Ajoute (ou remplace) des tags sur un élément existant, sans toucher aux
/// autres.
///
/// L'élément est **relu juste avant** l'écriture : l'API refuse une
/// modification dont la version ne correspond plus, et écrire à l'aveugle
/// écraserait le travail de quelqu'un d'autre entre-temps.
pub async fn update_tags(
    http: &reqwest::Client,
    access_token: &str,
    osm_id: &str,
    tags: &[(String, String)],
    comment: &str,
) -> Result<(i64, String), OsmError> {
    let mut element = fetch_element(http, access_token, osm_id).await?;
    for (key, value) in tags {
        element.tags.insert(key.clone(), value.clone());
    }

    let changeset = create_changeset(http, access_token, comment).await?;
    let osm_change = format!(
        r#"<osmChange version="0.6" generator="SunMap"><modify>{}</modify></osmChange>"#,
        element_xml(&element, changeset)
    );
    let result = upload(http, access_token, changeset, &osm_change).await;
    // Le changeset se referme même après un échec d'upload : un changeset
    // laissé ouvert bloque les suivants du même compte pendant une heure.
    let _ = close_changeset(http, access_token, changeset).await;
    result?;
    Ok((changeset, osm_id.to_string()))
}

/// Crée un nœud avec ses tags. Rend `(changeset, "node/123")`.
pub async fn create_node(
    http: &reqwest::Client,
    access_token: &str,
    lat: f64,
    lon: f64,
    tags: &[(String, String)],
    comment: &str,
) -> Result<(i64, String), OsmError> {
    let changeset = create_changeset(http, access_token, comment).await?;
    let tags_xml: String = tags
        .iter()
        .map(|(k, v)| format!(r#"<tag k="{}" v="{}"/>"#, escape_xml(k), escape_xml(v)))
        .collect();
    // Identifiant négatif : c'est ainsi qu'on désigne un élément à créer, l'API
    // rend le vrai identifiant dans sa réponse.
    let osm_change = format!(
        r#"<osmChange version="0.6" generator="SunMap"><create><node id="-1" lat="{lat}" lon="{lon}" changeset="{changeset}" version="0">{tags_xml}</node></create></osmChange>"#
    );
    let result = upload(http, access_token, changeset, &osm_change).await;
    let _ = close_changeset(http, access_token, changeset).await;
    let body = result?;

    let new_id = parse_new_node_id(&body)
        .ok_or_else(|| OsmError::Api(200, format!("identifiant absent de la réponse : {body}")))?;
    Ok((changeset, format!("node/{new_id}")))
}

fn element_xml(element: &Element, changeset: i64) -> String {
    let tags: String = element
        .tags
        .iter()
        .map(|(k, v)| format!(r#"<tag k="{}" v="{}"/>"#, escape_xml(k), escape_xml(v)))
        .collect();
    let position = match (element.lat, element.lon) {
        (Some(lat), Some(lon)) => format!(r#" lat="{lat}" lon="{lon}""#),
        _ => String::new(),
    };
    format!(
        r#"<{kind} id="{id}" version="{version}" changeset="{changeset}"{position}>{tags}</{kind}>"#,
        kind = element.kind,
        id = element.id,
        version = element.version,
    )
}

/// `node/123` → `("node", 123)`.
pub fn split_osm_id(osm_id: &str) -> Result<(&str, i64), OsmError> {
    let (kind, id) = osm_id
        .split_once('/')
        .ok_or_else(|| OsmError::Api(400, format!("identifiant OSM mal formé : {osm_id}")))?;
    if !matches!(kind, "node" | "way" | "relation") {
        return Err(OsmError::Api(400, format!("type OSM inconnu : {kind}")));
    }
    let id = id
        .parse()
        .map_err(|_| OsmError::Api(400, format!("identifiant OSM mal formé : {osm_id}")))?;
    Ok((kind, id))
}

/// L'API répond un `diffResult` en XML ; on n'en a besoin que du `new_id` du
/// nœud créé. Un parseur XML complet pour un seul attribut serait disproportionné.
fn parse_new_node_id(body: &str) -> Option<i64> {
    let marker = "new_id=\"";
    let start = body.find(marker)? + marker.len();
    let end = body[start..].find('"')? + start;
    body[start..end].parse().ok()
}

fn escape_xml(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decoupe_un_identifiant_osm() {
        assert!(matches!(split_osm_id("node/123"), Ok(("node", 123))));
        assert!(matches!(split_osm_id("relation/9"), Ok(("relation", 9))));
        assert!(split_osm_id("user/42").is_err());
        assert!(split_osm_id("node").is_err());
        assert!(split_osm_id("node/abc").is_err());
    }

    #[test]
    fn lit_l_identifiant_du_noeud_cree() {
        let body = r#"<diffResult version="0.6" generator="OpenStreetMap server">
          <node old_id="-1" new_id="4295835911" new_version="1"/></diffResult>"#;
        assert_eq!(parse_new_node_id(body), Some(4_295_835_911));
        assert_eq!(parse_new_node_id("<diffResult/>"), None);
    }

    #[test]
    fn echappe_ce_qui_casserait_le_xml() {
        assert_eq!(escape_xml(r#"Chez "Jo" & fils"#), "Chez &quot;Jo&quot; &amp; fils");
    }

    #[test]
    fn serialise_un_element_avec_sa_version() {
        let element = Element {
            kind: "node".into(),
            id: 42,
            version: 7,
            lat: Some(48.85),
            lon: Some(2.35),
            tags: BTreeMap::from([("amenity".to_string(), "cafe".to_string())]),
        };
        let xml = element_xml(&element, 99);
        assert!(xml.contains(r#"id="42""#));
        // La version est ce qui protège du travail d'autrui : elle ne doit
        // jamais disparaître du XML.
        assert!(xml.contains(r#"version="7""#));
        assert!(xml.contains(r#"changeset="99""#));
        assert!(xml.contains(r#"<tag k="amenity" v="cafe"/>"#));
    }
}
