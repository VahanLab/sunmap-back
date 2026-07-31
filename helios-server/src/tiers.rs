//! Paliers de contribution.
//!
//! Signaler une terrasse ne rapporte rien à celui qui le fait : il donne une
//! information dont profitent les autres. Les paliers sont la seule contrepartie
//! qu'on puisse offrir sans fausser la donnée — reconnaître l'effort, sans
//! jamais récompenser le volume au point d'inviter à contribuer n'importe quoi.
//!
//! Le calcul vit **côté serveur** et non dans chaque client : les seuils vont
//! bouger avec l'usage réel, et une app déjà installée continuerait d'afficher
//! les anciens. Android n'aura rien à recopier non plus.

use crate::i18n::Lang;

/// Palier atteint, du premier signalement au contributeur de référence.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Tier {
    Novice,
    Budding,
    Established,
    Innovator,
    Influential,
}

impl Tier {
    /// Dans l'ordre, du plus bas au plus haut.
    pub const ALL: [Tier; 5] = [
        Tier::Novice,
        Tier::Budding,
        Tier::Established,
        Tier::Innovator,
        Tier::Influential,
    ];

    /// Nombre de contributions à partir duquel le palier est atteint.
    ///
    /// Premier saut volontairement court (3) : c'est celui qui décide si
    /// quelqu'un contribue une deuxième fois. Les suivants s'écartent, pour que
    /// le haut du barème garde du sens — un palier atteint en une soirée ne
    /// distinguerait personne.
    pub fn threshold(&self) -> i64 {
        match self {
            Tier::Novice => 0,
            Tier::Budding => 3,
            Tier::Established => 10,
            Tier::Innovator => 25,
            Tier::Influential => 60,
        }
    }

    /// Palier correspondant à un nombre de contributions.
    pub fn for_count(count: i64) -> Tier {
        // Du plus haut au plus bas : le premier seuil franchi est le bon.
        Tier::ALL
            .iter()
            .rev()
            .copied()
            .find(|tier| count >= tier.threshold())
            .unwrap_or(Tier::Novice)
    }

    /// Palier suivant, ou `None` au sommet du barème.
    pub fn next(&self) -> Option<Tier> {
        match self {
            Tier::Novice => Some(Tier::Budding),
            Tier::Budding => Some(Tier::Established),
            Tier::Established => Some(Tier::Innovator),
            Tier::Innovator => Some(Tier::Influential),
            Tier::Influential => None,
        }
    }

    /// Clé stable, pour que le client choisisse son icône et sa couleur sans
    /// dépendre du libellé traduit.
    pub fn key(&self) -> &'static str {
        match self {
            Tier::Novice => "novice",
            Tier::Budding => "budding",
            Tier::Established => "established",
            Tier::Innovator => "innovator",
            Tier::Influential => "influential",
        }
    }

    pub fn label(&self, lang: Lang) -> &'static str {
        match (self, lang) {
            (Tier::Novice, Lang::Fr) => "Novice",
            (Tier::Budding, Lang::Fr) => "Contributeur en herbe",
            (Tier::Established, Lang::Fr) => "Contributeur affirmé",
            (Tier::Innovator, Lang::Fr) => "Novateur",
            (Tier::Influential, Lang::Fr) => "Influent",
            (Tier::Novice, Lang::En) => "Novice",
            (Tier::Budding, Lang::En) => "Budding contributor",
            (Tier::Established, Lang::En) => "Established contributor",
            (Tier::Innovator, Lang::En) => "Innovator",
            (Tier::Influential, Lang::En) => "Influential",
        }
    }

    /// Une phrase qui dit ce que le palier vaut, à afficher sous le titre.
    ///
    /// Elle parle de l'effet de la contribution — des terrasses trouvées par
    /// d'autres — plutôt que du score : c'est ce qui donne envie de continuer.
    pub fn tagline(&self, lang: Lang) -> &'static str {
        match (self, lang) {
            (Tier::Novice, Lang::Fr) => "Votre premier signalement placera une terrasse sur la carte.",
            (Tier::Budding, Lang::Fr) => "Vos signalements aident déjà à trouver l'ombre.",
            (Tier::Established, Lang::Fr) => "Vos terrasses guident les recherches du quartier.",
            (Tier::Innovator, Lang::Fr) => "Vous cartographiez ce qu'aucune donnée publique ne dit.",
            (Tier::Influential, Lang::Fr) => "Une part de la carte tient grâce à vous.",
            (Tier::Novice, Lang::En) => "Your first report will put a terrace on the map.",
            (Tier::Budding, Lang::En) => "Your reports already help people find shade.",
            (Tier::Established, Lang::En) => "Your terraces guide searches across the area.",
            (Tier::Innovator, Lang::En) => "You map what no public dataset records.",
            (Tier::Influential, Lang::En) => "Part of the map stands thanks to you.",
        }
    }
}

