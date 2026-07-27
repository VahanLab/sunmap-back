//! Décodage du tag OSM `opening_hours`.
//!
//! La spécification est standardisée mais énorme : saisons, jours fériés,
//! semaines paires, `sunrise`/`sunset`, « le 3e lundi », commentaires libres.
//! L'implémenter entièrement est un projet en soi. On décode donc **un
//! sous-ensemble** et on échoue proprement sur le reste — le client affiche
//! alors la chaîne brute, ce qui vaut mieux qu'un tableau faux.
//!
//! Le sous-ensemble a été mis au point contre les 6 055 valeurs distinctes de
//! la base parisienne et en décode 97,9 %. Les échecs restants sont du
//! saisonnier, du texte libre et des fautes de frappe.
//!
//! Ce décodage vit côté serveur pour que le client n'ait qu'à afficher, et pour
//! qu'Android n'ait pas à réécrire la même grammaire.

/// Une plage horaire, en minutes depuis minuit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TimeRange {
    pub start: u16,
    /// Peut être inférieur à `start` quand l'établissement ferme après minuit.
    pub end: u16,
}

impl TimeRange {
    /// Journée entière, à présenter autrement qu'en « 00:00 – 24:00 ».
    pub fn is_all_day(&self) -> bool {
        self.start == 0 && self.end == 24 * 60
    }

    /// Pas de repli de 24:00 sur 00:00 : une fermeture à minuit s'écrit
    /// « 24:00 », l'afficher « 00:00 » laisserait croire à une ouverture nulle.
    fn format(minutes: u16) -> String {
        format!("{:02}:{:02}", minutes / 60, minutes % 60)
    }

    pub fn text(&self) -> String {
        format!("{} – {}", Self::format(self.start), Self::format(self.end))
    }
}

/// Semaine décodée : sept listes de créneaux, du lundi au dimanche. Une liste
/// vide signifie fermé — les jours non mentionnés dans la chaîne le sont, c'est
/// la règle de la spécification.
pub type Week = [Vec<TimeRange>; 7];

const OSM_DAYS: [&str; 7] = ["Mo", "Tu", "We", "Th", "Fr", "Sa", "Su"];

/// Motifs qu'on ne sait pas décoder. Leur présence fait échouer d'emblée plutôt
/// que de produire un tableau qui ignorerait silencieusement une restriction
/// saisonnière ou un commentaire.
const UNSUPPORTED: &[&str] = &[
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec", "sunrise",
    "sunset", "dawn", "dusk", "week", "easter", "\"", "[",
];

/// `None` = non décodable, le client doit afficher la chaîne brute.
pub fn parse(raw: &str) -> Option<Week> {
    let text = raw.trim();
    if text.is_empty() || UNSUPPORTED.iter().any(|p| text.contains(p)) {
        return None;
    }

    if text == "24/7" {
        let all = TimeRange { start: 0, end: 24 * 60 };
        return Some(std::array::from_fn(|_| vec![all]));
    }

    let mut week: Week = Default::default();
    let mut matched = false;

    // Le point-virgule sépare des règles qui se REMPLACENT, la virgule des
    // règles qui s'AJOUTENT. Confondre les deux fait perdre le second service
    // dans `Mo-Su 12:00-14:30,Mo-Su 19:00-23:00`.
    for chunk in text.split(';') {
        let chunk = chunk.trim();
        if chunk.is_empty() {
            continue;
        }
        let effect = parse_chunk(chunk)?;
        if effect.is_empty() {
            continue; // règle purement fériés, ignorée
        }
        for (day, ranges) in effect {
            week[day] = ranges;
            matched = true;
        }
    }

    matched.then_some(week)
}

