// Captures d'écran : réservation, envoi par morceaux, validation, ordre.
// L'API impose trois temps — POST /v1/appScreenshots (réservation, qui renvoie
// des `uploadOperations`), PUT sur les URL fournies, puis PATCH avec
// `uploaded: true` et la somme MD5 du fichier.
import { readFileSync, readdirSync, existsSync } from 'node:fs';
import { join, basename } from 'node:path';
import { createHash } from 'node:crypto';
import { DISPLAY_TYPES, EXPECTED_SIZES } from './config.js';
import { versionLocalizations } from './app.js';

const IMAGE_RE = /\.(png|jpe?g)$/i;
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

/** Dimensions d'un PNG/JPEG, pour l'affichage — renvoie null si illisible. */
export function dimensions(buf) {
  if (buf.length > 24 && buf.readUInt32BE(0) === 0x89504e47) {
    return { w: buf.readUInt32BE(16), h: buf.readUInt32BE(20) };
  }
  if (buf.length > 4 && buf[0] === 0xff && buf[1] === 0xd8) {
    let i = 2;
    while (i < buf.length - 9) {
      if (buf[i] !== 0xff) { i++; continue; }
      const marker = buf[i + 1];
      const len = buf.readUInt16BE(i + 2);
      if (marker >= 0xc0 && marker <= 0xcf && ![0xc4, 0xc8, 0xcc].includes(marker)) {
        return { h: buf.readUInt16BE(i + 5), w: buf.readUInt16BE(i + 7) };
      }
      i += 2 + len;
    }
  }
  return null;
}

/** Inventaire local : locale -> displayType -> [chemins triés par nom]. */
export function scan(screenshotsDir) {
  const out = new Map();
  if (!existsSync(screenshotsDir)) return out;
  for (const locale of readdirSync(screenshotsDir, { withFileTypes: true })) {
    if (!locale.isDirectory() || locale.name.startsWith('.')) continue;
    const byType = new Map();
    const localeDir = join(screenshotsDir, locale.name);
    for (const type of readdirSync(localeDir, { withFileTypes: true })) {
      if (!type.isDirectory() || type.name.startsWith('.')) continue;
      if (!DISPLAY_TYPES.includes(type.name)) {
        console.error(`  ! dossier ignoré : ${locale.name}/${type.name} n'est pas un type d'écran connu`);
        continue;
      }
      const files = readdirSync(join(localeDir, type.name))
        .filter((f) => IMAGE_RE.test(f) && !f.startsWith('.'))
        .sort((a, b) => a.localeCompare(b, 'en', { numeric: true }))
        .map((f) => join(localeDir, type.name, f));
      if (files.length) byType.set(type.name, files);
    }
    if (byType.size) out.set(locale.name, byType);
  }
  return out;
}

/**
 * Décrit un fichier local : dimensions, poids, et si la taille fait partie de
 * celles qu'Apple accepte pour ce type d'écran.
 */
export function describe(path, displayType) {
  const buf = readFileSync(path);
  const dims = dimensions(buf);
  const accepted = EXPECTED_SIZES[displayType];
  let ok = null;
  if (dims && accepted) {
    ok = accepted.some(([w, h]) => (dims.w === w && dims.h === h) || (dims.w === h && dims.h === w));
  }
  return { name: basename(path), bytes: buf.length, ...(dims ?? {}), sizeOk: ok };
}

async function ensureLocalization(client, versionId, locale, cache) {
  const existing = cache.get(locale);
  if (existing) return existing;
  const res = await client.post('/v1/appStoreVersionLocalizations', {
    data: {
      type: 'appStoreVersionLocalizations',
      attributes: { locale },
      relationships: { appStoreVersion: { data: { type: 'appStoreVersions', id: versionId } } },
    },
  });
  cache.set(locale, res.data);
  return res.data;
}

async function ensureSet(client, localizationId, displayType) {
  const sets = await client.getAll(`/v1/appStoreVersionLocalizations/${localizationId}/appScreenshotSets`);
  const found = sets.find((s) => s.attributes.screenshotDisplayType === displayType);
  if (found) return found;
  const res = await client.post('/v1/appScreenshotSets', {
    data: {
      type: 'appScreenshotSets',
      attributes: { screenshotDisplayType: displayType },
      relationships: {
        appStoreVersionLocalization: { data: { type: 'appStoreVersionLocalizations', id: localizationId } },
      },
    },
  });
  return res.data;
}

