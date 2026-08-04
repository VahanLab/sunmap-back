// Textes de la fiche : lecture/écriture des fichiers locaux et synchro avec l'API.
import { readFileSync, writeFileSync, mkdirSync, existsSync, readdirSync } from 'node:fs';
import { join } from 'node:path';
import { VERSION_FIELDS, INFO_FIELDS, LOCALES } from './config.js';
import { versionLocalizations, infoLocalizations } from './app.js';

/** Locales présentes sur disque, à défaut la liste par défaut. */
export function localesOnDisk(metadataDir) {
  if (!existsSync(metadataDir)) return [];
  return readdirSync(metadataDir, { withFileTypes: true })
    .filter((e) => e.isDirectory() && !e.name.startsWith('.'))
    .map((e) => e.name)
    .sort();
}

function readField(dir, file) {
  const path = join(dir, file);
  if (!existsSync(path)) return undefined;
  const raw = readFileSync(path, 'utf8');
  // On retire le saut de ligne final que tout éditeur ajoute, mais on garde les
  // retours internes : ils comptent dans les 4000 caractères de la description.
  return raw.replace(/\n+$/, '');
}

function writeField(dir, file, value) {
  mkdirSync(dir, { recursive: true });
  writeFileSync(join(dir, file), value == null ? '' : `${value}\n`, 'utf8');
}

/**
 * Lit les fichiers d'une locale et renvoie {info: {...}, version: {...}, problems: []}.
 * Un fichier vide est ignoré (il n'efface pas la valeur en ligne) sauf `allowEmpty`.
 */
export function readLocale(metadataDir, locale, { allowEmpty = false } = {}) {
  const dir = join(metadataDir, locale);
  const out = { info: {}, version: {}, problems: [] };

  const collect = (specs, target) => {
    for (const [file, spec] of Object.entries(specs)) {
      const v = readField(dir, file);
      if (v === undefined) continue;
      if (v.trim() === '' && !allowEmpty) continue;
      if (v.length > spec.max) out.problems.push(`${locale}/${file} : ${v.length} caractères (max ${spec.max})`);
      target[spec.attr] = v;
    }
  };
  collect(INFO_FIELDS, out.info);
  collect(VERSION_FIELDS, out.version);
  return out;
}

/** Écrit sur disque ce que l'App Store connaît déjà. */
export async function pull(client, cfg, ctx, { locales } = {}) {
  const vLoc = await versionLocalizations(client, ctx.version.id);
  const iLoc = await infoLocalizations(client, ctx.appInfo.id);
  const wanted = locales?.length ? locales : [...new Set([...vLoc.keys(), ...iLoc.keys()])].sort();

  for (const locale of wanted) {
    const dir = join(cfg.metadataDir, locale);
    const v = vLoc.get(locale)?.attributes ?? {};
    const i = iLoc.get(locale)?.attributes ?? {};
    for (const [file, spec] of Object.entries(INFO_FIELDS)) writeField(dir, file, i[spec.attr]);
    for (const [file, spec] of Object.entries(VERSION_FIELDS)) writeField(dir, file, v[spec.attr]);
    console.log(`  ${locale} → ${dir}`);
  }
  return wanted;
}

const changed = (current, next) =>
  Object.fromEntries(Object.entries(next).filter(([k, v]) => (current?.[k] ?? '') !== v));

/** Pousse les textes. `dryRun` n'écrit rien, il affiche seulement le diff. */
export async function push(client, cfg, ctx, { locales, dryRun = false, allowEmpty = false } = {}) {
  const vLoc = await versionLocalizations(client, ctx.version.id);
  const iLoc = await infoLocalizations(client, ctx.appInfo.id);
  const wanted = locales?.length ? locales : localesOnDisk(cfg.metadataDir);
  if (wanted.length === 0) throw new Error(`aucune locale dans ${cfg.metadataDir} (lancer "asc pull" ou "asc init")`);

  const problems = [];
  for (const locale of wanted) {
    const local = readLocale(cfg.metadataDir, locale, { allowEmpty });
    problems.push(...local.problems);
  }
  if (problems.length) {
    throw new Error(`textes trop longs, rien n'a été envoyé :\n  ${problems.join('\n  ')}`);
  }

  for (const locale of wanted) {
    const local = readLocale(cfg.metadataDir, locale, { allowEmpty });

    // Fiche (nom, sous-titre, URL de confidentialité)
    if (Object.keys(local.info).length) {
      const existing = iLoc.get(locale);
      const delta = changed(existing?.attributes, local.info);
      if (Object.keys(delta).length === 0) {
        console.log(`  ${locale} fiche : inchangée`);
      } else if (dryRun) {
        console.log(`  ${locale} fiche : ${Object.keys(delta).join(', ')} (dry-run)`);
      } else if (existing) {
        await client.patch(`/v1/appInfoLocalizations/${existing.id}`, {
          data: { type: 'appInfoLocalizations', id: existing.id, attributes: delta },
        });
        console.log(`  ${locale} fiche : ${Object.keys(delta).join(', ')} ✎`);
      } else {
        await client.post('/v1/appInfoLocalizations', {
          data: {
            type: 'appInfoLocalizations',
            attributes: { locale, ...local.info },
            relationships: { appInfo: { data: { type: 'appInfos', id: ctx.appInfo.id } } },
          },
        });
        console.log(`  ${locale} fiche : créée +`);
      }
    }

    // Version (description, mots-clés, nouveautés, URLs)
    if (Object.keys(local.version).length) {
      const existing = vLoc.get(locale);
      const delta = changed(existing?.attributes, local.version);
      if (Object.keys(delta).length === 0) {
        console.log(`  ${locale} version : inchangée`);
      } else if (dryRun) {
        console.log(`  ${locale} version : ${Object.keys(delta).join(', ')} (dry-run)`);
      } else if (existing) {
        await client.patch(`/v1/appStoreVersionLocalizations/${existing.id}`, {
          data: { type: 'appStoreVersionLocalizations', id: existing.id, attributes: delta },
        });
        console.log(`  ${locale} version : ${Object.keys(delta).join(', ')} ✎`);
      } else {
        await client.post('/v1/appStoreVersionLocalizations', {
          data: {
            type: 'appStoreVersionLocalizations',
            attributes: { locale, ...local.version },
            relationships: { appStoreVersion: { data: { type: 'appStoreVersions', id: ctx.version.id } } },
          },
        });
        console.log(`  ${locale} version : créée +`);
      }
    }
  }
}

/** Crée l'arborescence vide pour les langues de l'app. */
export function init(cfg, locales = LOCALES) {
  for (const locale of locales) {
    const dir = join(cfg.metadataDir, locale);
    mkdirSync(dir, { recursive: true });
    for (const file of [...Object.keys(INFO_FIELDS), ...Object.keys(VERSION_FIELDS)]) {
      if (!existsSync(join(dir, file))) writeField(dir, file, '');
    }
    mkdirSync(join(cfg.screenshotsDir, locale, 'APP_IPHONE_67'), { recursive: true });
    console.log(`  ${locale}`);
  }
}
