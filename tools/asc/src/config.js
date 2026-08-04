// Configuration : .env local (jamais commité) + constantes de la fiche App Store.
import { readFileSync, existsSync } from 'node:fs';
import { dirname, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

export const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '..');

function loadDotenv(path) {
  if (!existsSync(path)) return;
  for (const line of readFileSync(path, 'utf8').split('\n')) {
    const m = line.match(/^\s*([A-Z0-9_]+)\s*=\s*(.*)$/);
    if (!m) continue;
    let value = m[2].trim();
    if ((value.startsWith('"') && value.endsWith('"')) || (value.startsWith("'") && value.endsWith("'"))) {
      value = value.slice(1, -1);
    }
    if (process.env[m[1]] === undefined) process.env[m[1]] = value;
  }
}

loadDotenv(resolve(ROOT, '.env'));

/** Chemins et cible ; ne réclame pas la clé API (les commandes hors ligne s'en passent). */
export function config() {
  return {
    appId: process.env.ASC_APP_ID,
    bundleId: process.env.ASC_BUNDLE_ID,
    platform: process.env.ASC_PLATFORM || 'IOS',
    metadataDir: resolve(ROOT, process.env.ASC_METADATA_DIR || 'metadata'),
    screenshotsDir: resolve(ROOT, process.env.ASC_SCREENSHOTS_DIR || 'screenshots'),
  };
}

/** Clé API, réclamée seulement au moment d'appeler Apple. */
export function credentials() {
  const keyId = process.env.ASC_KEY_ID;
  const keyPath = process.env.ASC_KEY_PATH;
  const keyB64 = process.env.ASC_KEY_B64;

  if (!keyId) throw new Error('ASC_KEY_ID manquant (copier .env.example en .env)');
  if (!keyPath && !keyB64) throw new Error('ASC_KEY_PATH ou ASC_KEY_B64 manquant');

  return {
    keyId,
    issuerId: process.env.ASC_ISSUER_ID,
    privateKeyPem: keyB64
      ? Buffer.from(keyB64, 'base64').toString('utf8')
      : readFileSync(resolve(ROOT, keyPath), 'utf8'),
  };
}

// Champs portés par appStoreVersionLocalizations (la version en préparation).
// nom du fichier -> { attribut API, longueur max }
export const VERSION_FIELDS = {
  'description.txt': { attr: 'description', max: 4000 },
  'keywords.txt': { attr: 'keywords', max: 100 },
  'promotional_text.txt': { attr: 'promotionalText', max: 170 },
  'release_notes.txt': { attr: 'whatsNew', max: 4000 },
  'support_url.txt': { attr: 'supportUrl', max: 255 },
  'marketing_url.txt': { attr: 'marketingUrl', max: 255 },
};

// Champs portés par appInfoLocalizations (la fiche, hors version).
export const INFO_FIELDS = {
  'name.txt': { attr: 'name', max: 30 },
  'subtitle.txt': { attr: 'subtitle', max: 30 },
  'privacy_url.txt': { attr: 'privacyPolicyUrl', max: 255 },
};

// Types d'écran acceptés par l'API (valeurs `screenshotDisplayType`).
// Note : il n'existe PAS de APP_IPHONE_69 — les captures 6,9" (1320×2868)
// se déposent dans APP_IPHONE_67, comme les 6,7" (1290×2796).
export const DISPLAY_TYPES = [
  'APP_IPHONE_67',
  'APP_IPHONE_65',
  'APP_IPHONE_61',
  'APP_IPHONE_58',
  'APP_IPHONE_55',
  'APP_IPHONE_47',
  'APP_IPAD_PRO_3GEN_129',
  'APP_IPAD_PRO_3GEN_11',
  'APP_IPAD_PRO_129',
  'APP_IPAD_105',
  'APP_IPAD_97',
  'APP_DESKTOP',
  'APP_APPLE_VISION_PRO',
  'APP_WATCH_ULTRA',
  'APP_WATCH_SERIES_10',
  'APP_WATCH_SERIES_7',
  'APP_WATCH_SERIES_4',
  'APP_WATCH_SERIES_3',
  'APP_APPLE_TV',
];

// Tailles acceptées par Apple, en portrait (le paysage est la transposée).
// Sert à prévenir avant l'envoi ; l'API reste seule juge.
export const EXPECTED_SIZES = {
  APP_IPHONE_67: [[1320, 2868], [1290, 2796]],
  APP_IPHONE_65: [[1242, 2688], [1284, 2778]],
  APP_IPHONE_61: [[1179, 2556], [1206, 2622]],
  APP_IPHONE_58: [[1125, 2436], [1170, 2532]],
  APP_IPHONE_55: [[1242, 2208]],
  APP_IPAD_PRO_3GEN_129: [[2064, 2752], [2048, 2732]],
  APP_IPAD_PRO_3GEN_11: [[1668, 2388]],
  APP_IPAD_PRO_129: [[2048, 2732]],
};

// Langues de l'app (cf. ios/SunMap/SunMap/Localizable.xcstrings) → codes App Store Connect.
export const LOCALES = ['en-US', 'fr-FR', 'de-DE', 'es-ES', 'it'];

// États dans lesquels une version App Store accepte encore des modifications.
export const EDITABLE_VERSION_STATES = new Set([
  'PREPARE_FOR_SUBMISSION',
  'DEVELOPER_REJECTED',
  'REJECTED',
  'METADATA_REJECTED',
  'INVALID_BINARY',
  'WAITING_FOR_REVIEW',
  'DEVELOPER_REMOVED_FROM_SALE',
  'READY_FOR_REVIEW',
]);
