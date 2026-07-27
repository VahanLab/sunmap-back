//! Traduction des valeurs de tags OSM.
//!
//! Les tags OSM sont des clés techniques anglaises (`coffee_shop`, `fast_food`)
//! qu'on ne peut pas montrer telles quelles. La traduction vit côté serveur
//! plutôt que dans chaque client : la liste est longue, elle évolue avec OSM, et
//! Android n'aura pas à la recopier.
//!
//! Les valeurs inconnues ne sont pas masquées mais **humanisées** — `pizza_bar`
//! devient « Pizza bar ». Mieux vaut un libellé imparfait que rien, d'autant
//! qu'OSM compte 349 valeurs de `cuisine` rien que sur Paris : la traîne est
//! sans fin, on ne la couvrira jamais entièrement.

/// Langue demandée par le client. Anglais en repli : c'est la langue des tags,
/// donc celle où l'absence de traduction se voit le moins.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lang {
    Fr,
    En,
}

impl Lang {
    pub fn parse(raw: Option<&str>) -> Lang {
        match raw.map(|s| s.to_ascii_lowercase()) {
            Some(code) if code.starts_with("fr") => Lang::Fr,
            Some(_) => Lang::En,
            // Défaut français : seule langue proposée par l'app aujourd'hui.
            None => Lang::Fr,
        }
    }

    /// Jours de la semaine, du lundi au dimanche.
    pub fn weekdays(&self) -> [&'static str; 7] {
        match self {
            Lang::Fr => [
                "Lundi", "Mardi", "Mercredi", "Jeudi", "Vendredi", "Samedi", "Dimanche",
            ],
            Lang::En => [
                "Monday",
                "Tuesday",
                "Wednesday",
                "Thursday",
                "Friday",
                "Saturday",
                "Sunday",
            ],
        }
    }

    pub fn all_day(&self) -> &'static str {
        match self {
            Lang::Fr => "24 h/24",
            Lang::En => "24/7",
        }
    }
}

/// Libellé d'une catégorie d'établissement (`amenity`).
pub fn amenity_label(amenity: &str, lang: Lang) -> String {
    let table: &[(&str, &str)] = match lang {
        Lang::Fr => &[
            ("bar", "Bar"),
            ("pub", "Pub"),
            ("restaurant", "Restaurant"),
            ("cafe", "Café"),
            ("fast_food", "Restauration rapide"),
            ("biergarten", "Biergarten"),
        ],
        Lang::En => &[
            ("bar", "Bar"),
            ("pub", "Pub"),
            ("restaurant", "Restaurant"),
            ("cafe", "Café"),
            ("fast_food", "Fast food"),
            ("biergarten", "Beer garden"),
        ],
    };
    lookup(table, amenity)
}

/// Libellés des types de cuisine. Le tag est une liste séparée par `;`.
pub fn cuisine_labels(cuisine: &str, lang: Lang) -> Vec<String> {
    cuisine
        .split(';')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|value| cuisine_label(value, lang))
        .collect()
}

