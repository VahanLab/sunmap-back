//! Pseudos : validation et suggestions.
//!
//! La forme est validée ici *et* par une contrainte `CHECK` en base. Le double
//! emploi est voulu : le serveur sait dire pourquoi un pseudo est refusé, la
//! base garantit qu'aucun chemin — import, correction à la main, futur client —
//! n'y échappe.

/// Bornes du pseudo, à garder identiques à la contrainte `username_shape` de
/// `schema.sql`.
pub const MIN_LEN: usize = 3;
pub const MAX_LEN: usize = 20;

#[derive(Debug, PartialEq, Eq)]
pub enum UsernameError {
    TooShort,
    TooLong,
    BadCharacters,
}

impl UsernameError {
    pub fn message(&self) -> String {
        match self {
            Self::TooShort => format!("au moins {MIN_LEN} caractères"),
            Self::TooLong => format!("au plus {MAX_LEN} caractères"),
            Self::BadCharacters => {
                "lettres, chiffres, tiret bas et point uniquement".into()
            }
        }
    }
}

/// Valide la forme d'un pseudo et renvoie sa version nettoyée.
///
/// Seuls les espaces de bordure sont retirés : le reste est refusé plutôt que
/// corrigé en silence, pour que l'utilisateur voie le pseudo qu'il aura vraiment.
pub fn validate(raw: &str) -> Result<String, UsernameError> {
    let trimmed = raw.trim();
    // Compté en `char` et non en octets : « é » pèse deux octets et ne doit pas
    // consommer deux caractères du quota. Il sera refusé plus bas, mais avec le
    // bon motif.
    let len = trimmed.chars().count();
    if len < MIN_LEN {
        return Err(UsernameError::TooShort);
    }
    if len > MAX_LEN {
        return Err(UsernameError::TooLong);
    }
    if !trimmed
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '.')
    {
        return Err(UsernameError::BadCharacters);
    }
    Ok(trimmed.to_string())
}

/// Racines proposées, sur le thème de l'app. Mélange français et anglais : c'est
/// ce qu'on lit sur les pseudos réels, et ça double le vivier sans allonger la
/// liste.
///
/// Toutes tiennent en 16 caractères, pour qu'un suffixe de quatre chiffres laisse
/// le pseudo sous la limite.
const ROOTS: &[&str] = &[
    "SunSeeker",
    "ShadowSeeker",
    "SunLover",
    "ShadowLover",
    "ViveLeSoleil",
    "ChasseurSoleil",
    "PleinSoleil",
    "CoinDOmbre",
    "RayonDeSoleil",
    "TerrasseAuSoleil",
    "AmiDuSoleil",
    "SunChaser",
    "ShadeHunter",
    "SoleilDeMidi",
    "BainDeSoleil",
    "OmbreEtSoleil",
    "SunSpotter",
    "TerrasseFinder",
    "MidiAuSoleil",
    "AperoAuSoleil",
];

/// Suggestions de pseudos, racine du thème + nombre.
///
/// Déterministe pour une graine donnée : c'est ce qui rend la génération
/// testable, et ce qui permet de rejouer une série si besoin. L'appelant tire la
/// graine de l'horloge.
///
/// Ne dit rien de la disponibilité — c'est à la base de trancher. En produire
/// plus que nécessaire permet justement d'en écarter sans revenir à zéro.
pub fn candidates(seed: u64, count: usize) -> Vec<String> {
    let mut rng = Xorshift(seed | 1);
    let mut out = Vec::with_capacity(count);
    while out.len() < count {
        let root = ROOTS[(rng.next() % ROOTS.len() as u64) as usize];
        // Trois ou quatre chiffres : deux se ressemblent trop d'un utilisateur à
        // l'autre, cinq donnent un pseudo qui a l'air d'un numéro de série.
        let digits = 100 + rng.next() % 9_900;
        let candidate = format!("{root}{digits}");
        if !out.contains(&candidate) {
            out.push(candidate);
        }
    }
    out
}

/// Xorshift64 : suffisant pour proposer des pseudos, et évite une dépendance
/// pour ce seul usage. Rien ici n'a besoin d'être imprévisible.
struct Xorshift(u64);

impl Xorshift {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formes_acceptees() {
        assert_eq!(validate("karl").unwrap(), "karl");
        assert_eq!(validate("  Karl_2.0  ").unwrap(), "Karl_2.0");
        assert_eq!(validate("aaa").unwrap(), "aaa");
        assert_eq!(validate(&"a".repeat(20)).unwrap().len(), 20);
    }

    #[test]
    fn formes_refusees() {
        assert_eq!(validate("ka"), Err(UsernameError::TooShort));
        assert_eq!(validate(&"a".repeat(21)), Err(UsernameError::TooLong));
        assert_eq!(validate("karl g"), Err(UsernameError::BadCharacters));
        assert_eq!(validate("karl-g"), Err(UsernameError::BadCharacters));
        assert_eq!(validate("café"), Err(UsernameError::BadCharacters));
        assert_eq!(validate("🙂🙂🙂"), Err(UsernameError::BadCharacters));
    }

    /// Un accent pèse deux octets : compté ainsi, « ét » ferait trois et
    /// passerait la longueur minimale.
    #[test]
    fn longueur_comptee_en_caracteres() {
        assert_eq!(validate("ét"), Err(UsernameError::TooShort));
    }

    /// Le contrat qui compte : une suggestion doit toujours être acceptable,
    /// sinon l'app en propose une que la base refusera.
    #[test]
    fn toute_suggestion_est_valide() {
        for seed in 1..200u64 {
            for candidate in candidates(seed, 4) {
                assert_eq!(validate(&candidate).as_deref(), Ok(candidate.as_str()),
                           "suggestion invalide : {candidate}");
            }
        }
    }

    #[test]
    fn suggestions_distinctes_et_en_nombre_demande() {
        let list = candidates(42, 4);
        assert_eq!(list.len(), 4);
        let mut sorted = list.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(sorted.len(), 4, "doublon dans {list:?}");
    }

    #[test]
    fn generation_deterministe() {
        assert_eq!(candidates(7, 4), candidates(7, 4));
        assert_ne!(candidates(7, 4), candidates(8, 4));
    }
}