/// Où en est un contributeur : son palier, et ce qui le sépare du suivant.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Progress {
    pub tier: Tier,
    pub count: i64,
    pub next: Option<Tier>,
    /// Contributions restantes avant le palier suivant. `0` au sommet.
    pub remaining: i64,
    /// Avancement dans le palier courant, de 0 à 1. Vaut 1 au sommet — la barre
    /// est pleine parce qu'il n'y a plus rien à remplir, pas par défaut.
    pub fraction: f64,
}

impl Progress {
    pub fn of(count: i64) -> Progress {
        let count = count.max(0);
        let tier = Tier::for_count(count);
        let Some(next) = tier.next() else {
            return Progress {
                tier,
                count,
                next: None,
                remaining: 0,
                fraction: 1.0,
            };
        };
        let floor = tier.threshold();
        let ceiling = next.threshold();
        // `ceiling > floor` par construction du barème ; le max(1) protège
        // quand même d'une division par zéro si deux seuils devenaient égaux.
        let span = (ceiling - floor).max(1) as f64;
        Progress {
            tier,
            count,
            next: Some(next),
            remaining: (ceiling - count).max(0),
            fraction: ((count - floor) as f64 / span).clamp(0.0, 1.0),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn thresholds_are_strictly_increasing() {
        for pair in Tier::ALL.windows(2) {
            assert!(
                pair[1].threshold() > pair[0].threshold(),
                "{:?} devrait exiger plus que {:?}",
                pair[1],
                pair[0]
            );
        }
    }

    #[test]
    fn tier_boundaries() {
        assert_eq!(Tier::for_count(0), Tier::Novice);
        assert_eq!(Tier::for_count(2), Tier::Novice);
        // Le seuil est atteint *à* la valeur, pas juste après.
        assert_eq!(Tier::for_count(3), Tier::Budding);
        assert_eq!(Tier::for_count(9), Tier::Budding);
        assert_eq!(Tier::for_count(10), Tier::Established);
        assert_eq!(Tier::for_count(24), Tier::Established);
        assert_eq!(Tier::for_count(25), Tier::Innovator);
        assert_eq!(Tier::for_count(59), Tier::Innovator);
        assert_eq!(Tier::for_count(60), Tier::Influential);
        assert_eq!(Tier::for_count(10_000), Tier::Influential);
    }

    #[test]
    fn progress_counts_down_to_next_tier() {
        let p = Progress::of(0);
        assert_eq!(p.tier, Tier::Novice);
        assert_eq!(p.next, Some(Tier::Budding));
        assert_eq!(p.remaining, 3);
        assert!(p.fraction.abs() < 1e-9);

        let p = Progress::of(2);
        assert_eq!(p.remaining, 1);
        assert!((p.fraction - 2.0 / 3.0).abs() < 1e-9);

        // Juste après un palier : la barre repart de zéro.
        let p = Progress::of(3);
        assert_eq!(p.tier, Tier::Budding);
        assert_eq!(p.remaining, 7);
        assert!(p.fraction.abs() < 1e-9);
    }

    #[test]
    fn top_tier_has_no_next() {
        let p = Progress::of(100);
        assert_eq!(p.tier, Tier::Influential);
        assert_eq!(p.next, None);
        assert_eq!(p.remaining, 0);
        assert!((p.fraction - 1.0).abs() < 1e-9);
    }

    #[test]
    fn negative_count_is_clamped() {
        // Aucun appelant ne devrait en produire, mais un COUNT signé rend la
        // valeur représentable : mieux vaut retomber sur Novice que paniquer.
        let p = Progress::of(-5);
        assert_eq!(p.tier, Tier::Novice);
        assert_eq!(p.count, 0);
    }

    #[test]
    fn every_tier_has_distinct_labels() {
        for lang in [Lang::Fr, Lang::En] {
            let labels: Vec<&str> = Tier::ALL.iter().map(|t| t.label(lang)).collect();
            let mut unique = labels.clone();
            unique.sort_unstable();
            unique.dedup();
            assert_eq!(unique.len(), labels.len(), "libellés dupliqués : {labels:?}");
        }
    }
}
