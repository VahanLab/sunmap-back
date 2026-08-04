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
    Es,
    It,
    De,
}

impl Lang {
    pub fn parse(raw: Option<&str>) -> Lang {
        match raw.map(|s| s.to_ascii_lowercase()) {
            Some(code) if code.starts_with("fr") => Lang::Fr,
            Some(code) if code.starts_with("es") => Lang::Es,
            Some(code) if code.starts_with("it") => Lang::It,
            Some(code) if code.starts_with("de") => Lang::De,
            Some(_) => Lang::En,
            // Défaut français : la langue des clients d'avant le paramètre.
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
            Lang::Es => [
                "Lunes",
                "Martes",
                "Miércoles",
                "Jueves",
                "Viernes",
                "Sábado",
                "Domingo",
            ],
            Lang::It => [
                "Lunedì",
                "Martedì",
                "Mercoledì",
                "Giovedì",
                "Venerdì",
                "Sabato",
                "Domenica",
            ],
            Lang::De => [
                "Montag",
                "Dienstag",
                "Mittwoch",
                "Donnerstag",
                "Freitag",
                "Samstag",
                "Sonntag",
            ],
        }
    }

    pub fn all_day(&self) -> &'static str {
        match self {
            Lang::Fr => "24 h/24",
            Lang::En => "24/7",
            Lang::Es => "24 h",
            Lang::It => "24 ore su 24",
            Lang::De => "Rund um die Uhr",
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
            ("bench", "Banc public"),
            ("picnic_table", "Table de pique-nique"),
        ],
        Lang::En => &[
            ("bar", "Bar"),
            ("pub", "Pub"),
            ("restaurant", "Restaurant"),
            ("cafe", "Café"),
            ("fast_food", "Fast food"),
            ("biergarten", "Beer garden"),
            ("bench", "Public bench"),
            ("picnic_table", "Picnic table"),
        ],
        Lang::Es => &[
            ("bar", "Bar"),
            ("pub", "Pub"),
            ("restaurant", "Restaurante"),
            ("cafe", "Café"),
            ("fast_food", "Comida rápida"),
            ("biergarten", "Biergarten"),
            ("bench", "Banco público"),
            ("picnic_table", "Mesa de pícnic"),
        ],
        Lang::It => &[
            ("bar", "Bar"),
            ("pub", "Pub"),
            ("restaurant", "Ristorante"),
            ("cafe", "Caffè"),
            ("fast_food", "Fast food"),
            ("biergarten", "Biergarten"),
            ("bench", "Panchina pubblica"),
            ("picnic_table", "Tavolo da picnic"),
        ],
        Lang::De => &[
            ("bar", "Bar"),
            ("pub", "Pub"),
            ("restaurant", "Restaurant"),
            ("cafe", "Café"),
            ("fast_food", "Imbiss"),
            ("biergarten", "Biergarten"),
            ("bench", "Sitzbank"),
            ("picnic_table", "Picknicktisch"),
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
        Lang::Es => &[
            ("french", "Francesa"),
            ("italian", "Italiana"),
            ("italian_pizza", "Pizza italiana"),
            ("pizza", "Pizza"),
            ("japanese", "Japonesa"),
            ("sushi", "Sushi"),
            ("ramen", "Ramen"),
            ("asian", "Asiática"),
            ("chinese", "China"),
            ("vietnamese", "Vietnamita"),
            ("thai", "Tailandesa"),
            ("korean", "Coreana"),
            ("indian", "India"),
            ("burger", "Hamburguesas"),
            ("kebab", "Kebab"),
            ("sandwich", "Sándwiches"),
            ("coffee_shop", "Cafetería"),
            ("crepe", "Crepería"),
            ("lebanese", "Libanesa"),
            ("chicken", "Pollo"),
            ("bubble_tea", "Bubble tea"),
            ("regional", "Regional"),
            ("salad", "Ensaladas"),
            ("turkish", "Turca"),
            ("african", "Africana"),
            ("mediterranean", "Mediterránea"),
            ("pasta", "Pasta"),
            ("mexican", "Mexicana"),
            ("greek", "Griega"),
            ("seafood", "Marisco"),
            ("moroccan", "Marroquí"),
            ("couscous", "Cuscús"),
            ("portuguese", "Portuguesa"),
            ("brasserie", "Brasserie"),
            ("poke", "Poké"),
            ("spanish", "Española"),
            ("tapas", "Tapas"),
            ("bakery", "Panadería"),
            ("ice_cream", "Helados"),
            ("dessert", "Postres"),
            ("breakfast", "Desayuno"),
            ("brunch", "Brunch"),
            ("vegetarian", "Vegetariana"),
            ("vegan", "Vegana"),
            ("steak_house", "Parrilla"),
            ("barbecue", "Barbacoa"),
            ("tea", "Salón de té"),
            ("juice", "Zumos"),
            ("wine_bar", "Bar de vinos"),
            ("beer", "Cerveza"),
            ("cocktail", "Cócteles"),
            ("international", "Internacional"),
            ("fusion", "Fusión"),
            ("local", "Local"),
            ("traditional", "Tradicional"),
            ("fish", "Pescado"),
            ("fish_and_chips", "Fish and chips"),
            ("noodle", "Fideos"),
            ("dumpling", "Dumplings"),
            ("soup", "Sopas"),
            ("tacos", "Tacos"),
            ("bagel", "Bagels"),
            ("donut", "Dónuts"),
            ("waffle", "Gofres"),
            ("caribbean", "Caribeña"),
            ("peruvian", "Peruana"),
            ("brazilian", "Brasileña"),
            ("argentinian", "Argentina"),
            ("german", "Alemana"),
            ("russian", "Rusa"),
            ("polish", "Polaca"),
            ("tibetan", "Tibetana"),
            ("nepalese", "Nepalí"),
            ("pakistani", "Pakistaní"),
            ("persian", "Persa"),
            ("syrian", "Siria"),
            ("egyptian", "Egipcia"),
            ("ethiopian", "Etíope"),
            ("senegalese", "Senegalesa"),
            ("cambodian", "Camboyana"),
            ("filipino", "Filipina"),
            ("indonesian", "Indonesia"),
            ("malaysian", "Malasia"),
            ("taiwanese", "Taiwanesa"),
            ("american", "Americana"),
            ("british", "Británica"),
            ("irish", "Irlandesa"),
            ("swiss", "Suiza"),
            ("belgian", "Belga"),
            ("savoyard", "Saboyana"),
            ("basque", "Vasca"),
            ("corsican", "Corsa"),
            ("alsatian", "Alsaciana"),
            ("gourmet", "Gastronómica"),
            ("organic", "Ecológica"),
            ("halal", "Halal"),
            ("kosher", "Kosher"),
        ],
        Lang::It => &[
            ("french", "Francese"),
            ("italian", "Italiana"),
            ("italian_pizza", "Pizza italiana"),
            ("pizza", "Pizza"),
            ("japanese", "Giapponese"),
            ("sushi", "Sushi"),
            ("ramen", "Ramen"),
            ("asian", "Asiatica"),
            ("chinese", "Cinese"),
            ("vietnamese", "Vietnamita"),
            ("thai", "Thailandese"),
            ("korean", "Coreana"),
            ("indian", "Indiana"),
            ("burger", "Burger"),
            ("kebab", "Kebab"),
            ("sandwich", "Panini"),
            ("coffee_shop", "Caffetteria"),
            ("crepe", "Crêperie"),
            ("lebanese", "Libanese"),
            ("chicken", "Pollo"),
            ("bubble_tea", "Bubble tea"),
            ("regional", "Regionale"),
            ("salad", "Insalate"),
            ("turkish", "Turca"),
            ("african", "Africana"),
            ("mediterranean", "Mediterranea"),
            ("pasta", "Pasta"),
            ("mexican", "Messicana"),
            ("greek", "Greca"),
            ("seafood", "Frutti di mare"),
            ("moroccan", "Marocchina"),
            ("couscous", "Cous cous"),
            ("portuguese", "Portoghese"),
            ("brasserie", "Brasserie"),
            ("poke", "Poke"),
            ("spanish", "Spagnola"),
            ("tapas", "Tapas"),
            ("bakery", "Panetteria"),
            ("ice_cream", "Gelati"),
            ("dessert", "Dolci"),
            ("breakfast", "Colazione"),
            ("brunch", "Brunch"),
            ("vegetarian", "Vegetariana"),
            ("vegan", "Vegana"),
            ("steak_house", "Griglieria"),
            ("barbecue", "Barbecue"),
            ("tea", "Sala da tè"),
            ("juice", "Succhi"),
            ("wine_bar", "Enoteca"),
            ("beer", "Birra"),
            ("cocktail", "Cocktail"),
            ("international", "Internazionale"),
            ("fusion", "Fusion"),
            ("local", "Locale"),
            ("traditional", "Tradizionale"),
            ("fish", "Pesce"),
            ("fish_and_chips", "Fish and chips"),
            ("noodle", "Noodles"),
            ("dumpling", "Ravioli"),
            ("soup", "Zuppe"),
            ("tacos", "Tacos"),
            ("bagel", "Bagel"),
            ("donut", "Donut"),
            ("waffle", "Waffle"),
            ("caribbean", "Caraibica"),
            ("peruvian", "Peruviana"),
            ("brazilian", "Brasiliana"),
            ("argentinian", "Argentina"),
            ("german", "Tedesca"),
            ("russian", "Russa"),
            ("polish", "Polacca"),
            ("tibetan", "Tibetana"),
            ("nepalese", "Nepalese"),
            ("pakistani", "Pakistana"),
            ("persian", "Persiana"),
            ("syrian", "Siriana"),
            ("egyptian", "Egiziana"),
            ("ethiopian", "Etiope"),
            ("senegalese", "Senegalese"),
            ("cambodian", "Cambogiana"),
            ("filipino", "Filippina"),
            ("indonesian", "Indonesiana"),
            ("malaysian", "Malese"),
            ("taiwanese", "Taiwanese"),
            ("american", "Americana"),
            ("british", "Britannica"),
            ("irish", "Irlandese"),
            ("swiss", "Svizzera"),
            ("belgian", "Belga"),
            ("savoyard", "Savoiarda"),
            ("basque", "Basca"),
            ("corsican", "Corsa"),
            ("alsatian", "Alsaziana"),
            ("gourmet", "Gourmet"),
            ("organic", "Biologica"),
            ("halal", "Halal"),
            ("kosher", "Kosher"),
        ],
        Lang::De => &[
            ("french", "Französisch"),
            ("italian", "Italienisch"),
            ("italian_pizza", "Italienische Pizza"),
            ("pizza", "Pizza"),
            ("japanese", "Japanisch"),
            ("sushi", "Sushi"),
            ("ramen", "Ramen"),
            ("asian", "Asiatisch"),
            ("chinese", "Chinesisch"),
            ("vietnamese", "Vietnamesisch"),
            ("thai", "Thailändisch"),
            ("korean", "Koreanisch"),
            ("indian", "Indisch"),
            ("burger", "Burger"),
            ("kebab", "Kebab"),
            ("sandwich", "Sandwiches"),
            ("coffee_shop", "Kaffeehaus"),
            ("crepe", "Crêperie"),
            ("lebanese", "Libanesisch"),
            ("chicken", "Hähnchen"),
            ("bubble_tea", "Bubble Tea"),
            ("regional", "Regional"),
            ("salad", "Salate"),
            ("turkish", "Türkisch"),
            ("african", "Afrikanisch"),
            ("mediterranean", "Mediterran"),
            ("pasta", "Pasta"),
            ("mexican", "Mexikanisch"),
            ("greek", "Griechisch"),
            ("seafood", "Meeresfrüchte"),
            ("moroccan", "Marokkanisch"),
            ("couscous", "Couscous"),
            ("portuguese", "Portugiesisch"),
            ("brasserie", "Brasserie"),
            ("poke", "Poké"),
            ("spanish", "Spanisch"),
            ("tapas", "Tapas"),
            ("bakery", "Bäckerei"),
            ("ice_cream", "Eis"),
            ("dessert", "Desserts"),
            ("breakfast", "Frühstück"),
            ("brunch", "Brunch"),
            ("vegetarian", "Vegetarisch"),
            ("vegan", "Vegan"),
            ("steak_house", "Steakhaus"),
            ("barbecue", "Barbecue"),
            ("tea", "Teestube"),
            ("juice", "Säfte"),
            ("wine_bar", "Weinbar"),
            ("beer", "Bier"),
            ("cocktail", "Cocktails"),
            ("international", "International"),
            ("fusion", "Fusion"),
            ("local", "Lokal"),
            ("traditional", "Traditionell"),
            ("fish", "Fisch"),
            ("fish_and_chips", "Fish and Chips"),
            ("noodle", "Nudeln"),
            ("dumpling", "Teigtaschen"),
            ("soup", "Suppen"),
            ("tacos", "Tacos"),
            ("bagel", "Bagels"),
            ("donut", "Donuts"),
            ("waffle", "Waffeln"),
            ("caribbean", "Karibisch"),
            ("peruvian", "Peruanisch"),
            ("brazilian", "Brasilianisch"),
            ("argentinian", "Argentinisch"),
            ("german", "Deutsch"),
            ("russian", "Russisch"),
            ("polish", "Polnisch"),
            ("tibetan", "Tibetisch"),
            ("nepalese", "Nepalesisch"),
            ("pakistani", "Pakistanisch"),
            ("persian", "Persisch"),
            ("syrian", "Syrisch"),
            ("egyptian", "Ägyptisch"),
            ("ethiopian", "Äthiopisch"),
            ("senegalese", "Senegalesisch"),
            ("cambodian", "Kambodschanisch"),
            ("filipino", "Philippinisch"),
            ("indonesian", "Indonesisch"),
            ("malaysian", "Malaysisch"),
            ("taiwanese", "Taiwanisch"),
            ("american", "Amerikanisch"),
            ("british", "Britisch"),
            ("irish", "Irisch"),
            ("swiss", "Schweizerisch"),
            ("belgian", "Belgisch"),
            ("savoyard", "Savoyisch"),
            ("basque", "Baskisch"),
            ("corsican", "Korsisch"),
            ("alsatian", "Elsässisch"),
            ("gourmet", "Gourmet"),
            ("organic", "Bio"),
            ("halal", "Halal"),
            ("kosher", "Koscher"),
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
