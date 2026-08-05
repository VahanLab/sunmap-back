# helios-core — le moteur soleil/ombre

Répond à une seule question, et la répond vite : **« ce point est-il éclairé
par le soleil à cet instant ? »**

Trois briques, 850 lignes, **zéro dépendance** : position solaire (NOAA), un
Digital Surface Model (terrain + bâtiments + canopée), et un ray marching qui
marche du point vers le soleil.

```rust
use helios_core::{sun_position, Dsm, ShadowParams, shadow::is_shadowed_from_ground};

let sun = sun_position(1_782_043_200.0, 48.8566, 2.3522);  // Paris, solstice
let dsm = Dsm::from_terrarium_rgb(&rgb, 512, 512, 1.57);   // tuile DEM décodée
let ombre = is_shadowed_from_ground(&dsm, &sun, 256.0, 256.0, 35.0, &ShadowParams::default());
```

```bash
cargo test -p helios-core     # 14 tests, sans réseau ni base de données
```

## Ce dont il est responsable — et de rien d'autre

| Il fait | Il ne fait pas |
|---|---|
| Position solaire à ±0,01° | Chercher les tuiles DEM (c'est le serveur) |
| Décoder un buffer RGB Terrarium en altitudes | Décoder le PNG/WebP qui le contient |
| Marcher un rayon et dire soleil/ombre | Savoir si l'obstacle est un immeuble ou une colline |
| Atténuer la lumière à travers la canopée | Savoir si la canopée est un platane ou une futaie |
| Rendre un masque d'ombre plein | Le paralléliser, l'encoder, le servir |

La ligne de partage est toujours la même : **le cœur ne connaît que des
altitudes**. Nommer le bâtiment fautif, distinguer un arbre isolé d'une
emprise boisée, aller chercher la donnée — tout cela demande une grille
d'occupation et un réseau que seul l'appelant possède. `shadow_hit_from_ground`
renvoie donc les coordonnées `(x, y)` de la cellule bloquante, à charge pour le
serveur de dire à qui elle appartient.

### Pourquoi zéro dépendance

Le même code doit tourner **sur le serveur** (axum), **sur mobile** (via
UniFFI) et potentiellement **en WASM**. Chaque dépendance est un obstacle à
l'un des trois. Les dépendances utiles — `rayon` pour paralléliser le rendu,
`image` pour décoder les tuiles — sont listées en commentaire dans
[`Cargo.toml`](Cargo.toml) et vivent chez les binaires qui en ont besoin.

Effet de bord agréable : les tests tournent en 0,00 s, sans base, sans réseau,
sans fixture.

## Les trois modules

### `sun.rs` — position solaire

Algorithme NOAA (*General Solar Position Calculations*), précision ~0,01° sur
1900–2100. Pas de correction de réfraction atmosphérique : elle ne joue qu'à
l'horizon (< 0,6°) et ne déplace aucune ombre visible.

```rust
pub fn sun_position(unix_seconds: f64, lat_deg: f64, lon_deg: f64) -> SunPosition
pub struct SunPosition { pub azimuth_deg: f64, pub elevation_deg: f64 }
```

Azimut en degrés depuis le nord, sens horaire (0 = N, 90 = E, 180 = S).
Élévation au-dessus de l'horizon, négative quand le soleil est couché.

**Valeur de référence** — Paris (48,8566 N / 2,3522 E), 2026-06-21 12:00 UTC :
élévation ≈ 64,6°, azimut ≈ 180°. C'est le chiffre qui vérifie que le port
Swift `SunPosition.swift` (dépôt `sunmap-ios`) n'a pas divergé : **les deux
fichiers sont des ports exacts l'un de l'autre**, toute modification de l'un se
répercute sur l'autre, tests compris.

### `dsm.rs` — Digital Surface Model

Une grille d'altitudes en mètres, `x` vers l'est, `y` vers le sud (ligne 0 =
bord nord, comme une image raster).

| Méthode | Rôle |
|---|---|
| `Dsm::flat` | Grille uniforme — tests et zones plates |
| `Dsm::from_terrarium_rgb` | Décodage `alt = r·256 + g + b/256 − 32768` |
| `sample` | Altitude interpolée bilinéairement |
| `get` | Altitude d'une cellule, `None` hors grille |
| `max_elevation` | Point le plus haut — sert à l'arrêt anticipé du rayon |
| `stamp_max` | Tamponne un rectangle en gardant le max (bâtiment = sol + hauteur) |
| `canopy_layers_mut` / `canopy_at` | Couche de végétation, en écriture puis en lecture |

**La végétation ne vit pas dans `data`** : un arbre n'est pas un mur. Elle
occupe deux couches parallèles — `canopy_top` et `canopy_base`, en altitudes
absolues — que le ray marching traverse au lieu de heurter. Les couches ne sont
allouées qu'à la première végétation tamponnée ; sans elles, la boucle saute
toute la logique canopée.

`canopy_at` lit la **cellule entière**, sans interpolation, contrairement à
`sample` : la canopée est éparse, et interpoler entre une couronne et du vide
fabriquerait des demi-arbres fantômes autour de chaque houppier.

`stamp_max` ne rasterise qu'un **rectangle**. La rasterisation polygone réelle
(scanline, règle pair-impair, cours intérieures creuses) vit dans le serveur —
elle a besoin de la géométrie OSM, que le cœur ne connaît pas.

### `shadow.rs` — ray marching

On marche depuis le point vers le soleil, projeté au sol. À chaque pas, la
hauteur du rayon monte de `distance · tan(élévation)`. Si la DSM opaque dépasse
cette hauteur, un obstacle bloque : ombre.

```rust
pub fn is_shadowed(dsm, sun, px, py, params) -> bool
pub fn is_shadowed_from_ground(dsm, sun, px, py, ground, params) -> bool
pub fn shadow_hit_from_ground(…) -> Option<ShadowHit>          // qui bloque, et à quelle distance
pub fn shadow_causes_from_ground(…, on_cause) -> ShadowCauses  // tout ce qui ombre, sur tout le rayon
pub fn render_mask(dsm, sun, params) -> Vec<u8>                // 255 = ombre
```

**La canopée atténue, elle ne bloque pas.** Chaque mètre de couronne traversé
multiplie la lumière restante par `canopy_transmittance_per_m` (défaut 0,6/m) ;
le point passe à l'ombre quand la lumière tombe sous `sunlit_light_threshold`
(défaut 25 %). Un platane d'alignement laisse donc passer le soleil sur ses
bords, une futaie l'éteint. Le rayon passe librement **sous la base du
houppier**.

Ce n'est pas de la théorie : une terrasse à 1,3 m d'un platane
(`node/653366336`) passait de « soleil l'après-midi » à « 0 h par jour » avec
un tamponnage opaque.

**`from_ground` n'est pas une variante cosmétique.** L'altitude de l'observateur
est fournie séparément de la DSM parce qu'un POI OSM tombe parfois à
l'intérieur d'un bâtiment : sans cette séparation, le point hérite de
l'altitude du toit et « voit par-dessus » des obstacles qui devraient le
masquer.

**`causes` parcourt tout le rayon, `hit` s'arrête au premier obstacle.** L'arrêt
anticipé suffit à répondre « suis-je à l'ombre » et c'est lui qui rend la
classification en masse rapide. Il ne suffit pas à répondre « de QUOI suis-je à
l'ombre » : l'arbre au-dessus de la tête n'efface pas la montagne qui, plus
loin, ombre toute la vallée.

#### `ShadowParams`

| Champ | Défaut | Note |
|---|---|---|
| `max_distance_m` | 1 000 | 500 m suffisent en ville ; 2–5 km si le relief compte |
| `observer_height_m` | 0 | 1,5 m ≈ une personne attablée en terrasse |
| `step_px` | 1,0 | 0,5 double la précision et le coût |
| `canopy_transmittance_per_m` | 0,6 | Feuillu d'alignement en été ; 1,0 = transparent |
| `sunlit_light_threshold` | 0,25 | En deçà, le point est réputé à l'ombre |

#### Deux optimisations qui ont compté

- **`dsm_max_elevation` est un paramètre, pas un calcul.** Au-delà d'une
  certaine distance, même le point le plus haut de la grille passe sous le
  rayon : inutile de marcher plus loin. Mais `Dsm::max_elevation` scanne toute
  la grille — le recalculer à chaque appel a fait passer une requête `/places`
  de ~150 ms à ~2 s sur une zone dense (1074 lieux × un scan complet chacun).
  La grille ne bouge pas pendant une classification : un calcul en amont suffit.
- **`ln(τ)` une fois pour toutes.** Accumuler `light *= τ^d` pas à pas revient
  à sommer `d · ln(τ)` : une addition par pas au lieu d'un `powf`.

## Conventions

- Grille : `x` vers l'est, `y` vers le sud, ligne 0 = bord nord.
- Azimut : degrés depuis le nord, sens horaire. Élévation : degrés au-dessus de
  l'horizon.
- Longueur 3D d'un pas : `step_m / cos(élévation)` — au soleil haut, un mètre
  horizontal parcourt bien plus d'un mètre de rayon, et c'est cette
  longueur-là qui traverse la canopée.
- Commentaires en français.

## Tests

14 tests, tous déterministes. Ce qu'ils tiennent :

| Fichier | Ce qui est vérifié |
|---|---|
| `sun.rs` | Solstice parisien, soleil couché à minuit, azimut est le matin / ouest le soir |
| `dsm.rs` | Décodage Terrarium, échantillonnage bilinéaire, `stamp_max` |
| `shadow.rs` | Un mur de 10 m porte 10 m d'ombre à 45°, soleil bas → ombre plus longue, orientation de l'azimut, nuit, canopée qui atténue sans bloquer, rayon passant sous le houppier, canopée comptée dans `max_elevation`, hauteur d'observateur |

Le test du mur est le garde-fou principal : mur nord-sud de 10 m à `x=50`,
soleil plein ouest à 45°, l'ombre s'étend exactement sur 10 m à l'est.

## Qui l'utilise

- [`helios-server`](../helios-server/) — classification des lieux, bitfield
  `sun_day` (144 tranches de 10 min par lieu et par jour), frises `/sun-hours`,
  debug `/debug/ray`.
- `Shaders.metal` (dépôt `sunmap-ios`) — port du ray marching en Metal pour le
  masque d'ombre du terrain. **La DSM client est terrain seul** : ni bâtiments
  ni canopée, donc pas de logique de transmittance côté Metal.
