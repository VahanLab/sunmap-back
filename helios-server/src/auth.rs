//! Vérification des jetons d'identité Firebase.
//!
//! Firebase signe un JWT RS256 par session. Le serveur ne fait *que* le
//! vérifier : il n'appelle jamais Firebase pour valider une requête, ce qui
//! ajouterait un aller-retour réseau à chaque contribution et rendrait l'API
//! indisponible dès que Firebase l'est. Seules les clés publiques sont
//! récupérées, et elles tournent lentement.
//!
//! Ne jamais se contenter de décoder le jeton sans vérifier sa signature : la
//! charge utile d'un JWT est en clair, donc forgeable à volonté par n'importe
//! qui.

use std::collections::HashMap;
use std::sync::RwLock;
use std::time::{Duration, SystemTime};

use jsonwebtoken::{decode, decode_header, Algorithm, DecodingKey, Validation};
use serde::Deserialize;

/// Clés publiques de Firebase, au format JWK.
const JWKS_URL: &str =
    "https://www.googleapis.com/service_accounts/v1/jwk/securetoken@system.gserviceaccount.com";

#[derive(Debug, PartialEq, Eq)]
pub enum AuthError {
    /// En-tête absent ou mal formé.
    Missing,
    /// Signature invalide, jeton expiré, émetteur ou destinataire inattendu.
    Invalid(String),
    /// Clés publiques injoignables : on ne peut ni accepter ni infirmer.
    KeysUnavailable,
}

impl AuthError {
    /// 401 quand le jeton est en cause, 503 quand c'est nous : un client dont
    /// le jeton est bon ne doit pas être invité à se reconnecter parce que
    /// Google est momentanément injoignable.
    pub fn status(&self) -> axum::http::StatusCode {
        match self {
            Self::Missing | Self::Invalid(_) => axum::http::StatusCode::UNAUTHORIZED,
            Self::KeysUnavailable => axum::http::StatusCode::SERVICE_UNAVAILABLE,
        }
    }

    pub fn message(&self) -> String {
        match self {
            Self::Missing => "jeton d'identité absent".into(),
            Self::Invalid(why) => format!("jeton d'identité invalide : {why}"),
            Self::KeysUnavailable => "clés de vérification indisponibles".into(),
        }
    }
}

/// Ce qu'on retient d'un jeton vérifié.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Identity {
    /// `sub` : identifiant stable du compte, quel que soit le fournisseur.
    pub uid: String,
    /// Présent selon le fournisseur — Apple permet de le masquer.
    pub email: Option<String>,
}

#[derive(Debug, Deserialize)]
struct Claims {
    sub: String,
    email: Option<String>,
}

/// Vérificateur de jetons, avec ses clés en cache.
pub struct FirebaseAuth {
    project_id: String,
    keys: RwLock<KeyCache>,
}

#[derive(Default)]
struct KeyCache {
    /// `kid` → clé publique.
    keys: HashMap<String, DecodingKey>,
    /// Instant au-delà duquel les clés sont relues.
    expires_at: Option<SystemTime>,
}

impl FirebaseAuth {
    pub fn new(project_id: impl Into<String>) -> Self {
        Self {
            project_id: project_id.into(),
            keys: RwLock::new(KeyCache::default()),
        }
    }

    /// Vérifie un en-tête `Authorization: Bearer <jwt>`.
    pub async fn verify_header(&self, header: Option<&str>) -> Result<Identity, AuthError> {
        let token = header
            .and_then(|h| h.strip_prefix("Bearer "))
            .map(str::trim)
            .filter(|t| !t.is_empty())
            .ok_or(AuthError::Missing)?;
        self.verify(token).await
    }