/// Décode un groupe séparé par `;`, dont les sous-règles séparées par des
/// virgules s'additionnent.
fn parse_chunk(chunk: &str) -> Option<Vec<(usize, Vec<TimeRange>)>> {
    let mut effect: Vec<(usize, Vec<TimeRange>)> = Vec::new();

    // Une liste de jours peut s'étaler sur plusieurs segments avant que les
    // horaires n'arrivent (`Sa,Su 18:30-22:30`).
    let mut pending: Vec<usize> = Vec::new();
    let mut saw_times = false;

    for segment in chunk.split(',') {
        let segment = segment.trim();
        if segment.is_empty() {
            continue;
        }

        // Le premier caractère lève l'ambiguïté de la virgule, qui sépare aussi
        // bien des jours que des créneaux.
        if segment.starts_with(|c: char| c.is_ascii_digit()) {
            let ranges = parse_time_ranges(segment)?;
            let target: Vec<usize> = if pending.is_empty() {
                (0..7).collect()
            } else {
                pending.clone()
            };
            for day in target {
                push(&mut effect, day, &ranges, false);
            }
            saw_times = true;
            continue;
        }

        // Nouveaux jours : nouvelle sous-règle dès que la précédente avait ses
        // horaires.
        if saw_times {
            pending.clear();
            saw_times = false;
        }

        let (day_token, rest) = match segment.split_once(' ') {
            Some((d, r)) => (d, r.trim()),
            None => (segment, ""),
        };

        // Jours fériés et vacances scolaires : ignorés, pas bloquants. Le
        // tableau montre la semaine ordinaire, qui est ce qu'on cherche.
        if day_token == "PH" || day_token == "SH" {
            continue;
        }
        if day_token == "off" || day_token == "closed" {
            let target: Vec<usize> = if pending.is_empty() {
                (0..7).collect()
            } else {
                pending.clone()
            };
            for day in target {
                push(&mut effect, day, &[], true);
            }
            saw_times = true;
            continue;
        }

        let days = parse_day_selector(day_token)?;
        for day in &days {
            if !effect.iter().any(|(d, _)| d == day) {
                effect.push((*day, Vec::new()));
            }
        }
        pending.extend_from_slice(&days);

        if rest.is_empty() {
            continue;
        }
        if rest == "off" || rest == "closed" {
            let target = pending.clone();
            for day in target {
                push(&mut effect, day, &[], true);
            }
            saw_times = true;
            continue;
        }
        let ranges = parse_time_ranges(rest)?;
        let target = pending.clone();
        for day in target {
            push(&mut effect, day, &ranges, false);
        }
        saw_times = true;
    }
    Some(effect)
}

/// Ajoute (ou remplace) les créneaux d'un jour dans l'effet en construction.
fn push(effect: &mut Vec<(usize, Vec<TimeRange>)>, day: usize, ranges: &[TimeRange], replace: bool) {
    match effect.iter_mut().find(|(d, _)| *d == day) {
        Some((_, existing)) if replace => *existing = ranges.to_vec(),
        Some((_, existing)) => existing.extend_from_slice(ranges),
        None => effect.push((day, ranges.to_vec())),
    }
}

/// `Mo`, `Mo-Fr`, `Mo-We` → indices de jours. `Sa-Mo` enjambe la fin de
/// semaine, ce que la spécification autorise.
fn parse_day_selector(token: &str) -> Option<Vec<usize>> {
    let index = |code: &str| OSM_DAYS.iter().position(|d| *d == code);
    match token.split_once('-') {
        None => Some(vec![index(token)?]),
        Some((from, to)) => {
            let (from, to) = (index(from)?, index(to)?);
            let mut days = vec![from];
            let mut day = from;
            while day != to {
                day = (day + 1) % 7;
                days.push(day);
            }
            Some(days)
        }
    }
}

/// `12:00-14:30` → créneau. Le suffixe `+` (« et plus tard ») est retiré.
fn parse_time_ranges(token: &str) -> Option<Vec<TimeRange>> {
    let mut ranges = Vec::new();
    for part in token.split(',') {
        let part = part.trim().trim_end_matches('+');
        let (start, end) = part.split_once('-')?;
        ranges.push(TimeRange {
            start: parse_minutes(start)?,
            end: parse_minutes(end)?,
        });
    }
    (!ranges.is_empty()).then_some(ranges)
}

