// Jeton JWT ES256 pour l'API App Store Connect.
// Apple impose : alg ES256, kid = identifiant de clé, aud "appstoreconnect-v1",
// et une durée de vie de 20 minutes maximum (on prend 15 pour la marge).
import { createPrivateKey, sign } from 'node:crypto';

const MAX_TTL_S = 20 * 60;

function b64url(input) {
  return Buffer.from(input)
    .toString('base64')
    .replace(/\+/g, '-')
    .replace(/\//g, '_')
    .replace(/=+$/, '');
}

/**
 * @param {{keyId: string, issuerId?: string, privateKeyPem: string, ttlSeconds?: number}} opts
 * @returns {{token: string, expiresAt: number}}
 */
export function makeToken({ keyId, issuerId, privateKeyPem, ttlSeconds = 15 * 60 }) {
  if (ttlSeconds > MAX_TTL_S) throw new Error(`ttlSeconds > ${MAX_TTL_S} : Apple refuse le jeton`);

  const key = createPrivateKey(privateKeyPem);
  if (key.asymmetricKeyType !== 'ec') {
    throw new Error(`clé privée de type ${key.asymmetricKeyType}, attendu ec (fichier .p8 App Store Connect)`);
  }

  const now = Math.floor(Date.now() / 1000);
  const exp = now + ttlSeconds;

  const header = { alg: 'ES256', kid: keyId, typ: 'JWT' };
  // Clé d'équipe : iss = issuer id. Clé individuelle : pas d'issuer, sub = "user".
  const payload = issuerId
    ? { iss: issuerId, iat: now, exp, aud: 'appstoreconnect-v1' }
    : { sub: 'user', iat: now, exp, aud: 'appstoreconnect-v1' };

  const signingInput = `${b64url(JSON.stringify(header))}.${b64url(JSON.stringify(payload))}`;
  // ieee-p1363 = signature brute R||S, ce qu'attend JWS (et pas le DER par défaut de Node).
  const signature = sign('sha256', Buffer.from(signingInput), { key, dsaEncoding: 'ieee-p1363' });

  return { token: `${signingInput}.${b64url(signature)}`, expiresAt: exp };
}
