# asc — fiches App Store en ligne de commande

Petit outil Node (zéro dépendance) qui pousse **les textes de la fiche et les
captures d'écran, dans toutes les langues**, via l'API App Store Connect.
Les textes vivent en fichiers versionnés, l'outil ne fait que synchroniser.

Langues de SunMap : `en-US`, `fr-FR`, `de-DE`, `es-ES`, `it`
(miroir de `ios/SunMap/SunMap/Localizable.xcstrings`).

## Mise en route

1. App Store Connect → **Utilisateurs et accès → Intégrations → Clés API** →
   créer une clé de rôle **App Manager**. Le `.p8` ne se télécharge qu'une fois.
2. Déposer le `.p8` dans `tools/asc/` (il est dans le `.gitignore`).
3. `cp .env.example .env` et remplir `ASC_KEY_ID`, `ASC_ISSUER_ID`, `ASC_KEY_PATH`.
4. Vérifier la connexion :

```bash
node tools/asc/src/cli.js status
```

Aucun `npm install` : Node 18+ suffit (`fetch` et `node:crypto` natifs).

## Arborescence

Compatible avec la convention `fastlane deliver`, au cas où l'outil serait
remplacé un jour :

```
tools/asc/
  metadata/
    fr-FR/
      name.txt              nom de l'app          ≤ 30
      subtitle.txt          sous-titre            ≤ 30
      privacy_url.txt       URL de confidentialité
      description.txt       description           ≤ 4000
      keywords.txt          mots-clés (séparés par des virgules, sans espace) ≤ 100
      promotional_text.txt  texte promotionnel    ≤ 170
      release_notes.txt     nouveautés            ≤ 4000
      support_url.txt       URL d'assistance
      marketing_url.txt     URL marketing
    en-US/ de-DE/ es-ES/ it/
  screenshots/
    fr-FR/
      APP_IPHONE_67/
        01-carte.png
        02-timeline.png
      APP_IPAD_PRO_3GEN_129/
        ...
```

`name`, `subtitle` et `privacy_url` sont portés par la **fiche** (`appInfo`),
les autres par la **version en préparation** (`appStoreVersion`). L'outil s'en
occupe, la distinction n'apparaît que dans les messages.

L'ordre d'affichage des captures est l'ordre **alphabétique des noms de
fichiers** — d'où le préfixe numérique.

### Types d'écran

Le nom du sous-dossier est la valeur `screenshotDisplayType` de l'API.
Les deux formats attendus aujourd'hui par Apple :

| Dossier | Appareil | Pixels acceptés |
| --- | --- | --- |
| `APP_IPHONE_67` | iPhone 6,9" et 6,7" | 1320×2868 ou 1290×2796 (et paysage) |
| `APP_IPAD_PRO_3GEN_129` | iPad 13" et 12,9" | 2064×2752 ou 2048×2732 |

Il n'existe **pas** de `APP_IPHONE_69` : les captures 6,9" se déposent dans
`APP_IPHONE_67`. Les autres valeurs possibles sont listées dans
[`src/config.js`](src/config.js) (`DISPLAY_TYPES`).

## Commandes

```bash
node tools/asc/src/cli.js <commande> [options]
```

| Commande | Effet |
| --- | --- |
| `status` | app, version en préparation, langues en ligne et sur disque |
| `init` | crée l'arborescence vide pour les 5 langues |
| `check` | valide les longueurs et compte les captures, **hors ligne** |
| `pull` | télécharge les textes existants vers `metadata/` |
| `push` | envoie textes **et** captures |
| `push:text` | textes seuls |
| `push:shots` | captures seules |
| `shots` | liste les captures déjà en ligne |

Options : `--locale fr-FR,de-DE`, `--type APP_IPHONE_67`, `--dry-run`,
`--replace`, `--allow-empty`, `--verbose`.

### Le parcours habituel

```bash
node tools/asc/src/cli.js pull                 # récupérer l'existant
# … éditer metadata/**/*.txt, déposer les PNG dans screenshots/**/
node tools/asc/src/cli.js check                # longueurs, comptage
node tools/asc/src/cli.js push --dry-run       # ce qui changerait
node tools/asc/src/cli.js push --replace       # envoi réel
```

## Ce qu'il faut savoir

- **Un fichier vide est ignoré**, il n'efface pas la valeur en ligne. Pour
  vider un champ pour de bon : `--allow-empty`.
- **Captures : `--replace` vide le jeu avant d'envoyer.** Sans lui, l'outil
  complète et saute les noms de fichiers déjà en ligne — pratique pour
  reprendre un envoi interrompu, trompeur si le contenu d'un fichier a changé
  sans que son nom bouge.
- **`release_notes.txt` est refusé sur une première version** (Apple n'accepte
  les nouveautés qu'à partir de la deuxième). Laisser le fichier vide.
- La version visée est celle en préparation. Pour en forcer une autre :
  `ASC_VERSION_ID=<id>`.
- Quota API : 3 600 requêtes par heure. L'outil réessaie tout seul sur 429 et
  5xx, avec attente doublée.
- Un envoi ne **soumet rien** : il faut toujours passer par App Store Connect
  pour envoyer en revue.
- Non couvert : aperçus vidéo (`appPreviews`), tarifs, disponibilité par pays,
  informations de revue.
