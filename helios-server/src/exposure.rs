//! Profil annuel d'ensoleillement : combien de soleil direct, chaque mois, et
//! à quelle heure de la journée.
//!
//! Module **pur** : il ne connaît ni la DSM ni axum. Il fabrique les instants
//! à échantillonner (dans un fuseau donné, changement d'heure compris), puis
//! agrège la réponse d'un classifieur `instant → soleil/ombre/nuit` fourni par
//! l'appelant. Le même code sert une case de façade, un point au sol et un
//! lieu — seul le classifieur change.
//!
//! ## Échantillonnage
//!
//! **Tous les jours** d'une **année de référence fixe**, par tranches de
//! 10 min (le pas de `sun_day`). Année fixe pour que le résultat ne dépende pas
//! de la date de la requête et se mette en cache sans expiration ; la position
//! solaire d'une année à l'autre ne bouge que de quelques centièmes de degré.
//!
//! Tous les jours et pas un sur 7, comme envisagé au départ : mesuré sur une
//! rue étroite, le sous-échantillonnage s'écartait jusqu'à 34 min/jour de la
//! moyenne mensuelle vraie (encore 12 min à un jour sur 2) — une ombre de
//! bâtiment bascule brutalement d'un jour à l'autre quand le soleil franchit
//! une ligne de toit, et les jours sautés ratent ces bascules. Le calcul
//! complet ne coûte pourtant que ~0,2 s de CPU pour une façade de 36 cases
//! (780 000 rayons, mesuré en release) : aucune raison d'approximer.

use chrono::{Datelike, Duration, NaiveDate, TimeZone, Timelike};
use chrono_tz::Tz;
use serde::Serialize;

/// Année de référence (non bissextile) : cf. en-tête du module.
pub const REFERENCE_YEAR: i32 = 2027;
/// Un jour échantillonné sur `DEFAULT_DAY_STRIDE` : tous (cf. en-tête).
pub const DEFAULT_DAY_STRIDE: u32 = 1;
/// Pas temporel, en secondes : celui de `sun_day`.
pub const STEP_S: i64 = 600;
/// Tranches de 30 min de la heatmap, en heure légale locale.
pub const SLOTS_PER_DAY: usize = 48;
/// Fuseau par défaut : toute la couverture actuelle est en France.
pub const DEFAULT_TZ: Tz = chrono_tz::Europe::Paris;

/// État d'un point à un instant — la nuit à part, sinon une façade nord
/// « à l'ombre 16 h par jour » ne dirait rien d'utile.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Sample {
    Sunlit,
    Shadow,
    Night,
}

/// Un instant à classer, et la tranche locale de 30 min où il tombe.
#[derive(Clone, Copy, Debug)]
pub struct SampleInstant {
    pub unix: f64,
    pub slot: u8,
}

/// Une journée locale échantillonnée (23 h ou 25 h les jours de bascule).
#[derive(Clone, Debug)]
pub struct SampledDay {
    /// Mois local, 1 à 12.
    pub month: u8,
    pub instants: Vec<SampleInstant>,
}

/// Un mois du profil.
#[derive(Serialize, Clone, Debug, PartialEq)]
pub struct MonthProfile {
    pub month: u8,
    /// Minutes de soleil direct par jour, en moyenne sur le mois.
    pub avg_sunlit_minutes: u32,
    /// Durée du jour (soleil levé) : de quoi dire « 1 h 14 sur 8 h 42 ».
    pub avg_daylight_minutes: u32,
    /// 48 tranches de 30 min (00:00 → 23:30, heure légale) : % du temps au
    /// soleil dans la tranche, de 0 à 100.
    pub slots: Vec<u8>,
}

/// Profil annuel complet.
#[derive(Serialize, Clone, Debug, PartialEq)]
pub struct ExposureProfile {
    pub annual_avg_sunlit_minutes: u32,
    pub annual_avg_daylight_minutes: u32,
    pub months: Vec<MonthProfile>,
}

/// Interprète un nom de fuseau IANA (`Europe/Paris`), défaut si absent.
pub fn parse_tz(raw: Option<&str>) -> Result<Tz, String> {
    match raw.map(str::trim).filter(|s| !s.is_empty()) {
        None => Ok(DEFAULT_TZ),
        Some(name) => name.parse::<Tz>().map_err(|_| format!("tz inconnu : {name}")),
    }
}

/// Instant UTC du début de la journée locale `date`.
///
/// Quelques fuseaux basculent à minuit (minuit n'existe pas ce jour-là) : on
/// prend alors le premier instant valide de la journée.
fn local_day_start(tz: Tz, date: NaiveDate) -> i64 {
    (0..3)
        .find_map(|h| {
            tz.from_local_datetime(&date.and_hms_opt(h, 0, 0)?)
                .earliest()
                .map(|d| d.timestamp())
        })
        .expect("une journée locale a toujours un instant valide avant 3 h")
}

