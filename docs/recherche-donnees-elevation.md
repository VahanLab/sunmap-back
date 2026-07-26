# Données d'élévation : ce qui existe, ce qu'on a retenu

Recherche menée le **26 juillet 2026**, en réponse à la question : *existe-t-il
un DSM public mondial assez précis pour remplacer notre construction de DSM
(DEM + emprises rasterisées) ?*

**Réponse courte : non, et l'écart est d'un facteur ~20.** Ce document garde le
raisonnement pour éviter de refaire la recherche, et note ce qui vaudrait le
coup d'être intégré plus tard.

## Rappel : ce qu'on construit aujourd'hui

Une DSM (Digital Surface Model — le sol *plus* ce qui est posé dessus) est
assemblée au runtime, par zone :

1. tuiles DEM Mapterhorn z15 (~1,57 m/px à Paris) décodées en grille de f32 ;
2. emprises OSM lues dans PostGIS et rasterisées par-dessus (`sol + hauteur`).

Voir `helios-server/README.md` pour les coûts mesurés.

## DSM raster mondiaux

| Source | Résolution | Millésime | Licence |
|---|---|---|---|
| [Copernicus DEM GLO-30](https://dataspace.copernicus.eu/explore-data/data-collections/copernicus-contributing-missions/collections-description/COP-DEM) | **30 m** | TanDEM-X 2011-2015 | libre |
| ALOS World 3D (AW3D30), JAXA | **30 m** | — | libre |
| [Maxar Precision3D](https://resources.maxar.com/data-sheets/precision3d-3d-surface-model-data-sheet) | **50 cm** | récent | **commercial** |

Les deux gratuits sont de vrais DSM (bâtiments et végétation inclus), RMSE
verticale ~4 m.

**Pourquoi 30 m est rédhibitoire.** Une rue parisienne fait 12 m de large. Notre
grille est déjà à 1,57 m et peine sur les cours de 6 m (constaté sur
`relation/2779974`). À 30 m, un îlot entier tient dans une cellule : il ne
reste aucune géométrie d'ombre exploitable. La donnée date en plus de
2011-2015.

**Second problème, moins évident.** Un DSM raster n'a aucune notion d'objet. On
perdrait ce qui a été construit en juillet 2026 : l'identifiant du bloqueur
(`way/…`, `relation/…`), la distinction hauteur taggée / hauteur estimée, et la
possibilité de corriger un bâtiment isolé. On échangerait de la traçabilité
contre une résolution qu'on n'aurait même pas.

Seul Maxar Precision3D (50 cm mondial, 3 m SE90) est la vraie chose. Payant,
tarif non public — à considérer seulement si le produit prend.

## L'état de l'art fait comme nous

[ShadeMap](https://tedpiotrowski.svbtle.com/sun-and-shadow-maps-models-vs-reality),
la référence du domaine, assemble exactement la même chose : bâtiments Overture
(OSM + ML Google/Microsoft/Amazon/TomTom), hauteur par défaut quand le tag
manque (3,1 m), canopée par estimation ML (±3 m). Le LiDAR y est vendu en
**premium**. Autrement dit, ils ont conclu la même chose : aucun DSM mondial
gratuit ne convient.

C'est une validation de l'architecture, pas un hasard.

## Ce qui vaut le coup d'être intégré : les hauteurs par bâtiment

Pas un DSM, mais des hauteurs ML par emprise — ça s'insère directement dans la
colonne `buildings.height_m` avec `height_from_osm = false`, sans toucher au
moteur.

| Dataset | Bâtiments | Hauteurs | Précision | Licence |
|---|---|---|---|---|
| [GlobalBuildingAtlas](https://essd.copernicus.org/articles/17/6647/2025/essd-17-6647-2025.html) (ESSD 2025) | 2,75 Md | 97,7 % | RMSE 1,5 m (Océanie) à 8,9 m (Am. du Sud), **4,1 m Europe** | CC-BY-4.0 |
| [3D-GloBFP](https://essd.copernicus.org/articles/16/5357/2024/) (ESSD 2024) | 1,66 Md | oui | RMSE 1,9 à 14,6 m | ouvert |

GlobalBuildingAtlas fournit en plus un raster de hauteurs à 3 m et des modèles
LoD1. Source : imagerie PlanetScope 3 m (2018-2019) + LiDAR gouvernemental pour
l'entraînement sur 168 villes.

**Pourquoi ça nous intéresse.** Aujourd'hui, quand OSM n'a ni `height` ni
`building:levels` — 31 % des bâtiments parisiens — on met la médiane du
quartier (21 m à Paris centre). C'est un pis-aller grossier. Une estimation par
bâtiment ferait nettement mieux.

**Nuance importante.** À Paris, les `building:levels` relevés à pied par les
contributeurs OSM valent probablement mieux que du ML. Le gain est surtout
**hors des zones bien cartographiées** — c'est-à-dire le jour où on sort de
France. À faire quand la couverture géographique deviendra le sujet, pas avant.

## Le socle Mapterhorn : DSM ou MNT ?

[Mapterhorn](https://protomaps.com/blog/mapterhorn-terrain/) utilise
**Copernicus GLO-30 comme socle mondial**, raffiné par des modèles LiDAR
nationaux là où ils existent (swissALTI3D à 0,5 m pour la Suisse, etc.).

Or **Copernicus GLO-30 est un DSM, pas un MNT**. Là où aucun LiDAR national ne
le raffine, notre « relief nu » contient donc déjà les bâtiments — et on stampe
les emprises OSM par-dessus. **Double comptage.**

C'est le genre de défaut qui produirait les bloqueurs classés `terrain` à très
courte distance qu'on a observés (14 cas sur 416 dans une zone test).

→ Vérifié par `cargo run --bin dem_probe`, cf. section suivante.

## Verdict du test `dem_probe`

`cargo run --release --bin dem_probe`, exécuté le 26 juillet 2026 :

| Ville | Bâtiment | Hauteur | Zoom max | Sur | Sol | Écart | LiDAR |
|---|---|---|---|---|---|---|---|
| Dubaï | Burj Khalifa | 828 m | **12** | 12,6 m | 4,0 m | +8,6 m | non |
| Le Caire | Tour du Caire | 187 m | **12** | 26,3 m | 13,5 m | +12,8 m | non |
| Kuala Lumpur | Tours Petronas | 452 m | **12** | 39,1 m | 46,4 m | −7,2 m | non |
| São Paulo | Edifício Itália | 168 m | **12** | 776,3 m | 741,7 m | +34,7 m | non |
| Paris | Tour Montparnasse | 210 m | **16** | 52,7 m | 42,2 m | +10,5 m | oui |
| New York | Empire State | 381 m | **16** | 15,2 m | 15,6 m | −0,4 m | oui |

### 1. L'hypothèse du double comptage est réfutée

Le Burj Khalifa fait **828 m** et ne pèse que **+8,6 m** dans le socle. Si les
bâtiments y étaient, on verrait des centaines de mètres. Idem pour les Petronas
(452 m réels, **−7,2 m** mesurés). Les écarts observés sont du relief, pas du
bâti — São Paulo à 776 m d'altitude sur une ville vallonnée en est l'exemple
caricatural, et son verdict « ambigu » est un faux positif du seuil.

**Le socle Mapterhorn se comporte comme un MNT partout où on l'a testé.**
Stamper les emprises OSM par-dessus est donc légitime, en France comme
ailleurs. Copernicus GLO-30 est documenté comme un DSM, mais des corrections y
sont appliquées pour atténuer bâtiments et végétation — et à 30 m de
résolution, un bâtiment isolé disparaît de toute façon dans la moyenne.

Corollaire : **les bloqueurs classés `terrain` à très courte distance ne
viennent pas de là.** L'explication la plus probable est un artefact
d'arrondi dans `describe_blocker` : `shadow_hit_from_ground` échantillonne la
DSM en bilinéaire à une position fractionnaire, donc en bord de bâtiment le
mélange des quatre cellules peut dépasser le rayon alors que la cellule la plus
proche — celle qu'on arrondit pour lire la grille de propriétaires — est du
terrain. Le blocage est réel, c'est son attribution qui est fausse.

### 2. Le vrai problème est la couverture, pas la nature du socle

**Hors zones raffinées par un LiDAR national, Mapterhorn s'arrête à z12.**
Soit ~17-19 m/px, cohérent avec Copernicus GLO-30. Notre serveur demande z15 en
dur : il reçoit donc des **404**, qu'il traduit en **502**. Autrement dit, le
moteur ne fonctionne aujourd'hui que dans les pays à LiDAR ouvert (France,
Suisse, États-Unis…). Hors de ces zones, il ne dégrade pas : il tombe.

C'est le blocage réel pour une couverture mondiale, et il était invisible tant
qu'on testait à Paris. Deux pistes :

- replier sur le zoom maximal disponible (`dem::max_zoom_at`) plutôt que
  d'échouer — le relief devient grossier mais les ombres de bâtiments, qui
  viennent des emprises OSM, restent justes ;
- en ville dense, le relief compte peu : un socle à 17 m/px y est acceptable,
  l'essentiel de l'ombre vient du bâti.

## À retenir

1. Garder l'architecture actuelle : DEM + emprises vectorielles rasterisées à
   la volée. C'est l'état de l'art pour une couverture mondiale, et le test
   confirme que le socle est bien du sol nu.
2. **Priorité couverture** : gérer le repli de zoom, sinon le serveur est
   inutilisable hors pays à LiDAR ouvert.
3. Le levier de qualité n'est **pas** le DSM mais **le repli de hauteur** —
   GlobalBuildingAtlas plutôt que la médiane de quartier, quand on sortira de
   France.
4. IGN LiDAR HD reste la meilleure donnée là où elle existe (France, ~80 % fin
   2025) — couche premium régionale, pas socle.
5. Corriger l'attribution du bloqueur en bord de bâtiment (artefact d'arrondi
   ci-dessus).
