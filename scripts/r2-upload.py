#!/usr/bin/env python3
# /// script
# requires-python = ">=3.10"
# dependencies = ["boto3"]
# ///
"""Pousse un ou plusieurs fichiers sur le bucket Cloudflare R2.

    .venv-tiles/bin/python scripts/r2-upload.py tiles/sunmap.pmtiles

Identifiants : variables ``R2_ACCOUNT_ID``, ``R2_ACCESS_KEY_ID``,
``R2_SECRET_ACCESS_KEY``, ``R2_BUCKET`` — depuis l'environnement, sinon
``helios-server/.env`` (gitignoré). Multipart géré par boto3 : un fichier de
plusieurs Go passe sans réglage. Le remplacement d'un objet est atomique du
point de vue des lecteurs (nouvel etag).
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
    paths = sys.argv[1:]
    if not paths:
        raise SystemExit("Usage : r2-upload.py <fichier> [fichier…]")
    for path in paths:
        if not os.path.isfile(path):
            raise SystemExit(f"fichier introuvable : {path}")

    keys = ("R2_ACCOUNT_ID", "R2_ACCESS_KEY_ID", "R2_SECRET_ACCESS_KEY", "R2_BUCKET")
    r2 = {k: os.environ.get(k) or dotenv_value(k) for k in keys}
    missing = [k for k, v in r2.items() if not v]
    if missing:
        raise SystemExit(
            f"variables R2 manquantes : {', '.join(missing)} "
            "(environnement ou helios-server/.env — cf. docs/import-zone.md)"
        )

    import boto3

    s3 = boto3.client(
        "s3",
        endpoint_url=f"https://{r2['R2_ACCOUNT_ID']}.r2.cloudflarestorage.com",
        aws_access_key_id=r2["R2_ACCESS_KEY_ID"],
        aws_secret_access_key=r2["R2_SECRET_ACCESS_KEY"],
        region_name="auto",
    )
    for path in paths:
        key = os.path.basename(path)
        size_mb = os.path.getsize(path) / 1e6
        print(f"[r2] {path} ({size_mb:.1f} Mo) → s3://{r2['R2_BUCKET']}/{key}")
        s3.upload_file(path, r2["R2_BUCKET"], key)
    print("[r2] terminé")


if __name__ == "__main__":
    main()