/// Les journées à échantillonner : une tous les `stride_days`, la première au
/// milieu de la première période (le 4 janvier pour un pas de 7), chacune de
/// minuit local à minuit local, instants au milieu de chaque tranche de 10 min.
pub fn sampled_days(tz: Tz, year: i32, stride_days: u32) -> Vec<SampledDay> {
    let stride = stride_days.max(1) as i64;
    let mut date = NaiveDate::from_ymd_opt(year, 1, 1).expect("année valide")
        + Duration::days(stride / 2);
    let mut out = Vec::new();
    while date.year() == year {
        let start = local_day_start(tz, date);
        let end = local_day_start(tz, date + Duration::days(1));
        let mut instants = Vec::with_capacity(((end - start) / STEP_S) as usize);
        let mut t = start + STEP_S / 2;
        while t < end {
            let local = tz.timestamp_opt(t, 0).single().expect("instant UTC non ambigu");
            let slot = ((local.hour() * 60 + local.minute()) / 30) as u8;
            instants.push(SampleInstant { unix: t as f64, slot });
            t += STEP_S;
        }
        out.push(SampledDay { month: date.month() as u8, instants });
        date += Duration::days(stride);
    }
    out
}

/// Agrège un profil. `classify(jour, instant)` reçoit les indices dans `days`
/// plutôt que l'instant lui-même : l'appelant peut ainsi précalculer la
/// position solaire une fois pour toutes les cases d'une façade.
pub fn build_profile(
    days: &[SampledDay],
    mut classify: impl FnMut(usize, usize) -> Sample,
) -> ExposureProfile {
    #[derive(Clone)]
    struct Acc {
        days: u32,
        sunlit: u32,
        daylight: u32,
        slot_sun: [u32; SLOTS_PER_DAY],
        slot_total: [u32; SLOTS_PER_DAY],
    }
    let empty = Acc {
        days: 0,
        sunlit: 0,
        daylight: 0,
        slot_sun: [0; SLOTS_PER_DAY],
        slot_total: [0; SLOTS_PER_DAY],
    };
    let mut months = vec![empty; 12];
    for (di, day) in days.iter().enumerate() {
        let acc = &mut months[(day.month - 1) as usize];
        acc.days += 1;
        for (ii, instant) in day.instants.iter().enumerate() {
            let slot = instant.slot as usize;
            acc.slot_total[slot] += 1;
            match classify(di, ii) {
                Sample::Sunlit => {
                    acc.sunlit += 1;
                    acc.daylight += 1;
                    acc.slot_sun[slot] += 1;
                }
                Sample::Shadow => acc.daylight += 1,
                Sample::Night => {}
            }
        }
    }

    let step_min = (STEP_S / 60) as f64;
    let avg = |samples: u32, days: u32| -> u32 {
        if days == 0 {
            0
        } else {
            (samples as f64 * step_min / days as f64).round() as u32
        }
    };
    let total_days: u32 = months.iter().map(|m| m.days).sum();
    let total_sunlit: u32 = months.iter().map(|m| m.sunlit).sum();
    let total_daylight: u32 = months.iter().map(|m| m.daylight).sum();

    ExposureProfile {
        annual_avg_sunlit_minutes: avg(total_sunlit, total_days),
        annual_avg_daylight_minutes: avg(total_daylight, total_days),
        months: months
            .iter()
            .enumerate()
            .map(|(i, m)| MonthProfile {
                month: i as u8 + 1,
                avg_sunlit_minutes: avg(m.sunlit, m.days),
                avg_daylight_minutes: avg(m.daylight, m.days),
                slots: (0..SLOTS_PER_DAY)
                    .map(|s| match m.slot_total[s] {
                        0 => 0,
                        total => (100.0 * m.slot_sun[s] as f64 / total as f64).round() as u8,
                    })
                    .collect(),
            })
            .collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use helios_core::sun::sun_position;

    const PARIS: (f64, f64) = (48.8566, 2.3522);

    /// Toujours au soleil quand le soleil est levé : le profil ne mesure alors
    /// que la durée du jour.
    fn open_sky(days: &[SampledDay]) -> ExposureProfile {
        build_profile(days, |d, i| {
            let sun = sun_position(days[d].instants[i].unix, PARIS.0, PARIS.1);
            if sun.is_up() { Sample::Sunlit } else { Sample::Night }
        })
    }

    #[test]
    fn default_sampling_covers_every_day() {
        assert_eq!(sampled_days(DEFAULT_TZ, REFERENCE_YEAR, DEFAULT_DAY_STRIDE).len(), 365);
    }

    #[test]
    fn stride_seven_gives_52_days_starting_jan_4() {
        let days = sampled_days(DEFAULT_TZ, REFERENCE_YEAR, 7);
        assert_eq!(days.len(), 52);
        assert_eq!(days[0].month, 1);
        // 4 janvier, 00:05 heure de Paris (UTC+1) = 3 janvier 23:05 UTC.
        let first = days[0].instants[0];
        let expected = chrono::Utc.with_ymd_and_hms(2027, 1, 3, 23, 5, 0).unwrap();
        assert_eq!(first.unix as i64, expected.timestamp());
        assert_eq!(first.slot, 0);
        assert_eq!(days[0].instants.len(), 144);
    }

    #[test]
    fn dst_days_have_23_and_25_hours_in_local_slots() {
        let days = sampled_days(DEFAULT_TZ, REFERENCE_YEAR, 1);
        assert_eq!(days.len(), 365);
        // 2027 : passage à l'heure d'été le 28 mars, retour le 31 octobre.
        let march_28 = &days[NaiveDate::from_ymd_opt(2027, 3, 28).unwrap().ordinal0() as usize];
        let oct_31 = &days[NaiveDate::from_ymd_opt(2027, 10, 31).unwrap().ordinal0() as usize];
        assert_eq!(march_28.instants.len(), 138);
        assert_eq!(oct_31.instants.len(), 150);
        // Le 28 mars, 02:00–03:00 locale n'existe pas : aucune tranche 4 ou 5.
        assert!(march_28.instants.iter().all(|i| i.slot != 4 && i.slot != 5));
        // Le 31 octobre, 02:00–03:00 locale passe deux fois : 12 instants.
        assert_eq!(oct_31.instants.iter().filter(|i| i.slot == 4 || i.slot == 5).count(), 12);
        // La dernière tranche est toujours 23:30.
        assert_eq!(march_28.instants.last().unwrap().slot, 47);
        assert_eq!(oct_31.instants.last().unwrap().slot, 47);
    }

    #[test]
    fn open_sky_sun_equals_daylight_and_follows_seasons() {
        let days = sampled_days(DEFAULT_TZ, REFERENCE_YEAR, DEFAULT_DAY_STRIDE);
        let p = open_sky(&days);
        assert_eq!(p.months.len(), 12);
        for m in &p.months {
            assert_eq!(m.avg_sunlit_minutes, m.avg_daylight_minutes, "mois {}", m.month);
            assert_eq!(m.slots.len(), SLOTS_PER_DAY);
        }
        // Paris : ~16 h de jour en juin, ~8 h 15 en décembre.
        let june = &p.months[5];
        let december = &p.months[11];
        assert!((950..=1000).contains(&june.avg_daylight_minutes), "juin {}", june.avg_daylight_minutes);
        assert!((480..=520).contains(&december.avg_daylight_minutes), "déc {}", december.avg_daylight_minutes);
        // En heure légale : plein soleil à 14 h l'été, nuit à 2 h, et en
        // décembre la tranche 17:30 est déjà la nuit.
        assert_eq!(june.slots[28], 100);
        assert_eq!(june.slots[4], 0);
        assert_eq!(december.slots[35], 0);
        assert_eq!(december.slots[26], 100);
        // L'annuelle est pondérée par les jours : entre les deux extrêmes.
        assert!((700..=760).contains(&p.annual_avg_daylight_minutes), "{}", p.annual_avg_daylight_minutes);
        assert_eq!(p.annual_avg_sunlit_minutes, p.annual_avg_daylight_minutes);
    }

    #[test]
    fn shadow_counts_as_daylight_not_sun() {
        let days = sampled_days(DEFAULT_TZ, REFERENCE_YEAR, DEFAULT_DAY_STRIDE);
        let p = build_profile(&days, |d, i| {
            let sun = sun_position(days[d].instants[i].unix, PARIS.0, PARIS.1);
            if sun.is_up() { Sample::Shadow } else { Sample::Night }
        });
        assert_eq!(p.annual_avg_sunlit_minutes, 0);
        assert!(p.annual_avg_daylight_minutes > 700);
        assert!(p.months.iter().all(|m| m.slots.iter().all(|&s| s == 0)));
    }

    #[test]
    fn tz_parsing() {
        assert_eq!(parse_tz(None).unwrap(), DEFAULT_TZ);
        assert_eq!(parse_tz(Some("")).unwrap(), DEFAULT_TZ);
        assert_eq!(parse_tz(Some("America/Montreal")).unwrap(), chrono_tz::America::Montreal);
        assert!(parse_tz(Some("Mars/Olympus")).is_err());
    }
}