async function uploadOne(client, setId, path) {
  const buf = readFileSync(path);
  const fileName = basename(path);
  const dims = dimensions(buf);

  const reservation = await client.post('/v1/appScreenshots', {
    data: {
      type: 'appScreenshots',
      attributes: { fileName, fileSize: buf.length },
      relationships: { appScreenshotSet: { data: { type: 'appScreenshotSets', id: setId } } },
    },
  });
  const id = reservation.data.id;
  const ops = reservation.data.attributes.uploadOperations ?? [];

  for (const op of ops) {
    const headers = Object.fromEntries((op.requestHeaders ?? []).map((h) => [h.name, h.value]));
    const chunk = buf.subarray(op.offset, op.offset + op.length);
    const res = await fetch(op.url, { method: op.method ?? 'PUT', headers, body: chunk });
    if (!res.ok) {
      throw new Error(`envoi de ${fileName} refusé : ${res.status} ${await res.text().catch(() => '')}`);
    }
  }

  await client.patch(`/v1/appScreenshots/${id}`, {
    data: {
      type: 'appScreenshots',
      id,
      attributes: { uploaded: true, sourceFileChecksum: createHash('md5').update(buf).digest('hex') },
    },
  });

  // Apple valide la capture de façon asynchrone (taille, format, alpha…).
  let state = 'UPLOAD_COMPLETE';
  for (let i = 0; i < 20; i++) {
    const res = await client.get(`/v1/appScreenshots/${id}`);
    const delivery = res.data.attributes.assetDeliveryState ?? {};
    state = delivery.state;
    if (state === 'COMPLETE') break;
    if (state === 'FAILED') {
      const why = (delivery.errors ?? []).map((e) => e.description ?? e.code).join(' ; ');
      throw new Error(`${fileName} refusée par App Store Connect : ${why || 'raison non fournie'}`);
    }
    await sleep(1500);
  }

  const size = dims ? ` ${dims.w}×${dims.h}` : '';
  console.log(`      + ${fileName}${size} (${(buf.length / 1024).toFixed(0)} Ko) ${state}`);
  return id;
}

/**
 * Pousse les captures. Par défaut on complète (les noms déjà présents sont sautés) ;
 * `replace` vide le jeu avant d'envoyer, ce qui est le mode à utiliser pour un
 * remplacement propre — l'ordre final suit l'ordre alphabétique des fichiers.
 */
export async function push(client, cfg, ctx, { locales, displayTypes, replace = false, dryRun = false } = {}) {
  const inventory = scan(cfg.screenshotsDir);
  if (inventory.size === 0) throw new Error(`aucune capture dans ${cfg.screenshotsDir}`);

  const locCache = await versionLocalizations(client, ctx.version.id);

  for (const [locale, byType] of inventory) {
    if (locales?.length && !locales.includes(locale)) continue;
    console.log(`  ${locale}`);
    // En dry-run on ne crée rien, pas même la localisation manquante.
    const localization = dryRun ? locCache.get(locale) : await ensureLocalization(client, ctx.version.id, locale, locCache);

    for (const [displayType, files] of byType) {
      if (displayTypes?.length && !displayTypes.includes(displayType)) continue;
      console.log(`    ${displayType} — ${files.length} capture(s)`);
      if (dryRun) {
        for (const f of files) {
          const d = describe(f, displayType);
          const flag = d.sizeOk === false ? ' ← taille inattendue' : '';
          console.log(`      · ${d.name} ${d.w ?? '?'}×${d.h ?? '?'} (${(d.bytes / 1024).toFixed(0)} Ko)${flag}`);
        }
        if (!localization) console.log(`      (la langue ${locale} n'existe pas encore sur la version, elle sera créée)`);
        continue;
      }

      const set = await ensureSet(client, localization.id, displayType);
      const existing = await client.getAll(`/v1/appScreenshotSets/${set.id}/appScreenshots`);

      if (replace) {
        for (const shot of existing) await client.delete(`/v1/appScreenshots/${shot.id}`);
        if (existing.length) console.log(`      − ${existing.length} capture(s) supprimée(s)`);
      }

      const kept = replace ? [] : existing;
      const keptNames = new Set(kept.map((s) => s.attributes.fileName));
      const order = [];
      for (const file of files) {
        const name = basename(file);
        if (keptNames.has(name)) {
          console.log(`      = ${name} (déjà en ligne)`);
          order.push(kept.find((s) => s.attributes.fileName === name).id);
          continue;
        }
        order.push(await uploadOne(client, set.id, file));
      }

      // L'ordre d'affichage sur la fiche est celui de la relation, pas celui d'envoi.
      await client.patch(`/v1/appScreenshotSets/${set.id}/relationships/appScreenshots`, {
        data: order.map((id) => ({ type: 'appScreenshots', id })),
      });
    }
  }
}

/** Ce qui est en ligne, sans rien modifier. */
export async function list(client, ctx) {
  const locs = await versionLocalizations(client, ctx.version.id);
  for (const [locale, loc] of locs) {
    const sets = await client.getAll(`/v1/appStoreVersionLocalizations/${loc.id}/appScreenshotSets`);
    if (sets.length === 0) {
      console.log(`  ${locale} : aucune capture`);
      continue;
    }
    console.log(`  ${locale}`);
    for (const set of sets) {
      const shots = await client.getAll(`/v1/appScreenshotSets/${set.id}/appScreenshots`);
      console.log(`    ${set.attributes.screenshotDisplayType} — ${shots.length}`);
      for (const s of shots) {
        const a = s.attributes;
        console.log(`      ${a.fileName} ${a.imageAsset?.width ?? '?'}×${a.imageAsset?.height ?? '?'} ${a.assetDeliveryState?.state ?? ''}`);
      }
    }
  }
}
