//! Digital Surface Model : grille d'altitudes fusionnant terrain et bâtiments,
//! plus une couche de canopée semi-transparente.
//!
//! Convention de grille : `x` croît vers l'est, `y` croît vers le sud
//! (ligne 0 = bord nord), comme une image raster classique.
//!
//! La fusion terrain + bâtiments se fait en amont du moteur : on part d'un DEM
//! (tuile Terrarium décodée) puis on « tamponne » chaque emprise de bâtiment à
//! `altitude_sol + hauteur` via [`Dsm::stamp_max`].
//!
//! La végétation ne vit PAS dans `data` : un arbre n'est pas un mur. Elle
//! occupe sa propre couche (`canopy_top`/`canopy_base`), que le ray marching
//! traverse en accumulant une atténuation au lieu de s'arrêter — cf.
//! `shadow.rs`.

/// Grille d'altitudes en mètres.
#[derive(Debug, Clone)]
pub struct Dsm {
    pub width: usize,
    pub height: usize,
    /// Taille d'une cellule en mètres (résolution au sol).
    pub meters_per_pixel: f64,
    /// Altitudes des obstacles **opaques** (terrain + bâtiments), ordre
    /// row-major, `data[y * width + x]`.
    pub data: Vec<f32>,
    /// Sommet de canopée par cellule (altitude absolue), `f32::NEG_INFINITY`
    /// là où il n'y a pas de végétation. `None` tant qu'aucune végétation n'a
    /// été tamponnée — le ray marching saute alors toute la logique canopée.
    pub canopy_top: Option<Vec<f32>>,
    /// Base de la couronne (altitude absolue) : un rayon qui passe sous elle
    /// — sous le houppier d'un arbre d'alignement — ne traverse rien.
    /// Toujours présent si `canopy_top` l'est.
    pub canopy_base: Option<Vec<f32>>,
}

impl Dsm {
    /// Grille uniforme (pratique pour les tests et les zones plates).
    pub fn flat(width: usize, height: usize, meters_per_pixel: f64, elevation: f32) -> Self {
        Self {
            width,
            height,
            meters_per_pixel,
            data: vec![elevation; width * height],
            canopy_top: None,
            canopy_base: None,
        }
    }

    /// Décode une tuile Terrarium (RGB brut, row-major, 3 octets/pixel).
    ///
    /// Encodage AWS/Mapzen : `altitude = r·256 + g + b/256 − 32768`.
    /// Le décodage PNG reste à la charge de l'appelant (crate `image` côté
    /// serveur) pour garder ce cœur sans dépendance.
    pub fn from_terrarium_rgb(
        rgb: &[u8],
        width: usize,
        height: usize,
        meters_per_pixel: f64,
    ) -> Self {
        assert_eq!(rgb.len(), width * height * 3, "buffer RGB de taille invalide");
        let data = rgb
            .chunks_exact(3)
            .map(|p| (p[0] as f32) * 256.0 + (p[1] as f32) + (p[2] as f32) / 256.0 - 32_768.0)
            .collect();
        Self {
            width,
            height,
            meters_per_pixel,
            data,
            canopy_top: None,
            canopy_base: None,
        }
    }

    /// Altitude d'une cellule, `None` hors grille.
    #[inline]
    pub fn get(&self, x: isize, y: isize) -> Option<f32> {
        if x < 0 || y < 0 || x as usize >= self.width || y as usize >= self.height {
            return None;
        }
        Some(self.data[y as usize * self.width + x as usize])
    }

    /// Échantillonnage bilinéaire en coordonnées pixel fractionnaires.
    /// `None` si le point sort de la grille.
    pub fn sample(&self, x: f64, y: f64) -> Option<f32> {
        if x < 0.0 || y < 0.0 || x > (self.width - 1) as f64 || y > (self.height - 1) as f64 {
            return None;
        }
        let x0 = x.floor() as usize;
        let y0 = y.floor() as usize;
        let x1 = (x0 + 1).min(self.width - 1);
        let y1 = (y0 + 1).min(self.height - 1);
        let fx = (x - x0 as f64) as f32;
        let fy = (y - y0 as f64) as f32;

        let top = self.data[y0 * self.width + x0] * (1.0 - fx) + self.data[y0 * self.width + x1] * fx;
        let bot = self.data[y1 * self.width + x0] * (1.0 - fx) + self.data[y1 * self.width + x1] * fx;
        Some(top * (1.0 - fy) + bot * fy)
    }

