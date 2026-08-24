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
    LocalInfluencer,
    Influencer,
    StarInfluencer,
    Vip,
    Lover,
    Got,
}

impl Tier {
    /// Dans l'ordre, du plus bas au plus haut.
    pub const ALL: [Tier; 9] = [
        Tier::Novice,
        Tier::Budding,
        Tier::Established,
        Tier::LocalInfluencer,
        Tier::Influencer,
        Tier::StarInfluencer,
        Tier::Vip,
        Tier::Lover,
        Tier::Got,
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
            Tier::LocalInfluencer => 25,
            Tier::Influencer => 60,
            Tier::StarInfluencer => 120,
            Tier::Vip => 250,
            Tier::Lover => 500,
            Tier::Got => 1_000,
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

    /// Rang du palier dans le barème, **à partir de 1** — c'est un rang lu par
    /// un humain, pas un index.
    ///
    /// Servi au client comme le libellé et le seuil : il ne reçoit que le
    /// palier courant et le suivant, il n'a pas de quoi compter.
    pub fn level(&self) -> usize {
        Tier::ALL.iter().position(|t| t == self).unwrap_or(0) + 1
    }

    /// Nombre total de paliers, pour dire « niveau 3 sur 5 ».
    pub const COUNT: usize = Tier::ALL.len();

    /// Palier suivant, ou `None` au sommet du barème.
    pub fn next(&self) -> Option<Tier> {
        let i = self.level(); // `level` est 1-based : c'est déjà l'index du suivant.
        Tier::ALL.get(i).copied()
    }

    /// Clé stable, pour que le client choisisse son icône et sa couleur sans
    /// dépendre du libellé traduit.
    ///
    /// **Une clé ne se renomme pas**, même quand le libellé change : les apps
    /// déjà installées y accrochent leur icône, et une clé inconnue les fait
    /// retomber sur un symbole générique. D'où `innovator` et `influential`,
    /// hérités des libellés « Novateur » et « Influent » que ces deux paliers
    /// portaient avant de devenir « Influenceur local » et « Influenceur ».
    pub fn key(&self) -> &'static str {
        match self {
            Tier::Novice => "novice",
            Tier::Budding => "budding",
            Tier::Established => "established",
            Tier::LocalInfluencer => "innovator",
            Tier::Influencer => "influential",
            Tier::StarInfluencer => "star_influencer",
            Tier::Vip => "vip",
            Tier::Lover => "lover",
            Tier::Got => "got",
        }
    }

    /// Libellé traduit du palier.
    ///
    /// **`Vip`, `Lover` et `Got` ne se traduisent pas** : ce sont des noms
    /// propres du produit, pas des descriptions de rang. Les décliner par
    /// langue en ferait cinq titres différents pour une même distinction.
    pub fn label(&self, lang: Lang) -> &'static str {
        match self {
            Tier::Vip => return "VIP",
            Tier::Lover => return "SunMap Lover",
            Tier::Got => return "SunMap GoT",
            _ => {}
        }
        match (self, lang) {
            (Tier::Novice, Lang::Fr) => "Novice",
            (Tier::Budding, Lang::Fr) => "Contributeur en herbe",
            (Tier::Established, Lang::Fr) => "Contributeur affirmé",
            (Tier::LocalInfluencer, Lang::Fr) => "Influenceur local",
            (Tier::Influencer, Lang::Fr) => "Influenceur",
            (Tier::StarInfluencer, Lang::Fr) => "Influenceur star",
            (Tier::Novice, Lang::En) => "Novice",
            (Tier::Budding, Lang::En) => "Budding contributor",
            (Tier::Established, Lang::En) => "Established contributor",
            (Tier::LocalInfluencer, Lang::En) => "Local influencer",
            (Tier::Influencer, Lang::En) => "Influencer",
            (Tier::StarInfluencer, Lang::En) => "Star influencer",
            (Tier::Novice, Lang::Es) => "Novato",
            (Tier::Budding, Lang::Es) => "Contribuidor en ciernes",
            (Tier::Established, Lang::Es) => "Contribuidor consolidado",
            (Tier::LocalInfluencer, Lang::Es) => "Influyente local",
            (Tier::Influencer, Lang::Es) => "Influyente",
            (Tier::StarInfluencer, Lang::Es) => "Influyente estrella",
            (Tier::Novice, Lang::It) => "Novizio",
            (Tier::Budding, Lang::It) => "Contributore in erba",
            (Tier::Established, Lang::It) => "Contributore affermato",
            (Tier::LocalInfluencer, Lang::It) => "Influencer locale",
            (Tier::Influencer, Lang::It) => "Influencer",
            (Tier::StarInfluencer, Lang::It) => "Influencer star",
            (Tier::Novice, Lang::De) => "Neuling",
            (Tier::Budding, Lang::De) => "Angehender Beitragender",
            (Tier::Established, Lang::De) => "Etablierter Beitragender",
            (Tier::LocalInfluencer, Lang::De) => "Lokaler Influencer",
            (Tier::Influencer, Lang::De) => "Influencer",
            (Tier::StarInfluencer, Lang::De) => "Star-Influencer",
            // Traités plus haut, quelle que soit la langue.
            (Tier::Vip | Tier::Lover | Tier::Got, _) => unreachable!(),
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
        assert_eq!(Tier::for_count(25), Tier::LocalInfluencer);
        assert_eq!(Tier::for_count(59), Tier::LocalInfluencer);
        assert_eq!(Tier::for_count(60), Tier::Influencer);
        assert_eq!(Tier::for_count(119), Tier::Influencer);
        assert_eq!(Tier::for_count(120), Tier::StarInfluencer);
        assert_eq!(Tier::for_count(250), Tier::Vip);
        assert_eq!(Tier::for_count(500), Tier::Lover);
        assert_eq!(Tier::for_count(999), Tier::Lover);
        assert_eq!(Tier::for_count(1_000), Tier::Got);
        assert_eq!(Tier::for_count(10_000), Tier::Got);
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
        let p = Progress::of(5_000);
        assert_eq!(p.tier, Tier::Got);
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
    fn levels_are_one_based_and_ordered() {
        assert_eq!(Tier::Novice.level(), 1);
        assert_eq!(Tier::Established.level(), 3);
        assert_eq!(Tier::Got.level(), Tier::COUNT);
        for pair in Tier::ALL.windows(2) {
            assert_eq!(pair[1].level(), pair[0].level() + 1);
        }
    }

    #[test]
    fn next_walks_the_whole_scale() {
        for pair in Tier::ALL.windows(2) {
            assert_eq!(pair[0].next(), Some(pair[1]));
        }
        assert_eq!(Tier::Got.next(), None);
    }

    #[test]
    fn product_names_are_not_translated() {
        for lang in [Lang::Fr, Lang::En, Lang::Es, Lang::It, Lang::De] {
            assert_eq!(Tier::Vip.label(lang), "VIP");
            assert_eq!(Tier::Lover.label(lang), "SunMap Lover");
            assert_eq!(Tier::Got.label(lang), "SunMap GoT");
        }
    }

    #[test]
    fn every_tier_has_distinct_labels() {
        for lang in [Lang::Fr, Lang::En, Lang::Es, Lang::It, Lang::De] {
            let labels: Vec<&str> = Tier::ALL.iter().map(|t| t.label(lang)).collect();
            let mut unique = labels.clone();
            unique.sort_unstable();
            unique.dedup();
            assert_eq!(unique.len(), labels.len(), "libellés dupliqués : {labels:?}");
        }
    }
}
