// Client HTTP minimal pour l'API App Store Connect : jeton renouvelé tout seul,
// pagination suivie, et reprise sur 429 / 5xx.
import { makeToken } from './jwt.js';

// ASC_API_BASE n'existe que pour brancher un serveur factice pendant les tests.
const BASE = process.env.ASC_API_BASE || 'https://api.appstoreconnect.apple.com';

export class AscError extends Error {
  constructor(status, body, method, url) {
    const errors = Array.isArray(body?.errors) ? body.errors : [];
    const detail = errors.length
      ? errors.map((e) => `${e.status} ${e.code} — ${e.title}${e.detail ? ` : ${e.detail}` : ''}`).join('\n  ')
      : typeof body === 'string'
        ? body.slice(0, 500)
        : JSON.stringify(body).slice(0, 500);
    super(`${method} ${url} → ${status}\n  ${detail}`);
    this.name = 'AscError';
    this.status = status;
    this.errors = errors;
  }
}

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

export class AscClient {
  /** @param {{keyId: string, issuerId?: string, privateKeyPem: string, verbose?: boolean}} opts */
  constructor({ keyId, issuerId, privateKeyPem, verbose = false }) {
    this.keyId = keyId;
    this.issuerId = issuerId;
    this.privateKeyPem = privateKeyPem;
    this.verbose = verbose;
    this._token = null;
  }

  token() {
    const now = Math.floor(Date.now() / 1000);
    if (!this._token || this._token.expiresAt - now < 60) {
      this._token = makeToken({ keyId: this.keyId, issuerId: this.issuerId, privateKeyPem: this.privateKeyPem });
    }
    return this._token.token;
  }

  async request(method, path, { query, body, retries = 4 } = {}) {
    const url = new URL(path.startsWith('http') ? path : `${BASE}${path}`);
    for (const [k, v] of Object.entries(query ?? {})) {
      if (v === undefined || v === null) continue;
      url.searchParams.set(k, Array.isArray(v) ? v.join(',') : String(v));
    }

    for (let attempt = 0; ; attempt++) {
      if (this.verbose) console.error(`  → ${method} ${url.pathname}${url.search}`);
      const res = await fetch(url, {
        method,
        headers: {
          Authorization: `Bearer ${this.token()}`,
          ...(body ? { 'Content-Type': 'application/json' } : {}),
        },
        body: body ? JSON.stringify(body) : undefined,
      });

      if (res.status === 429 || (res.status >= 500 && res.status < 600)) {
        if (attempt < retries) {
          const wait = Math.min(30_000, 1000 * 2 ** attempt);
          console.error(`  ! ${res.status} sur ${method} ${url.pathname} — nouvelle tentative dans ${wait / 1000}s`);
          await sleep(wait);
          continue;
        }
      }

      if (res.status === 204) return null;

      const text = await res.text();
      let parsed = text;
      try {
        parsed = text ? JSON.parse(text) : null;
      } catch {
        /* réponse non-JSON : on garde le texte brut pour le message d'erreur */
      }
      if (!res.ok) throw new AscError(res.status, parsed, method, url.pathname);
      return parsed;
    }
  }

  get(path, query) {
    return this.request('GET', path, { query });
  }
  post(path, body) {
    return this.request('POST', path, { body });
  }
  patch(path, body) {
    return this.request('PATCH', path, { body });
  }
  delete(path) {
    return this.request('DELETE', path);
  }

  /** GET en suivant links.next ; renvoie la concaténation des `data`. */
  async getAll(path, query) {
    const out = [];
    let page = await this.get(path, { limit: 200, ...query });
    for (;;) {
      out.push(...(page.data ?? []));
      const next = page.links?.next;
      if (!next) return out;
      page = await this.get(next);
    }
  }
}
