#!/bin/sh
# Refreshes the Russian IPv4 networks used by smart routing (geoip:ru from
# Loyalsoldier/geoip, the list v2ray/Xray "geoip:ru → direct" rules use),
# collapsed to the fewest prefixes. Rebuild the helper afterwards.
set -eu
cd "$(dirname "$0")/.."
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
curl -sSfL -o "$tmp/ru.txt" https://raw.githubusercontent.com/Loyalsoldier/geoip/release/text/ru.txt
python3 -I - "$tmp/ru.txt" crates/prism-engine/data/geoip-ru-v4.txt <<'PY'
import ipaddress, sys, datetime
nets = []
for line in open(sys.argv[1]):
    line = line.strip()
    if line and ":" not in line and not line.startswith("#"):
        nets.append(ipaddress.ip_network(line, strict=False))
out = list(ipaddress.collapse_addresses(nets))
if len(out) < 1000:
    sys.exit(f"suspiciously short list ({len(out)} networks), not writing")
with open(sys.argv[2], "w") as f:
    f.write(f"# geoip:ru IPv4 (Loyalsoldier/geoip), {datetime.date.today()}, {len(out)} networks\n")
    for n in out:
        f.write(f"{n}\n")
print(f"{len(nets)} entries -> {len(out)} networks")
PY
