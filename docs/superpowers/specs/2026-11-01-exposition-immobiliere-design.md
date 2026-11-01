# Exposition immobilière : profil annuel d'un point et d'une façade

Date : 2026-11-01 — statut : validé en brainstorming, implémentation serveur
dans la foulée.

## Intention

Ouvrir SunMap à l'**immobilier** : dire combien de soleil direct reçoit un
appartement (une façade, un étage) ou un extérieur (jardin, cour, terrasse)
**sur toute l'année**, et à quel moment de la journée.

- **Pour qui** : en priorité les particuliers acheteurs/locataires et les
  agents immobiliers, **dans les apps iOS et Android** (pas de web, pas de
  PDF). L'API reste propre et documentée pour d'éventuels clients B2B plus
  tard, sans UI de notre côté.
- **Ce qu'on mesure (v1)** : la **durée de soleil direct**, rien d'autre — pas
  d'irradiance, pas de score énergétique.
- **Critère de succès** : en deux taps sur un immeuble, l'utilisateur voit une
  grille étages × tronçons de la façade choisie, puis pour une case la moyenne
  annuelle, les 12 moyennes mensuelles et la heatmap mois × heure.

## Principes directeurs

- **Le serveur est la vérité soleil/ombre** : même DSM (relief + bâtiments +
  canopée) et même ray marching que `/places` et `/sun-hours`. Aucun calcul
  d'ombre dans les clients.
- **Façades dérivées de notre archive** `sunmap.pmtiles`, jamais des bâtiments
  Mapbox affichés : une façade proposée doit exister dans les données qui
  calculent.
- **Calcul à la demande + cache**. Pas de pré-calcul massif (des millions de
  façades jamais consultées, incompatible avec la correction d'étages).
- **Trois routes nouvelles, rien de modifié** : conforme à la règle de
  rétrocompatibilité (`AGENTS.md`). `/places` n'embarque **pas** le profil
  annuel : il n'est calculé que sur demande explicite (« Voir sur l'année »).

## Modèle : bâtiment → façades → grille

### Sélection du bâtiment

Le client envoie le point tapé. Le serveur lit les emprises voisines
(`vstore.buildings`) et retient celle qui **contient** le point (règle
pair-impair, comme la rasterisation). Plusieurs candidates (`building` et ses
`building:part`) → la plus grande emprise, qui porte le contour extérieur.
Limite : une tour sur socle est vue comme un seul volume.

### Façades

- Chaque arête d'anneau est candidate, **anneaux intérieurs compris** → les
  façades sur cour (`courtyard: true`).
- Arêtes consécutives **fusionnées** tant que leur direction reste à moins de
  15° de la corde du groupe ; le parcours démarre sur un vrai angle pour ne
  pas couper une façade en deux au sommet de départ de l'anneau.
- Façades de moins de **3 m** écartées.
- **Normale sortante** : celle des deux normales dont un point décalé de
  0,5 m tombe **hors** du bâtiment (pair-impair) — vaut pour l'extérieur
  comme pour une cour. Orientation = azimut de cette normale (degrés depuis
  le nord, horaire), libellé en 8 directions (`N`, `NE`, … `NO`).
- **Murs mitoyens exclus** : chaque tronçon est testé à ~2 m à l'extérieur, au
  niveau du sol. Si la majorité des tronçons tombe dans un **autre** bâtiment
  plus haut que le rez-de-chaussée, la façade est mitoyenne et n'est pas
  proposée. Une case isolée dont le point d'observation est sous le toit d'un
  voisin accolé est rendue **aveugle** (`profile: null`).
- Ordre et index des façades : déterministes (anneaux dans l'ordre de
  l'archive, arêtes dans l'ordre du polygone).

### Grille d'une façade

- **Étages** : `levels` fourni par le client (1 à 60), sinon déduit de la
  hauteur : `max(1, round((height_m − 3) / 3))`, signalé
  `levels_estimated: true`. Le nombre d'étages OSM n'est pas conservé dans
  l'archive (choix assumé : pas de régénération).
- RDC = étage 0. Hauteur d'observation de l'étage *n* :
  `n × 3 m + 1,5 m` au-dessus du sol.
- **Tronçons** : `clamp(round(longueur / 8 m), 1, 6)` colonnes, point
  d'échantillonnage au milieu de chacune (abscisse curviligne).
- Le point de départ du rayon est **décalé de 2 m vers l'extérieur** pour ne
  pas heurter son propre mur (même piège que `nudge_out_of_building`).
- **Soleil dans le dos** (azimut solaire à 90° ou plus de la normale) : ombre,
  sans lancer de rayon.

Hors v1 : balcons/retraits, hauteur d'étage réelle, surfaces vitrées.

## API

Routes publiques (même régime que `/sun-hours` : clé d'API globale si
configurée, pas de compte).

