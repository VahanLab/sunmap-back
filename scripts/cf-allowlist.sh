#!/usr/bin/env bash
# N'accepte 80/443 que depuis les plages IP Cloudflare — sinon l'IP de la VM,
# si elle fuite, permet de contourner le proxy (et son TLS, son rate limiting).
#
# Les règles vivent dans la chaîne DOCKER-USER : le trafic vers un port publié
# par Docker traverse la table FORWARD via cette chaîne, PAS les chaînes INPUT
# où ufw opère — une règle ufw sur 80/443 serait décorative (cf.
# docs/deploiement-ovh.md § 2).
#
# Idempotent : rejouable pour rafraîchir les plages (Cloudflare les fait
# évoluer rarement mais les fait évoluer). Installé en service systemd au
# boot + timer hebdomadaire, cf. docs/deploiement-ovh.md § 3.
set -euo pipefail

IFACE=$(ip route | awk '/^default/ {print $5; exit}')
V4=$(curl -fsS https://www.cloudflare.com/ips-v4)
V6=$(curl -fsS https://www.cloudflare.com/ips-v6)
[ -n "$V4" ] && [ -n "$V6" ]

# Chaîne dédiée : RETURN pour Cloudflare (le paquet poursuit son chemin
# normal), DROP pour le reste. Reconstruite à chaque exécution.
iptables -N CF-ALLOW 2>/dev/null || iptables -F CF-ALLOW
for r in $V4; do iptables -A CF-ALLOW -s "$r" -j RETURN; done
iptables -A CF-ALLOW -j DROP

ip6tables -N CF-ALLOW 2>/dev/null || ip6tables -F CF-ALLOW
for r in $V6; do ip6tables -A CF-ALLOW -s "$r" -j RETURN; done
ip6tables -A CF-ALLOW -j DROP

# Accroche en tête de DOCKER-USER (retirée d'abord si déjà posée : pas de
# doublon au rejeu). Seul le trafic entrant de l'interface publique vers
# 80/443 est concerné — l'inter-conteneurs et le port 81 (lié à 127.0.0.1,
# jamais forwardé) ne passent pas par là.
iptables -D DOCKER-USER -i "$IFACE" -p tcp -m multiport --dports 80,443 -j CF-ALLOW 2>/dev/null || true
iptables -I DOCKER-USER -i "$IFACE" -p tcp -m multiport --dports 80,443 -j CF-ALLOW
ip6tables -D DOCKER-USER -i "$IFACE" -p tcp -m multiport --dports 80,443 -j CF-ALLOW 2>/dev/null || true
ip6tables -I DOCKER-USER -i "$IFACE" -p tcp -m multiport --dports 80,443 -j CF-ALLOW

echo "[cf-allowlist] $(echo "$V4" | wc -l) plages v4, $(echo "$V6" | wc -l) plages v6, interface $IFACE"