fn parse_minutes(token: &str) -> Option<u16> {
    let (h, m) = token.trim().split_once(':')?;
    let (h, m): (u16, u16) = (h.parse().ok()?, m.parse().ok()?);
    (h <= 24 && m < 60).then(|| h * 60 + m)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn texts(week: &Week, day: usize) -> Vec<String> {
        week[day].iter().map(|r| r.text()).collect()
    }

    #[test]
    fn simple_range_applies_to_every_listed_day() {
        let w = parse("Mo-Su 11:00-23:00").unwrap();
        for day in 0..7 {
            assert_eq!(texts(&w, day), ["11:00 – 23:00"]);
        }
    }

    /// Jours non mentionnés = fermés, c'est la règle de la spécification.
    #[test]
    fn unlisted_days_are_closed() {
        let w = parse("Mo-Fr 11:30-14:30").unwrap();
        assert_eq!(texts(&w, 4), ["11:30 – 14:30"]);
        assert!(w[5].is_empty());
        assert!(w[6].is_empty());
    }

    /// La virgule ajoute, le point-virgule remplace. C'est la distinction qui a
    /// fait passer la couverture de 80,8 % à 97,9 % sur données réelles.
    #[test]
    fn comma_adds_semicolon_replaces() {
        let added = parse("Mo-Su 12:00-14:30,Mo-Su 19:00-23:00").unwrap();
        assert_eq!(texts(&added, 0), ["12:00 – 14:30", "19:00 – 23:00"]);

        let replaced = parse("Mo-Su 12:00-14:30; Mo-Su 19:00-23:00").unwrap();
        assert_eq!(texts(&replaced, 0), ["19:00 – 23:00"]);
    }

    /// La même virgule sépare aussi bien des jours que des créneaux.
    #[test]
    fn day_list_and_time_list_share_the_comma() {
        let w = parse("Mo-Fr 11:30-14:30,18:30-22:30; Sa,Su 18:30-22:30").unwrap();
        assert_eq!(texts(&w, 0), ["11:30 – 14:30", "18:30 – 22:30"]);
        assert_eq!(texts(&w, 5), ["18:30 – 22:30"]);
        assert_eq!(texts(&w, 6), ["18:30 – 22:30"]);
    }

    #[test]
    fn holidays_are_ignored_not_fatal() {
        let w = parse("Mo-Su 12:00-23:00; PH off").unwrap();
        assert_eq!(texts(&w, 0), ["12:00 – 23:00"]);
        let inline = parse("PH,Mo-Su 10:00-01:00").unwrap();
        assert_eq!(texts(&inline, 0), ["10:00 – 01:00"]);
    }

    #[test]
    fn explicit_off_closes() {
        let w = parse("Mo-Sa 10:00-19:00; Su off").unwrap();
        assert_eq!(texts(&w, 5), ["10:00 – 19:00"]);
        assert!(w[6].is_empty());
    }

    #[test]
    fn open_ended_suffix_is_dropped() {
        let w = parse("Mo-Su 07:00-22:30+").unwrap();
        assert_eq!(texts(&w, 0), ["07:00 – 22:30"]);
    }

    #[test]
    fn always_open() {
        let w = parse("24/7").unwrap();
        assert!(w[3][0].is_all_day());
    }

    /// Ce qu'on ne sait pas décoder doit échouer, pas produire un tableau
    /// partiel qui masquerait une restriction.
    #[test]
    fn unsupported_forms_fail() {
        assert!(parse("Apr-Sep Mo-Su 09:00-20:00").is_none());
        assert!(parse("Mo-Fr 09:00-18:00 \"sur rendez-vous\"").is_none());
        assert!(parse("Mo-Su sunrise-sunset").is_none());
        assert!(parse("n'importe quoi").is_none());
        assert!(parse("").is_none());
    }
}
