---
name: import-modele-3d
description: Intègre un modèle 3D (export Meshy .glb) dans l'app iOS SunMap — allègement en deux passes, génération de la variante lointaine (LOD proche/loin), relevé des mesures de bbox et câblage des ModelLayer Mapbox. À utiliser quand l'utilisateur veut ajouter, remplacer ou alléger un modèle 3D : arbre, bosquet, terrasse, banc, table de pique-nique, mobilier.
---

# Importer un modèle 3D dans SunMap

Tout modèle posé sur la carte est **instancié en masse** : des milliers de
bosquets dans une forêt, des dizaines de bancs dans une rue. Un maillage brut
d'export Meshy fait 50 000 à 1 500 000 triangles — multiplié par le nombre
d'instances, c'est un ou deux ordres de grandeur au-dessus de ce qu'un
téléphone rend en une image. **Aucun modèle ne part en production sans passer
par cette chaîne.**

## 1. Alléger — deux passes, pas une

Les exports bruts vivent dans `ios/SunMap/assets-src/` (ignoré par git, 20 à
75 Mo pièce). Cible : 100 à 300 Ko par modèle.

```bash
npx @gltf-transform/cli@3 optimize in.glb tmp.glb --compress false --texture-compress false --texture-size 1024 --simplify false
npx @gltf-transform/cli@3 resize tmp.glb near.glb --width 1024 --height 1024
npx @gltf-transform/cli@3 simplify near.glb out.glb --ratio 0.02 --error 1
```

- **CLI v3, pas v4** : la 4 exige Node ≥ 20.12.
- **`--texture-size` d'`optimize` ne redimensionne rien** en pratique — c'est
  `resize` qui fait le travail. Sans lui, un modèle sort à 16,9 Mo avec des
  textures encore en 4096.
- **`simplify` séparé plutôt que `--simplify-error`** : le taux se pilote,
  l'erreur non. `--ratio` est la fraction de sommets gardés, `--error 1` lève
  la contrainte d'erreur pour que le taux soit réellement atteint.
- Viser **~4 000 triangles** pour un modèle vu de près, **500 à 1 000** pour sa
  variante lointaine.

### Quand `simplify` refuse de descendre

Certains maillages plafonnent (la terrasse s'arrête à 31 874 triangles quel que
soit le taux). Cause : le maillage est fait de **coques disjointes** — parasols,
chaises, tables séparés — que meshoptimizer ne peut pas fusionner. Le repérer :
si `vertices` ≈ `triangles` dans `inspect`, la topologie est éclatée.