### `GET /buildings/at?lat=&lng=`

Calcul léger, sans ray marching.

```json
{
  "id": "way/123456", "name": null,
  "height_m": 21.0, "height_from_osm": false,
  "levels": 6, "levels_estimated": true,
  "facades": [
    { "index": 0, "azimuth_deg": 172.4, "orientation": "S",
      "length_m": 18.2, "courtyard": false,
      "line": [[48.85, 2.34], [48.85, 2.341]] }
  ]
}
```

`line` : `[lat, lng]`, pour surligner la façade sur la carte.

### `GET /exposure/facade?lat=&lng=&building=&facade=[&levels=][&tz=]`

`lat/lng` retrouvent le bâtiment, `building` vérifie que c'est le même
(409 sinon : l'archive a changé entre les deux appels).

```json
{
  "building": "way/123456", "facade": 0,
  "levels": 7, "levels_estimated": false, "columns": 3,
  "tz": "Europe/Paris", "vegetation_seasonal": false,
  "cells": [
    { "level": 0, "column": 0, "blind": false, "profile": { } }
  ]
}
```

Toutes les cases, profils complets, en une réponse : le tap sur une case est
instantané côté app.

### `GET /exposure/point?lat=&lng=[&height=1.5][&tz=]` ou `?place=<osm_id>[&tz=]`

Profil annuel d'un point au sol (`height` = hauteur d'observation, 0 à 200 m,
défaut 1,5). Avec `place=`, la position est **exactement** celle de `/places`
(terrasse contribuée telle quelle, mobilier tel quel, sinon recalage hors
bâtiment) — sinon le profil annuel contredirait la classification du jour.

```json
{
  "lat": 48.85, "lng": 2.34, "height": 1.5,
  "place": null, "tz": "Europe/Paris", "vegetation_seasonal": false,
  "profile": { }
}
```

### `profile` (commun)

```json
{
  "annual_avg_sunlit_minutes": 292,
  "annual_avg_daylight_minutes": 731,
  "months": [
    { "month": 1, "avg_sunlit_minutes": 74, "avg_daylight_minutes": 522,
      "slots": [0, 0, 50, 100, 33] }
  ]
}
```

- `slots` : **48** tranches de 30 min en **heure légale locale**
  (00:00 → 23:30), valeur = % du temps au soleil dans la tranche (0–100),
  moyenné sur les jours échantillonnés du mois.
- `tz` : fuseau IANA, défaut `Europe/Paris` (toute la couverture actuelle).
  Le changement d'heure est géré **côté serveur** (`chrono-tz`), une fois,
  plutôt qu'en miroir dans iOS et Android.

### Erreurs

| Code | Cas |
|------|-----|
| 400 | coordonnées hors bornes, `levels` hors 1–60, `height` hors 0–200, `tz` inconnu, ni `lat/lng` ni `place` |
| 404 | aucun bâtiment au point, index de façade inexistant, façade mitoyenne, `place` inconnu |
| 409 | `building` ne correspond plus au bâtiment trouvé au point |
| 502 | DEM indisponible (comme les autres routes) |

Codes **propres à ces nouvelles routes** : aucune version en ligne ne les
appelle, donc aucun contrat existant n'est modifié.

## Calcul

### Organisation du code

- `helios-server/src/facade.rs` (lib, **pure**) : bâtiment contenant un point,
  extraction/fusion des façades, normale, libellé, grille de points,
  détection mitoyenne via une fonction d'occupation fournie par l'appelant.
- `helios-server/src/exposure.rs` (lib, **pure**) : jours et instants
  échantillonnés dans un fuseau, agrégation en `profile` à partir d'un
  classifieur `instant → soleil/ombre/nuit` reçu en paramètre.
- `helios-server/src/exposure_api.rs` (module du binaire) : handlers axum,
  branchés sur `assemble_point`, la grille `owner` et `shadow_hit_from_ground`
  — le même moteur que `/places`. Calcul CPU dans `spawn_blocking`.
- `helios-core` inchangé.

### Échantillonnage temporel

- **Les 365 jours** d'une **année de référence fixe** (2027, non bissextile) :
  résultat indépendant de la date de requête, donc cachable ; l'écart de
  position solaire d'une année à l'autre est négligeable.
- Tranches de **10 min** (pas de `sun_day`), échantillonnées en leur milieu.
- Chaque jour est parcouru de **minuit local à minuit local** (23 h ou 25 h les
  jours de bascule) ; chaque instant tombe dans sa tranche locale de 30 min.
- **Révisé à l'implémentation** : le brainstorming prévoyait un jour sur 7,
  validé par un test de précision (< 5 min/jour). Le test a échoué : dans une
  rue étroite, l'écart mensuel atteint 34 min/jour (12 min encore à un jour
  sur 2) — une ombre de bâtiment bascule brutalement quand le soleil franchit
  une ligne de toit, et les jours sautés ratent ces bascules. Mesuré en
  release, le calcul complet coûte ~0,2 s de CPU pour 36 cases (780 000
  rayons), réparti sur les cœurs : aucune raison d'approximer.