    /// Altitude maximale de la grille — borne l'horizon de recherche du ray
    /// marching. Canopée comprise : une futaie qui dépasse les toits doit
    /// pouvoir arrêter un rayon au-delà du plus haut bâtiment.
    pub fn max_elevation(&self) -> f32 {
        let solid = self.data.iter().copied().fold(f32::MIN, f32::max);
        match &self.canopy_top {
            Some(top) => top
                .iter()
                .copied()
                .filter(|t| t.is_finite())
                .fold(solid, f32::max),
            None => solid,
        }
    }

    /// Alloue les couches de canopée si besoin, et les renvoie en écriture.
    pub fn canopy_layers_mut(&mut self) -> (&mut [f32], &mut [f32]) {
        let len = self.width * self.height;
        if self.canopy_top.is_none() {
            self.canopy_top = Some(vec![f32::NEG_INFINITY; len]);
            self.canopy_base = Some(vec![f32::NEG_INFINITY; len]);
        }
        (
            self.canopy_top.as_mut().unwrap(),
            self.canopy_base.as_mut().unwrap(),
        )
    }

    /// Couronne de canopée à la cellule contenant `(x, y)` : `(base, sommet)`
    /// en altitudes absolues, `None` sans végétation ici.
    ///
    /// Cellule entière et non bilinéaire, contrairement à `sample` : la
    /// canopée est éparse, et interpoler entre une couronne et du vide
    /// fabriquerait des demi-arbres fantômes autour de chaque houppier.
    #[inline]
    pub fn canopy_at(&self, x: f64, y: f64) -> Option<(f32, f32)> {
        let top = self.canopy_top.as_ref()?;
        let base = self.canopy_base.as_ref()?;
        if x < 0.0 || y < 0.0 {
            return None;
        }
        let (cx, cy) = (x.round() as usize, y.round() as usize);
        if cx >= self.width || cy >= self.height {
            return None;
        }
        let i = cy * self.width + cx;
        let t = top[i];
        if !t.is_finite() {
            return None;
        }
        Some((base[i], t))
    }

    /// « Tamponne » une altitude sur un rectangle de cellules en gardant le max
    /// (utilisé pour rasteriser une emprise de bâtiment : `sol + hauteur`).
    /// La rasterisation de polygones réels (scanline) viendra dans le pipeline
    /// data ; le rectangle suffit pour les tests et les POC.
    pub fn stamp_max(&mut self, x0: usize, y0: usize, x1: usize, y1: usize, elevation: f32) {
        for y in y0..=y1.min(self.height - 1) {
            for x in x0..=x1.min(self.width - 1) {
                let cell = &mut self.data[y * self.width + x];
                if *cell < elevation {
                    *cell = elevation;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terrarium_decoding() {
        // 100 m → 100 + 32768 = 32868 = 128·256 + 100 + 0/256
        let rgb = [128u8, 100, 0];
        let dsm = Dsm::from_terrarium_rgb(&rgb, 1, 1, 1.0);
        assert!((dsm.data[0] - 100.0).abs() < 1e-3, "décodé: {}", dsm.data[0]);

        // Niveau de la mer : 32768 = 128·256 + 0
        let rgb = [128u8, 0, 0];
        let dsm = Dsm::from_terrarium_rgb(&rgb, 1, 1, 1.0);
        assert!(dsm.data[0].abs() < 1e-3);
    }

    #[test]
    fn bilinear_sampling() {
        let mut dsm = Dsm::flat(2, 2, 1.0, 0.0);
        dsm.data = vec![0.0, 10.0, 0.0, 10.0]; // gradient ouest→est
        assert!((dsm.sample(0.5, 0.5).unwrap() - 5.0).abs() < 1e-4);
        assert!(dsm.sample(-0.1, 0.0).is_none());
    }

    #[test]
    fn stamp_keeps_max() {
        let mut dsm = Dsm::flat(4, 4, 1.0, 50.0);
        dsm.stamp_max(1, 1, 2, 2, 60.0); // bâtiment de 10 m sur un sol à 50 m
        dsm.stamp_max(1, 1, 1, 1, 40.0); // plus bas que l'existant → ignoré
        assert_eq!(dsm.get(1, 1), Some(60.0));
        assert_eq!(dsm.get(0, 0), Some(50.0));
    }
}