fn cuisine_label(cuisine: &str, lang: Lang) -> String {
    // Couvre les valeurs qui reviennent réellement : les premières de cette
    // liste représentent l'essentiel des 9 000 établissements parisiens taggés.
    let table: &[(&str, &str)] = match lang {
        Lang::Fr => &[
            ("french", "Française"),
            ("italian", "Italienne"),
            ("italian_pizza", "Pizza italienne"),
            ("pizza", "Pizza"),
            ("japanese", "Japonaise"),
            ("sushi", "Sushi"),
            ("ramen", "Ramen"),
            ("asian", "Asiatique"),
            ("chinese", "Chinoise"),
            ("vietnamese", "Vietnamienne"),
            ("thai", "Thaïlandaise"),
            ("korean", "Coréenne"),
            ("indian", "Indienne"),
            ("burger", "Burger"),
            ("kebab", "Kebab"),
            ("sandwich", "Sandwich"),
            ("coffee_shop", "Café"),
            ("crepe", "Crêperie"),
            ("lebanese", "Libanaise"),
            ("chicken", "Poulet"),
            ("bubble_tea", "Bubble tea"),
            ("regional", "Régionale"),
            ("salad", "Salades"),
            ("turkish", "Turque"),
            ("african", "Africaine"),
            ("mediterranean", "Méditerranéenne"),
            ("pasta", "Pâtes"),
            ("mexican", "Mexicaine"),
            ("greek", "Grecque"),
            ("seafood", "Fruits de mer"),
            ("moroccan", "Marocaine"),
            ("couscous", "Couscous"),
            ("portuguese", "Portugaise"),
            ("brasserie", "Brasserie"),
            ("poke", "Poké"),
            ("spanish", "Espagnole"),
            ("tapas", "Tapas"),
            ("bakery", "Boulangerie"),
            ("ice_cream", "Glaces"),
            ("dessert", "Desserts"),
            ("breakfast", "Petit-déjeuner"),
            ("brunch", "Brunch"),
            ("vegetarian", "Végétarienne"),
            ("vegan", "Végane"),
            ("steak_house", "Grillades"),
            ("barbecue", "Barbecue"),
            ("tea", "Salon de thé"),
            ("juice", "Jus de fruits"),
            ("wine_bar", "Bar à vin"),
            ("beer", "Bière"),
            ("cocktail", "Cocktails"),
            ("international", "Internationale"),
            ("fusion", "Fusion"),
            ("local", "Locale"),
            ("traditional", "Traditionnelle"),
            ("fish", "Poisson"),
            ("fish_and_chips", "Fish and chips"),
            ("noodle", "Nouilles"),
            ("dumpling", "Raviolis"),
            ("soup", "Soupes"),
            ("tacos", "Tacos"),
            ("bagel", "Bagels"),
            ("donut", "Donuts"),
            ("waffle", "Gaufres"),
            ("caribbean", "Antillaise"),
            ("peruvian", "Péruvienne"),
            ("brazilian", "Brésilienne"),
            ("argentinian", "Argentine"),
            ("german", "Allemande"),
            ("russian", "Russe"),
            ("polish", "Polonaise"),
            ("tibetan", "Tibétaine"),
            ("nepalese", "Népalaise"),
            ("pakistani", "Pakistanaise"),
            ("persian", "Persane"),
            ("syrian", "Syrienne"),
            ("egyptian", "Égyptienne"),
            ("ethiopian", "Éthiopienne"),
            ("senegalese", "Sénégalaise"),
            ("cambodian", "Cambodgienne"),
            ("filipino", "Philippine"),
            ("indonesian", "Indonésienne"),
            ("malaysian", "Malaisienne"),
            ("taiwanese", "Taïwanaise"),
            ("american", "Américaine"),
            ("british", "Britannique"),
            ("irish", "Irlandaise"),
            ("swiss", "Suisse"),
            ("belgian", "Belge"),
            ("savoyard", "Savoyarde"),
            ("basque", "Basque"),
            ("corsican", "Corse"),
            ("alsatian", "Alsacienne"),
            ("gourmet", "Gastronomique"),
            ("organic", "Bio"),
            ("halal", "Halal"),
            ("kosher", "Casher"),
        ],
        Lang::En => &[
            ("french", "French"),
            ("italian", "Italian"),
            ("coffee_shop", "Coffee shop"),
            ("crepe", "Crêperie"),
            ("regional", "Regional"),
        ],
    };
    lookup(table, cuisine)
}

fn lookup(table: &[(&str, &str)], key: &str) -> String {
    let key = key.trim();
    table
        .iter()
        .find(|(k, _)| *k == key)
        .map(|(_, label)| (*label).to_string())
        .unwrap_or_else(|| humanize(key))
}

/// `pizza_bar` → « Pizza bar ». Repli pour la traîne des valeurs OSM :
/// afficher une clé technique serait pire.
fn humanize(key: &str) -> String {
    let spaced = key.replace('_', " ");
    let mut chars = spaced.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_values_are_translated() {
        assert_eq!(cuisine_label("coffee_shop", Lang::Fr), "Café");
        assert_eq!(cuisine_label("french", Lang::Fr), "Française");
        assert_eq!(amenity_label("fast_food", Lang::Fr), "Restauration rapide");
    }

    /// La traîne OSM est sans fin : une valeur inconnue doit rester lisible.
    #[test]
    fn unknown_values_are_humanized() {
        assert_eq!(cuisine_label("pizza_bar", Lang::Fr), "Pizza bar");
        assert_eq!(cuisine_label("xyz", Lang::En), "Xyz");
    }

    #[test]
    fn cuisine_list_is_split_on_semicolons() {
        assert_eq!(
            cuisine_labels("french;pizza;coffee_shop", Lang::Fr),
            ["Française", "Pizza", "Café"]
        );
    }

    #[test]
    fn language_defaults_to_french() {
        assert_eq!(Lang::parse(None), Lang::Fr);
        assert_eq!(Lang::parse(Some("fr-FR")), Lang::Fr);
        assert_eq!(Lang::parse(Some("en")), Lang::En);
    }
}