### Coût

Point : 365 × ~70 tranches de jour ≈ 25 000 rayons. Façade 6 × 6 : ≈ 780 000
rayons au pire (~0,2 s de CPU, parallélisé par case), la moitié environ
écartée sans rayon (soleil dans le dos). L'assemblage DSM (tuiles DEM en
cache serveur) reste du même ordre.

### Cache

- En mémoire, borné (2 000 entrées, éviction FIFO). Clés : façade
  `building + index + levels + tz` ; point `lat/lng à 1e-5 + height + tz` ;
  lieu `place + tz`.
- L'archive étant chargée au démarrage, un import (redémarrage) vide
  naturellement le cache.
- `Cache-Control: public, max-age=86400`, prêt pour un CDN.

### Limites assumées

- **Végétation feuillue toute l'année** (pas de τ saisonnier) : le soleil
  d'hiver derrière un feuillu est **sous-estimé**. Exposé par
  `vegetation_seasonal: false`. Le τ saisonnier par `leaf_type` (backlog)
  devient prioritaire pour ce marché.
- Relief lointain au-delà de la marge DSM (~800 m), comme `/sun-hours`.
- Hauteur des bâtiments parfois devinée (`height_from_osm: false`).

## Expérience mobile (iOS et Android)

Implémentée dans les repos clients ; ce contrat fait référence.

**Entrées**
- **Tap simple sur un polygone de bâtiment** (hit-test client
  `queryRenderedFeatures` sur la couche bâtiments) → `/buildings/at`. Les
  pastilles de lieux restent prioritaires. 404 (bâtiment Mapbox absent de
  notre archive) → proposer l'analyse du point.
- **Appui long** sur le sol → `/exposure/point`.
- **Fiche d'un lieu** → bouton « Voir sur l'année » → `/exposure/point?place=`.
  Jamais chargé par défaut.

**Parcours bâtiment**
1. Fiche bâtiment (bottom sheet) : nom, hauteur, étages (« estimé »), liste
   des façades (`Sud · rue · 18 m`, `Nord · cour · 12 m`), façades surlignées
   sur la carte ; sélection par la liste ou sur la carte.
2. Vue façade : élévation schématique (dernier étage en haut), cases colorées
   par moyenne annuelle (échelle séquentielle + légende + valeur), cases
   aveugles grisées ; sélecteur « Étages : 6 (estimé) − / + » qui relance avec
   `levels`.
3. Détail d'une case.

**Composant de détail partagé** (case, point, lieu) : chiffre clé (« 4 h 52 de
soleil par jour en moyenne »), 12 barres soleil vs durée du jour, heatmap
mois × heure, mentions de fiabilité (hauteur estimée, végétation), résumé
texte pour VoiceOver/TalkBack.

**Clean Architecture** : domaine (`Building`, `Facade`, `ExposureProfile`,
`FacadeGrid` ; cas d'usage `GetBuildingAt`, `GetFacadeExposure`,
`GetPointExposure`, `GetPlaceExposure`), data (`ExposureRepository`, DTO →
domaine), présentation (un ViewModel par écran, composant de détail réutilisé).

**Erreurs côté app** : 404 bâtiment → analyse du point ; 409 → relance
silencieuse unique de `/buildings/at` puis de la façade ; 502 → « Calcul
indisponible, réessayer » ; chargement → squelette de grille.

**Hors v1** : partage/export, comparaison d'appartements, persistance des
corrections d'étages, remontée vers OSM, τ saisonnier.

## Tests

- `facade.rs` : rectangle → 4 façades N/E/S/O ; anneau intérieur → façades
  `courtyard` normale vers la cour ; sommets quasi alignés fusionnés ; façade
  courte écartée ; deux bâtiments accolés → mur commun exclu ; étages déduits
  vs fournis ; point de départ hors de l'emprise ; sélection de la plus grande
  emprise contenant le point.
- `exposure.rs` : classifieur « toujours au soleil » → soleil = durée du jour ;
  jours de 23 h / 25 h correctement rangés ; moyennes mensuelles ; 365 jours
  par défaut.
- Calcul de bout en bout sans réseau : `find_target` sur `mini.pmtiles`, et
  grille de façade sur une scène synthétique tamponnée par `stamp_buildings`
  (soleil croissant avec l'étage face à un immeuble, façade nord sans soleil
  en décembre).
- Handlers : validation des paramètres (fonctions pures : bornes, `levels`, `tz`). Les handlers eux-mêmes demandent PostgreSQL et les tuiles DEM, et sont vérifiés à la main sur un serveur de dev.
