#!/usr/bin/env python3
# /// script
# requires-python = ">=3.10"
# dependencies = ["boto3"]
# ///
"""Télécharge un ou plusieurs objets du bucket Cloudflare R2.

    python3 scripts/r2-download.py sunmap.pmtiles tiles/

Pendant de ``r2-upload.py``, pour la VM applicative : l'archive vectorielle
est générée et poussée sur R2 par la VM d'import, la VM applicative la
rapatrie d'ici (le serveur lit un fichier local, `VECTOR_TILES`).

Identifiants : variables ``R2_ACCOUNT_ID``, ``R2_ACCESS_KEY_ID``,
``R2_SECRET_ACCESS_KEY``, ``R2_BUCKET`` — depuis l'environnement, sinon
``helios-server/.env`` (gitignoré). Sur la VM applicative, un jeton
**Object Read only** limité au bucket suffit — ne pas y poser le jeton
d'écriture de la VM d'import.

Le téléchargement passe par un fichier temporaire renommé à la fin : le
serveur qui mmap l'archive ne voit jamais un fichier tronqué. Redémarrer
l'API après remplacement (`docker compose restart api`).
"""

from __future__ import annotations

import os
import sys


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


def main():
    args = sys.argv[1:]
    if not args:
        raise SystemExit("Usage : r2-download.py <clé> [clé…] [répertoire-cible]")
    if len(args) > 1 and os.path.isdir(args[-1]):
        dest_dir, keys = args[-1], args[:-1]
    else:
        dest_dir, keys = ".", args

    env_keys = ("R2_ACCOUNT_ID", "R2_ACCESS_KEY_ID", "R2_SECRET_ACCESS_KEY", "R2_BUCKET")
    r2 = {k: os.environ.get(k) or dotenv_value(k) for k in env_keys}
    missing = [k for k, v in r2.items() if not v]
    if missing:
        raise SystemExit(
            f"variables R2 manquantes : {', '.join(missing)} "
            "(environnement ou helios-server/.env — cf. docs/deploiement-ovh.md)"
        )

    import boto3

    s3 = boto3.client(
        "s3",
        endpoint_url=f"https://{r2['R2_ACCOUNT_ID']}.r2.cloudflarestorage.com",
        aws_access_key_id=r2["R2_ACCESS_KEY_ID"],
        aws_secret_access_key=r2["R2_SECRET_ACCESS_KEY"],
        region_name="auto",
    )
    for key in keys:
        dest = os.path.join(dest_dir, os.path.basename(key))
        tmp = dest + ".part"
        head = s3.head_object(Bucket=r2["R2_BUCKET"], Key=key)
        size_mb = head["ContentLength"] / 1e6
        print(f"[r2] s3://{r2['R2_BUCKET']}/{key} ({size_mb:.1f} Mo) → {dest}")
        s3.download_file(r2["R2_BUCKET"], key, tmp)
        os.replace(tmp, dest)
    print("[r2] terminé")


if __name__ == "__main__":
    main()
