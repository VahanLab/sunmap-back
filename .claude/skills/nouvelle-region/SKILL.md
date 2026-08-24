---
name: nouvelle-region
description: Checklist complète quand une nouvelle région/pays devient couvert par SunMap — après l'import de données (skill import-zone), mettre à jour le site web (page couverture + landing) et les fiches App Store et Google Play dans les 5 langues. À utiliser quand l'utilisateur annonce qu'un pays/une région est ajouté, couvert, ou demande de « mettre à jour la couverture ».
---

# Nouvelle région couverte — tout ce qui doit suivre

L'import des données est le début, pas la fin : trois surfaces publiques
énumèrent les pays couverts et **divergent silencieusement** si on n'y touche
pas. Ce skill est la liste exhaustive de ces surfaces. La règle : une seule
session met à jour TOUT, jamais « le site maintenant, les stores plus tard ».

## 0. Données (préalable — skill `import-zone`)

L'import serveur (PBF → pmtiles → R2 → base de lieux) est couvert par le
skill `import-zone`. Ce skill-ci commence quand les données sont en ligne.

## 1. Site web — `web/`

Deux endroits énumèrent les pays, en français ET en anglais :

- `web/src/lib/i18n.ts` → `coverageLoadedBody` (pays couverts) et
  `coverageSoonBody` (pays annoncés) — les deux blocs, `fr` et `en`.
  Un pays qui passe de « prochainement » à « couvert » doit **sortir** de
  `coverageSoonBody` en entrant dans `coverageLoadedBody`.
- `web/src/content/landing.ts` → deux occurrences de la liste
  (fr ~ligne 98, en ~ligne 191), format « France, Belgique, … — d'autres
  pays arrivent prochainement. »

Vérifier avec : `grep -rn "Luxembourg" web/src` — toute occurrence trouvée
doit être mise à jour ou justifiée.

Déploiement : suivre le README de `web/` (Next.js).

## 2. Fiche App Store — `tools/asc/`

La liste des pays vit dans la description, paragraphe « Pays couverts par le
calcul des ombres : … », dans **5 locales** :
`tools/asc/metadata/{fr-FR,en-US,de-DE,es-ES,it}/description.txt`
(l'italien est `it`, pas `it-IT`, côté App Store).

Push (nécessite une version en état modifiable — PREPARE_FOR_SUBMISSION) :

```bash
cd tools/asc && node src/cli.js push:text
```

Si « aucune version modifiable » : lister les versions et passer l'id
explicitement avec `ASC_VERSION_ID=<id>` (une version en préparation existe
souvent déjà ; sinon la créer dans App Store Connect). Piège connu : le
filtre `filter[platform]` de l'API rate parfois une version en préparation —
lister **sans** filtre pour vérifier avant de conclure qu'il n'y en a pas.

Piège connu du push : à la création d'une locale neuve, ASC crée lui-même la
localisation de version quand la fiche est créée, et le POST du tool tombe en
409 DUPLICATE. Relancer `push:text` jusqu'à ce que tout affiche
« inchangée » — chaque run avance d'une locale, rien n'est perdu.

Identifiants : `tools/asc/.env` (clé API dans
`~/.appstoreconnect/private_keys/`).

## 3. Fiche Google Play — `android/fastlane/`

Même paragraphe « Pays couverts… », dans **5 locales** :
`android/fastlane/metadata/android/{fr-FR,en-US,de-DE,es-ES,it-IT}/full_description.txt`
(ici l'italien est bien `it-IT`).

Valider puis pousser (metadata seulement — ne touche ni builds ni captures) :

```bash
cd android
fastlane supply --skip_upload_apk --skip_upload_aab --skip_upload_screenshots \
  --skip_upload_images --skip_upload_changelogs --track internal --validate_only
# même commande sans --validate_only une fois la validation passée
```

Identifiants : `android/fastlane/Appfile` → clé de service dans
`~/.googleplay/` (compte lié dans Play Console → Utilisateurs et
autorisations ; l'écriture exige « Gérer la présence sur le Play Store » ET
« Déployer les applications sur des canaux de test » — la seconde car
`supply` lit le canal même sans rien y envoyer).

### Captures d'écran (si les scènes ont changé)

Les deux stores se génèrent par la même mécanique de scènes
(`ScreenshotScene`, registres jumeaux iOS/Android — toute scène modifiée
d'un côté doit l'être de l'autre) :

- iOS : `cd ios/SunMap && fastlane snapshot`, puis
  `tools/asc/sync_screenshots.sh` et `node src/cli.js push:shots`
  (`--replace` remplace les jeux en ligne — demander avant) ;
- Android : émulateur démarré, `cd android && fastlane screengrab` — la
  sortie tombe directement dans `fastlane/metadata/android/<locale>/images/`,
  le `supply` suivant l'embarque. Piège : « not enough space » sur
  l'émulateur = désinstaller l'app d'abord (`adb uninstall`).

**Toujours relire une capture par langue avant d'envoyer** : c'est ce
contrôle qui a attrapé trois chaînes codées en dur (SunState iOS,
ShadowSource Android) que rien d'autre ne signalait.

## 4. Cohérence finale

Les trois listes doivent être **identiques au mot près** (dans chaque
langue). Ordre canonique actuel : France, Belgique, Luxembourg, Suisse,
Italie, Espagne, Royaume-Uni. Ajouter en fin de liste, sauf logique
géographique évidente.

Un `grep` de contrôle sur le nom du nouveau pays dans les trois zones :

```bash
grep -rn "NouveauPays" web/src tools/asc/metadata android/fastlane/metadata/android
```

— chaque locale de chaque surface doit ressortir. S'il manque une ligne,
c'est un oubli.
