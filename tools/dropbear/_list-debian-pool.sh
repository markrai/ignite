#!/bin/sh
set -eu
curl -fsSL https://deb.debian.org/debian/pool/main/d/dropbear/ | tr '"' '\n' | grep '^dropbear_.*debian.tar' | sort -u
echo '--- ubuntu jammy ---'
curl -fsSL http://archive.ubuntu.com/ubuntu/pool/universe/d/dropbear/ | tr '"' '\n' | grep '^dropbear_.*debian.tar' | sort -u | tail -n 15
