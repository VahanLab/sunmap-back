#!/usr/bin/env node
// Ligne de commande : voir README.md
import { AscClient, AscError } from './api.js';
import { config, credentials, LOCALES, VERSION_FIELDS, INFO_FIELDS } from './config.js';
import { context } from './app.js';
import * as metadata from './metadata.js';
import * as screenshots from './screenshots.js';

const USAGE = `
asc — fiches App Store (textes + captures), toutes langues

  node src/cli.js <commande> [options]

Commandes
  status                 app, version en préparation, langues déjà en ligne
  init                   crée l'arborescence metadata/ et screenshots/ vides
  check                  vérifie les longueurs des textes locaux (hors ligne)
  pull                   télécharge les textes en ligne vers metadata/
  push                   envoie textes + captures
  push:text              envoie seulement les textes
  push:shots             envoie seulement les captures
  shots                  liste les captures déjà en ligne

Options
  --locale <a,b>         restreint aux locales données (défaut : tout ce qui est sur disque)
  --type <A,B>           restreint aux types d'écran (ex. APP_IPHONE_67)
  --dry-run              n'écrit rien côté App Store, affiche ce qui changerait
  --replace              captures : vide le jeu avant d'envoyer (défaut : complète)
  --allow-empty          textes : un fichier vide efface la valeur en ligne
  --verbose              trace les requêtes HTTP
`;

function parseArgs(argv) {
  const out = { _: [], flags: {} };
  for (let i = 0; i < argv.length; i++) {
    const a = argv[i];
    if (!a.startsWith('--')) {
      out._.push(a);
      continue;
    }
    const [name, inline] = a.slice(2).split('=');
    if (['locale', 'type'].includes(name)) {
      out.flags[name] = (inline ?? argv[++i] ?? '').split(',').filter(Boolean);
    } else {
      out.flags[name] = true;
    }
  }
  return out;
}

async function main() {
  const { _: positional, flags } = parseArgs(process.argv.slice(2));
  const command = positional[0] ?? 'status';
  if (['help', '-h', '--help'].includes(command)) {
    console.log(USAGE);
    return;
  }

  const cfg = config();

  if (command === 'init') {
    console.log(`Arborescence dans ${cfg.metadataDir} et ${cfg.screenshotsDir}`);
    metadata.init(cfg, flags.locale?.length ? flags.locale : LOCALES);
    return;
  }

  if (command === 'check') {
    const locales = flags.locale?.length ? flags.locale : metadata.localesOnDisk(cfg.metadataDir);
    let problems = 0;
    for (const locale of locales) {
      const local = metadata.readLocale(cfg.metadataDir, locale);
      const filled = [...Object.keys(local.info), ...Object.keys(local.version)];
      console.log(`  ${locale} : ${filled.length} champ(s) rempli(s) — ${filled.join(', ') || 'aucun'}`);
      for (const p of local.problems) {
        console.error(`    ! ${p}`);
        problems++;
      }
    }
    const inventory = screenshots.scan(cfg.screenshotsDir);
    for (const [locale, byType] of inventory) {
      if (locales.length && !locales.includes(locale)) continue;
      for (const [type, files] of byType) {
        console.log(`  ${locale} ${type} : ${files.length} capture(s)`);
        for (const file of files) {
          const d = screenshots.describe(file, type);
          const size = d.w ? `${d.w}×${d.h}` : 'dimensions illisibles';
          console.log(`    ${d.name} ${size} ${(d.bytes / 1024).toFixed(0)} Ko`);
          if (d.sizeOk === false) {
            console.error(`    ! ${d.name} : ${size} n'est pas une taille acceptée pour ${type}`);
            problems++;
          }
        }
      }
    }
    if (problems) process.exitCode = 1;
    return;
  }

  const client = new AscClient({ ...credentials(), verbose: Boolean(flags.verbose) });
  const ctx = await context(client, cfg);
  const versionState = ctx.version.attributes.appVersionState ?? ctx.version.attributes.appStoreState;
  console.log(
    `${ctx.app.attributes.name} (${ctx.app.attributes.bundleId}) — version ${ctx.version.attributes.versionString} [${versionState}]`,
  );

  const opts = {
    locales: flags.locale,
    displayTypes: flags.type,
    dryRun: Boolean(flags['dry-run']),
    replace: Boolean(flags.replace),
    allowEmpty: Boolean(flags['allow-empty']),
  };

  switch (command) {
    case 'status': {
      const { versionLocalizations, infoLocalizations } = await import('./app.js');
      const v = await versionLocalizations(client, ctx.version.id);
      const i = await infoLocalizations(client, ctx.appInfo.id);
      console.log(`  langues de la version : ${[...v.keys()].sort().join(', ') || 'aucune'}`);
      console.log(`  langues de la fiche   : ${[...i.keys()].sort().join(', ') || 'aucune'}`);
      console.log(`  locales sur disque    : ${metadata.localesOnDisk(cfg.metadataDir).join(', ') || 'aucune'}`);
      console.log(
        `  champs gérés : ${[...Object.keys(INFO_FIELDS), ...Object.keys(VERSION_FIELDS)].join(', ')}`,
      );
      break;
    }
    case 'pull':
      console.log('Téléchargement des textes :');
      await metadata.pull(client, cfg, ctx, { locales: flags.locale });
      break;
    case 'push:text':
      console.log('Textes :');
      await metadata.push(client, cfg, ctx, opts);
      break;
    case 'push:shots':
      console.log('Captures :');
      await screenshots.push(client, cfg, ctx, opts);
      break;
    case 'push':
      console.log('Textes :');
      await metadata.push(client, cfg, ctx, opts);
      console.log('Captures :');
      await screenshots.push(client, cfg, ctx, opts);
      break;
    case 'shots':
      await screenshots.list(client, ctx);
      break;
    default:
      console.error(`commande inconnue : ${command}`);
      console.log(USAGE);
      process.exitCode = 1;
  }
}

main().catch((err) => {
  console.error(`\n✗ ${err instanceof AscError ? err.message : err.message || err}`);
  process.exitCode = 1;
});