`weld` **ne le sauve pas** (essayé jusqu'à `--tolerance 0.001
--tolerance-normal 1.0`, aucun effet). Le seul remède est un ré-export du
modèle. Le constater, le dire, et prendre le gain partiel — ne pas s'acharner
sur les réglages.

## 2. Deux modèles : proche et loin

**Tout modèle instancié en masse a une variante lointaine.** Le principe :

| | Proche | Loin |
|---|---|---|
| Triangles | ~4 000 | 500–1 000 |
| Textures | 1024 px | 512 px |
| Ombre portée | oui | **selon ce qu'elle signifie** |

La raison principale est le nombre : à z15–17 un bosquet fait quelques pixels
de haut, et c'est **précisément là qu'on en pose le plus** — une grande forêt
remplit l'écran.

**Sur l'ombre, se méfier du réflexe.** `modelCastShadows` fait re-rendre toute
la géométrie dans la passe de shadow map : c'est un doublement pur du coût, et
la couper est tentant. Ça a été fait sur les bosquets lointains, et ça revenait
à **supprimer l'ombre de la végétation dans tout l'usage normal** — on ne voit
le maillage détaillé que de très près. Or l'ombre d'un arbre est une
information du produit, pas un ornement. La bonne question n'est pas « est-ce
loin ? » mais « est-ce que cette ombre dit quelque chose ? » : oui pour un
arbre, non pour un banc à z18, dont l'ombre fait un pixel.

Quand l'ombre reste, **la compter dans le budget** (×2 sur le coût de
l'instance) plutôt que la retirer : l'arbitrage vit alors dans le budget, où on
peut le revoir, au lieu d'être figé dans l'asset.

Générer la variante lointaine **depuis le `.glb` proche déjà livré** quand c'est
possible : la bbox reste presque identique, donc les constantes d'échelle se
déduisent l'une de l'autre.

### Deux façons de basculer, selon qui reconstruit la source

C'est le point qui décide de tout le câblage :

- **La source est reconstruite au zoom** (végétation — `VegetationPlacement`
  repose ses instances à chaque changement de cadrage) → choisir le modèle
  **à l'implantation**, dans `VegetationModel.tree(for:zoom:)` /
  `cluster(for:canopyTopM:zoom:)`. La couche se déduit de la propriété `model`
  de la feature, donc choisir le modèle suffit à choisir la couche — et le
  budget de triangles peut compter le maillage réellement dessiné.
- **La source n'est reconstruite qu'au rechargement des données** (terrasses,
  mobilier — au rechargement des lieux, pas à chaque cran de zoom) → **deux
  couches**, `minZoom`/`maxZoom` opposés, même source et même filtre. Cf.
  `TerraceModelLayer.farLayer(...)` et `FurnitureModelLayer.farLayer(...)`.

**Piège** : les gestes de tap sont posés par identifiant de couche. Ajouter une
couche lointaine sans y poser le même `onLayerTap` rend l'objet visible mais
sourd au doigt sous le seuil de détail — ce qui se lit comme une panne.

Seuils en place : `VegetationModel.detailedZoom` = 17 (arbres isolés),
`VegetationModel.detailedClusterZoom` = 18 (bosquets),
`FurnitureModelLayer.detailedZoom` = `TerraceModelLayer.detailedZoom` = 19,5
(le zoom d'arrivée du survol vers un lieu, `CameraFraming.baseZoom`).

## 3. Relever les mesures

```bash
npx @gltf-transform/cli@3 inspect out.glb
```

- `SCENES` → `bboxMin` / `bboxMax`. Meshy normalise dans un cube unité et
  **centre sur l'origine** : sans relèvement, la moitié basse passe sous terre.
  - `widthUnits` = étendue en X (`bboxMax.x - bboxMin.x`)
  - `heightUnits` = étendue en Y (Y est l'axe vertical en glTF)
  - `minYUnits` = `|bboxMin.y|` — c'est le relèvement
- `MESHES` → `glPrimitives` = le nombre de triangles, à reporter dans
  `VegetationModel.triangleCount` : c'est la monnaie du budget d'implantation.

**La simplification rogne l'emprise de 1 à 2 %.** Rattraper l'échelle pour que
l'objet garde sa taille réelle de part et d'autre du seuil :

```
metersPerUnit_loin = metersPerUnit_proche × (emprise_proche / emprise_loin)
```

## 4. Câbler

- **Végétation** → `Features/Forest/Domain/VegetationModel.swift` (une constante
  par modèle : `widthUnits`, `heightUnits`, `minYUnits`, `triangleCount`,
  `castsShadows`), puis l'ajouter à `VegetationModel.all` — c'est cette liste
  qui enregistre les maillages dans le style au démarrage.
- **Mobilier** → `FurnitureModelLayer.Model` + sa `Far`, et `all`.
- **Terrasse** → constantes de `TerraceModelLayer`.
- Fichiers dans `Features/<Feature>/Resources/`. Les groupes Xcode sont
  synchronisés au système de fichiers : déposer le `.glb` suffit, il n'y a pas
  de `project.pbxproj` à éditer.

## 5. Le budget, à ne pas contourner

`VegetationPlacement.clusterTriangleBudget` (3 M) plafonne les bosquets d'une
image, passe d'ombre comprise. Un plafond exprimé en **nombre d'instances** est
aveugle au coût unitaire — il autorise autant de bosquets à 50 000 triangles
qu'à 4 000, et c'est exactement ce qui a laissé passer 300 M de triangles par
image en forêt de conifères. Si un nouveau modèle sature le budget, c'est le
modèle qu'il faut alléger, pas le budget qu'il faut lever.

La dilution qui en découle se fait par **crans de puissance de deux**, jamais
par un facteur continu : les nœuds d'un cran doivent rester un sous-ensemble
strict de ceux du cran précédent, sinon changer de niveau déplace toute la
forêt. Cf. `VegetationPlacement.thinningLevel`.

## Vérifier

```bash
cd ios/SunMap && xcodebuild -workspace SunMap.xcworkspace -scheme SunMap \
  -sdk iphonesimulator -destination 'generic/platform=iOS Simulator' build
```

Builder seulement. **Ne pas installer ni lancer sur simulateur** : c'est le
développeur qui teste le rendu, sauf demande explicite — et un modèle 3D ne se
valide qu'à l'œil.
