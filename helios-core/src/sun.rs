//! Position solaire — algorithme NOAA (General Solar Position Calculations).
//! Précision ~0.01° sur 1900–2100, largement suffisant pour des ombres urbaines.
//! Pas de correction de réfraction atmosphérique : elle ne joue qu'à l'horizon
//! (< 0.6°) et n'a aucun impact visible sur une ombre portée.

/// Position du soleil vue depuis un point du globe.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SunPosition {
    /// Azimut en degrés, depuis le nord, sens horaire (0 = N, 90 = E, 180 = S).
    pub azimuth_deg: f64,
    /// Élévation en degrés au-dessus de l'horizon (négatif = soleil couché).
    pub elevation_deg: f64,
}

impl SunPosition {
    /// Le soleil est-il levé ?
    pub fn is_up(&self) -> bool {
        self.elevation_deg > 0.0
    }
}

/// Calcule la position du soleil.
///
/// * `unix_seconds` — timestamp Unix UTC (secondes, fractions acceptées)
/// * `lat_deg` / `lon_deg` — latitude/longitude en degrés (est positif)
pub fn sun_position(unix_seconds: f64, lat_deg: f64, lon_deg: f64) -> SunPosition {
    let rad = std::f64::consts::PI / 180.0;

    // Jour julien puis siècles juliens depuis J2000.
    let jd = unix_seconds / 86_400.0 + 2_440_587.5;
    let t = (jd - 2_451_545.0) / 36_525.0;

    // Longitude moyenne géométrique du soleil (deg, ramenée dans [0, 360)).
    let l0 = (280.46646 + t * (36_000.76983 + t * 0.000_303_2)).rem_euclid(360.0);
    // Anomalie moyenne (deg).
    let m = 357.52911 + t * (35_999.05029 - t * 0.000_153_7);
    // Excentricité de l'orbite terrestre.
    let e = 0.016_708_634 - t * (0.000_042_037 + t * 0.000_000_126_7);

    // Équation du centre (deg).
    let m_rad = m * rad;
    let c = (1.914_602 - t * (0.004_817 + t * 0.000_014)) * m_rad.sin()
        + (0.019_993 - t * 0.000_101) * (2.0 * m_rad).sin()
        + 0.000_289 * (3.0 * m_rad).sin();

    // Longitude vraie puis apparente (deg).
    let true_long = l0 + c;
    let omega = 125.04 - 1_934.136 * t;
    let app_long = true_long - 0.005_69 - 0.004_78 * (omega * rad).sin();

    // Obliquité moyenne puis corrigée (deg).
    let seconds = 21.448 - t * (46.8150 + t * (0.000_59 - t * 0.001_813));
    let obliq0 = 23.0 + (26.0 + seconds / 60.0) / 60.0;
    let obliq = obliq0 + 0.002_56 * (omega * rad).cos();

    // Déclinaison solaire (rad).
    let decl = ((obliq * rad).sin() * (app_long * rad).sin()).asin();

    // Équation du temps (minutes).
    let y = (obliq * rad / 2.0).tan().powi(2);
    let l0_rad = l0 * rad;
    let eot_rad = y * (2.0 * l0_rad).sin() - 2.0 * e * m_rad.sin()
        + 4.0 * e * y * m_rad.sin() * (2.0 * l0_rad).cos()
        - 0.5 * y * y * (4.0 * l0_rad).sin()
        - 1.25 * e * e * (2.0 * m_rad).sin();
    let eot_min = 4.0 * eot_rad / rad;

    // Temps solaire vrai (minutes depuis minuit) puis angle horaire (deg).
    let minutes_utc = unix_seconds.rem_euclid(86_400.0) / 60.0;
    let tst = (minutes_utc + eot_min + 4.0 * lon_deg).rem_euclid(1_440.0);
    let hour_angle = tst / 4.0 - 180.0; // < 0 le matin, > 0 l'après-midi

    // Élévation.
    let lat = lat_deg * rad;
    let ha = hour_angle * rad;
    let sin_elev = lat.sin() * decl.sin() + lat.cos() * decl.cos() * ha.cos();
    let elevation = sin_elev.clamp(-1.0, 1.0).asin();

    // Azimut depuis le nord, sens horaire.
    // atan2(sin H, cos H·sin φ − tan δ·cos φ) donne l'azimut depuis le sud,
    // positif vers l'ouest → +180° pour le repère « depuis le nord ».
    let az_south = (ha.sin()).atan2(ha.cos() * lat.sin() - decl.tan() * lat.cos());
    let azimuth = (az_south / rad + 180.0).rem_euclid(360.0);

    SunPosition {
        azimuth_deg: azimuth,
        elevation_deg: elevation / rad,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 2026-06-21T12:00:00Z — quasi solstice d'été, Paris (48.8566 N, 2.3522 E).
    /// Le midi solaire local tombe vers 11h52 UTC : à 12h00 UTC le soleil est
    /// à peine passé au sud, élévation proche du maximum ≈ 90 − 48.86 + 23.44 ≈ 64.6°.
    #[test]
    fn paris_summer_solstice_noon() {
        let ts = 1_782_043_200.0; // 2026-06-21 12:00:00 UTC
        let p = sun_position(ts, 48.8566, 2.3522);
        assert!(
            (63.0..=66.0).contains(&p.elevation_deg),
            "élévation inattendue: {}",
            p.elevation_deg
        );
        assert!(
            (175.0..=195.0).contains(&p.azimuth_deg),
            "azimut inattendu: {}",
            p.azimuth_deg
        );
        assert!(p.is_up());
    }

    /// Même lieu à minuit UTC : soleil largement sous l'horizon.
    #[test]
    fn paris_midnight_sun_down() {
        let ts = 1_782_000_000.0; // 2026-06-21 00:00:00 UTC
        let p = sun_position(ts, 48.8566, 2.3522);
        assert!(p.elevation_deg < -5.0, "élévation: {}", p.elevation_deg);
        assert!(!p.is_up());
    }

    /// Le matin le soleil est à l'est (azimut < 180), l'après-midi à l'ouest.
    #[test]
    fn azimuth_morning_vs_afternoon() {
        let morning = sun_position(1_782_000_000.0 + 8.0 * 3600.0, 48.8566, 2.3522); // 08h UTC
        let evening = sun_position(1_782_000_000.0 + 17.0 * 3600.0, 48.8566, 2.3522); // 17h UTC
        assert!(morning.azimuth_deg < 180.0, "matin: {}", morning.azimuth_deg);
        assert!(evening.azimuth_deg > 180.0, "soir: {}", evening.azimuth_deg);
    }
}