    pub async fn verify(&self, token: &str) -> Result<Identity, AuthError> {
        let kid = decode_header(token)
            .map_err(|e| AuthError::Invalid(format!("en-tête illisible ({e})")))?
            .kid
            .ok_or_else(|| AuthError::Invalid("en-tête sans `kid`".into()))?;

        // Deux essais : un `kid` inconnu signifie souvent que Firebase a fait
        // tourner ses clés depuis notre dernière lecture. Les relire une fois
        // évite de rejeter à tort des jetons parfaitement valides.
        if let Some(key) = self.cached_key(&kid) {
            return verify_with(token, &key, &self.project_id);
        }
        self.refresh_keys().await?;
        let key = self
            .cached_key(&kid)
            .ok_or_else(|| AuthError::Invalid(format!("clé `{kid}` inconnue")))?;
        verify_with(token, &key, &self.project_id)
    }

    fn cached_key(&self, kid: &str) -> Option<DecodingKey> {
        let cache = self.keys.read().ok()?;
        match cache.expires_at {
            Some(at) if at > SystemTime::now() => cache.keys.get(kid).cloned(),
            _ => None,
        }
    }

    async fn refresh_keys(&self) -> Result<(), AuthError> {
        #[derive(Deserialize)]
        struct Jwks {
            keys: Vec<Jwk>,
        }
        #[derive(Deserialize)]
        struct Jwk {
            kid: String,
            n: String,
            e: String,
        }

        let response = reqwest::get(JWKS_URL)
            .await
            .map_err(|_| AuthError::KeysUnavailable)?;
        // Google indique lui-même combien de temps ses clés restent valables.
        let ttl = response
            .headers()
            .get(reqwest::header::CACHE_CONTROL)
            .and_then(|v| v.to_str().ok())
            .and_then(max_age)
            .unwrap_or(3_600);
        let jwks: Jwks = response
            .json()
            .await
            .map_err(|_| AuthError::KeysUnavailable)?;

        let keys = jwks
            .keys
            .into_iter()
            .filter_map(|k| {
                DecodingKey::from_rsa_components(&k.n, &k.e)
                    .ok()
                    .map(|key| (k.kid, key))
            })
            .collect::<HashMap<_, _>>();
        if keys.is_empty() {
            return Err(AuthError::KeysUnavailable);
        }

        if let Ok(mut cache) = self.keys.write() {
            cache.keys = keys;
            cache.expires_at = Some(SystemTime::now() + Duration::from_secs(ttl));
        }
        Ok(())
    }
}

/// Cœur de la vérification, sans réseau : une clé, un jeton, un verdict. C'est
/// ce qui le rend testable hors ligne.
fn verify_with(token: &str, key: &DecodingKey, project_id: &str) -> Result<Identity, AuthError> {
    let mut validation = Validation::new(Algorithm::RS256);
    // Un jeton émis pour un autre projet Firebase est signé par les mêmes clés
    // Google : sans ces deux contrôles, n'importe quel projet tiers pourrait
    // authentifier chez nous.
    validation.set_audience(&[project_id]);
    validation.set_issuer(&[format!("https://securetoken.google.com/{project_id}")]);
    validation.set_required_spec_claims(&["exp", "aud", "iss", "sub"]);
    // Aucune tolérance sur l'expiration : les jetons Firebase durent une heure
    // et le client les renouvelle tout seul, il n'y a rien à rattraper.
    validation.leeway = 0;

    // `exp` est vérifié ici par `jsonwebtoken`, contre l'horloge système.
    let data = decode::<Claims>(token, key, &validation)
        .map_err(|e| AuthError::Invalid(e.to_string()))?;

    if data.claims.sub.is_empty() {
        return Err(AuthError::Invalid("`sub` vide".into()));
    }
    Ok(Identity {
        uid: data.claims.sub,
        email: data.claims.email,
    })
}

