// Résolution des objets App Store Connect : app, fiche (appInfo) et version en préparation.
import { EDITABLE_VERSION_STATES } from './config.js';

export async function resolveApp(client, { appId, bundleId }) {
  if (appId) {
    const res = await client.get(`/v1/apps/${appId}`);
    return res.data;
  }
  if (!bundleId) throw new Error('ASC_APP_ID ou ASC_BUNDLE_ID requis');
  const apps = await client.getAll('/v1/apps', { 'filter[bundleId]': bundleId });
  if (apps.length === 0) throw new Error(`aucune app pour le bundle id ${bundleId}`);
  return apps[0];
}

/** La fiche modifiable (celle qui n'est pas encore distribuée), sinon la seule existante. */
export async function resolveAppInfo(client, appId) {
  const infos = await client.getAll(`/v1/apps/${appId}/appInfos`);
  if (infos.length === 0) throw new Error('aucun appInfo sur cette app');
  const editable = infos.find((i) => (i.attributes?.state ?? i.attributes?.appStoreState) !== 'READY_FOR_DISTRIBUTION');
  return editable ?? infos[0];
}

/** La version en préparation pour la plateforme demandée. */
export async function resolveVersion(client, appId, platform) {
  const versions = await client.getAll(`/v1/apps/${appId}/appStoreVersions`, {
    'filter[platform]': platform,
    limit: 50,
  });
  const state = (v) => v.attributes?.appVersionState ?? v.attributes?.appStoreState;
  const editable = versions.find((v) => EDITABLE_VERSION_STATES.has(state(v)));
  if (!editable) {
    const seen = versions.slice(0, 5).map((v) => `${v.attributes?.versionString} (${state(v)})`).join(', ');
    throw new Error(
      `aucune version modifiable pour ${platform}. Versions vues : ${seen || 'aucune'}.\n` +
        "Créer une nouvelle version dans App Store Connect, ou lancer avec ASC_VERSION_ID=<id>.",
    );
  }
  return editable;
}

export async function versionLocalizations(client, versionId) {
  const list = await client.getAll(`/v1/appStoreVersions/${versionId}/appStoreVersionLocalizations`);
  return new Map(list.map((l) => [l.attributes.locale, l]));
}

export async function infoLocalizations(client, appInfoId) {
  const list = await client.getAll(`/v1/appInfos/${appInfoId}/appInfoLocalizations`);
  return new Map(list.map((l) => [l.attributes.locale, l]));
}

export async function context(client, cfg) {
  const app = await resolveApp(client, cfg);
  const appInfo = await resolveAppInfo(client, app.id);
  const version = process.env.ASC_VERSION_ID
    ? (await client.get(`/v1/appStoreVersions/${process.env.ASC_VERSION_ID}`)).data
    : await resolveVersion(client, app.id, cfg.platform);
  return { app, appInfo, version };
}
