# Image de production du backend SunMap (helios-server).
#
# Contexte de build = racine du repo : le workspace Cargo (helios-core +
# helios-server) doit être visible en entier, helios-server dépendant de
# helios-core par chemin relatif.
#
#   docker build -t sunmap-api .
#   docker run -e DATABASE_URL=... -p 8080:8080 sunmap-api

# --------------------------------------------------------------- build
FROM rust:1-slim AS builder

WORKDIR /app
COPY Cargo.toml Cargo.lock ./
COPY helios-core helios-core
COPY helios-server helios-server

# `--locked` : le Cargo.lock du repo fait foi, pas de résolution surprise au
# moment du build. Les migrations SQL sont embarquées dans le binaire par
# `sqlx::migrate!` — rien d'autre à copier dans l'image finale.
RUN cargo build --release --locked -p helios-server

# --------------------------------------------------------------- runtime
FROM debian:bookworm-slim

# ca-certificates : TLS sortant (tuiles Mapterhorn, clés Firebase) et TLS vers
# la base managée OVH (sqlx/rustls lit le magasin système).
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*

# Le serveur, et l'outil d'import PBF — pour pouvoir remplir la base managée
# depuis la VM (`docker compose run --rm api import extrait.geojsonl`).
COPY --from=builder /app/target/release/helios-server /usr/local/bin/helios-server
COPY --from=builder /app/target/release/import /usr/local/bin/import

# Processus non-root : l'app n'écrit rien sur disque, aucun privilège requis.
RUN useradd --system --no-create-home sunmap
USER sunmap

EXPOSE 8080
CMD ["helios-server"]