/// `max-age` d'un en-tête `Cache-Control`, en secondes.
fn max_age(header: &str) -> Option<u64> {
    header.split(',').find_map(|part| {
        part.trim()
            .strip_prefix("max-age=")
            .and_then(|v| v.parse().ok())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use jsonwebtoken::{encode, EncodingKey, Header};
    use serde::Serialize;
    use std::time::UNIX_EPOCH;

    /// Paire de clés de test, générée une fois pour ce fichier. Rien de secret :
    /// elle ne signe que des jetons de test, jamais rien qui atteigne Firebase.
    const TEST_KEY_PEM: &str = include_str!("testdata/test_key.pem");
    /// Modules `n` et exposant `e` de la même clé, en base64url — le format sous
    /// lequel Google publie les siennes.
    const TEST_KEY_JWK: &str = include_str!("testdata/test_key.jwk.txt");

    const PROJECT: &str = "sunmap-d06df";

    #[derive(Serialize)]
    struct TestClaims {
        sub: String,
        aud: String,
        iss: String,
        exp: u64,
        #[serde(skip_serializing_if = "Option::is_none")]
        email: Option<String>,
    }

    fn now() -> u64 {
        SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs()
    }

    fn decoding_key() -> DecodingKey {
        let mut lines = TEST_KEY_JWK.lines();
        let n = lines.next().unwrap();
        let e = lines.next().unwrap();
        DecodingKey::from_rsa_components(n, e).unwrap()
    }

    fn sign(claims: TestClaims) -> String {
        let mut header = Header::new(Algorithm::RS256);
        header.kid = Some("test-kid".into());
        let key = EncodingKey::from_rsa_pem(TEST_KEY_PEM.as_bytes()).unwrap();
        encode(&header, &claims, &key).unwrap()
    }

    fn claims() -> TestClaims {
        TestClaims {
            sub: "uid-abc".into(),
            aud: PROJECT.into(),
            iss: format!("https://securetoken.google.com/{PROJECT}"),
            exp: now() + 3_600,
            email: Some("karl@example.com".into()),
        }
    }

    #[test]
    fn jeton_valide_donne_l_identite() {
        let identity = verify_with(&sign(claims()), &decoding_key(), PROJECT).unwrap();
        assert_eq!(identity.uid, "uid-abc");
        assert_eq!(identity.email.as_deref(), Some("karl@example.com"));
    }

    /// Le cas qui compte le plus : les jetons de *tous* les projets Firebase sont
    /// signés par les mêmes clés Google. Sans contrôle du destinataire, n'importe
    /// quel projet tiers authentifierait chez nous.
    #[test]
    fn jeton_d_un_autre_projet_refuse() {
        let mut c = claims();
        c.aud = "un-autre-projet".into();
        assert!(matches!(
            verify_with(&sign(c), &decoding_key(), PROJECT),
            Err(AuthError::Invalid(_))
        ));
    }

    #[test]
    fn emetteur_inattendu_refuse() {
        let mut c = claims();
        c.iss = "https://evil.example.com/".into();
        assert!(matches!(
            verify_with(&sign(c), &decoding_key(), PROJECT),
            Err(AuthError::Invalid(_))
        ));
    }

    #[test]
    fn jeton_expire_refuse() {
        let mut c = claims();
        c.exp = now() - 1;
        assert!(matches!(
            verify_with(&sign(c), &decoding_key(), PROJECT),
            Err(AuthError::Invalid(_))
        ));
    }

    /// Une charge utile de JWT est en clair : sans vérification de signature,
    /// n'importe qui se déclarerait n'importe qui.
    #[test]
    fn signature_falsifiee_refusee() {
        let token = sign(claims());
        let mut altered = token.clone();
        let last = altered.pop().unwrap();
        altered.push(if last == 'A' { 'B' } else { 'A' });
        assert!(matches!(
            verify_with(&altered, &decoding_key(), PROJECT),
            Err(AuthError::Invalid(_))
        ));
    }

    #[test]
    fn en_tete_sans_bearer_refuse() {
        let auth = FirebaseAuth::new(PROJECT);
        let rt = tokio::runtime::Builder::new_current_thread().build().unwrap();
        assert_eq!(rt.block_on(auth.verify_header(None)), Err(AuthError::Missing));
        assert_eq!(
            rt.block_on(auth.verify_header(Some("Basic abc"))),
            Err(AuthError::Missing)
        );
        assert_eq!(
            rt.block_on(auth.verify_header(Some("Bearer   "))),
            Err(AuthError::Missing)
        );
    }

    #[test]
    fn max_age_lu_parmi_les_directives() {
        assert_eq!(max_age("public, max-age=19860, must-revalidate"), Some(19_860));
        assert_eq!(max_age("no-store"), None);
    }
}
