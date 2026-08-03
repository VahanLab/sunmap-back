#!/usr/bin/env python3
# /// script
# requires-python = ">=3.10"
# ///
"""Purge le cache Cloudflare de la zone des tuiles.

    python3 scripts/cf-purge.py

À lancer **après chaque remplacement de `sunmap.pmtiles`** : le cache du
Worker est indexé sur l'URL de la tuile et ignore complètement le fait que
l'archive derrière ait changé — sans purge, les tuiles déjà servies restent
servies dans leur version précédente jusqu'à expiration du `Cache-Control`
(un jour par défaut).

Purge **totale de la zone**, faute de mieux : la purge par nom d'hôte ou par
préfixe est réservée aux offres Enterprise, et purger tuile par tuile
demanderait de lister des dizaines de milliers d'URL. Sans conséquence ici —
`sunmap.tech` et `www` sont en « DNS only » (servis par Vercel, jamais
cachés par Cloudflare), donc la seule chose réellement en cache dans cette
zone, ce sont les tuiles.

Deux variables, dans l'environnement ou `helios-server/.env` :

    CLOUDFLARE_ZONE_ID=…      # dashboard → sunmap.tech → Overview, colonne de droite
    CLOUDFLARE_PURGE_TOKEN=…  # jeton API dédié, cf. docs/import-zone.md

Le jeton doit porter la permission **Zone → Cache Purge → Purge**, limitée à
cette zone. Ce n'est pas le jeton R2 (identifiants S3, aucun droit sur le
cache) ni celui de wrangler (OAuth en `zone (read)` seulement).
"""

from __future__ import annotations

import json
import os
import sys
import urllib.error
import urllib.request

API = "https://api.cloudflare.com/client/v4/zones/{zone}/purge_cache"


def dotenv_value(key: str) -> str | None:
    env_path = os.path.join(os.path.dirname(__file__), "..", "helios-server", ".env")
    try:
        with open(env_path) as f:
            for line in f:
                line = line.strip()
                if line.startswith(f"{key}="):
                    return line.split("=", 1)[1] or None
    except OSError:
        pass
    return None


def setting(key: str) -> str | None:
    return os.environ.get(key) or dotenv_value(key)


def main() -> int:
    zone = setting("CLOUDFLARE_ZONE_ID")
    token = setting("CLOUDFLARE_PURGE_TOKEN")
    missing = [k for k, v in (("CLOUDFLARE_ZONE_ID", zone), ("CLOUDFLARE_PURGE_TOKEN", token)) if not v]
    if missing:
        # Pas une erreur fatale : l'import doit pouvoir tourner sans purge
        # configurée (développement, ou archive non téléversée).
        print(
            f"[purge] ignorée — {', '.join(missing)} absent(s). "
            "Sans purge, les tuiles déjà servies restent périmées jusqu'à "
            "expiration du cache (cf. docs/import-zone.md).",
            file=sys.stderr,
        )
        return 0

    req = urllib.request.Request(
        API.format(zone=zone),
        data=json.dumps({"purge_everything": True}).encode(),
        headers={
            "Authorization": f"Bearer {token}",
            "Content-Type": "application/json",
        },
        method="POST",
    )
    try:
        with urllib.request.urlopen(req, timeout=30) as resp:
            body = json.load(resp)
    except urllib.error.HTTPError as e:
        detail = e.read().decode(errors="replace")
        print(f"[purge] ÉCHEC HTTP {e.code} : {detail}", file=sys.stderr)
        return 1
    except OSError as e:
        print(f"[purge] ÉCHEC réseau : {e}", file=sys.stderr)
        return 1

    if not body.get("success"):
        print(f"[purge] ÉCHEC : {body.get('errors')}", file=sys.stderr)
        return 1
    print("[purge] cache de la zone vidé")
    return 0


if __name__ == "__main__":
    sys.exit(main())
