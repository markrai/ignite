#!/bin/sh
# Independently authenticate Dropbear 2024.85.
# The trusted signing key is taken from Debian packaging (not from the
# upstream tarball directory). The expected fingerprint is pinned below.
set -eu

# Fingerprint taken from Debian bookworm and Ubuntu universe packaging of
# debian/upstream/signing-key.asc (identical). Not taken from the upstream
# tarball directory.
PINNED_FPR="F7347EF2EE2E07A267628CA944931494F29C6773"
PINNED_SHA256="86b036c433a69d89ce51ebae335d65c47738ccf90d13e5eb0fea832e556da502"
VERSION="2024.85"
WORKDIR="${1:-/work/dropbear-src}"
mkdir -p "$WORKDIR"
cd "$WORKDIR"

echo "======== fetch Debian upstream signing key (independent of matt.ucc.asn.au) ========"
# Debian's debian.tar.xz is part of the Debian source package, signed in the
# archive by Debian. We extract only debian/upstream/signing-key.asc.
# Bookworm security/stable packaging; plus sign is %2B in the URL.
DEBIAN_TAR="dropbear_2022.83-1+deb12u3.debian.tar.xz"
if [ ! -f "$DEBIAN_TAR" ]; then
  curl -fsSL -o "$DEBIAN_TAR" \
    "https://deb.debian.org/debian/pool/main/d/dropbear/dropbear_2022.83-1%2Bdeb12u3.debian.tar.xz"
fi
rm -rf debian-key-extract
mkdir debian-key-extract
tar -xJf "$DEBIAN_TAR" -C debian-key-extract debian/upstream/signing-key.asc
DEBIAN_KEY="debian-key-extract/debian/upstream/signing-key.asc"
[ -s "$DEBIAN_KEY" ] || { echo "missing Debian signing-key.asc" >&2; exit 1; }

echo "======== fetch Ubuntu copy of the same upstream key (second distro) ========"
# Jammy still vendors the 2015 Dropbear key.
UBUNTU_TAR="dropbear_2022.83-4.debian.tar.xz"
if [ ! -f "$UBUNTU_TAR" ]; then
  curl -fsSL -o "$UBUNTU_TAR" \
    "http://archive.ubuntu.com/ubuntu/pool/universe/d/dropbear/${UBUNTU_TAR}"
fi
rm -rf ubuntu-key-extract
mkdir ubuntu-key-extract
tar -xzf "$UBUNTU_TAR" -C ubuntu-key-extract debian/upstream/signing-key.asc 2>/dev/null \
  || tar -xJf "$UBUNTU_TAR" -C ubuntu-key-extract debian/upstream/signing-key.asc
UBUNTU_KEY="ubuntu-key-extract/debian/upstream/signing-key.asc"
[ -s "$UBUNTU_KEY" ] || { echo "missing Ubuntu signing-key.asc" >&2; exit 1; }

GNUPGHOME="$WORKDIR/gnupg"
rm -rf "$GNUPGHOME"
mkdir -m 700 "$GNUPGHOME"
export GNUPGHOME

debian_fpr=$(gpg --with-colons --import-options show-only --import "$DEBIAN_KEY" \
  | awk -F: '/^fpr:/ {print $10; exit}')
ubuntu_fpr=$(gpg --with-colons --import-options show-only --import "$UBUNTU_KEY" \
  | awk -F: '/^fpr:/ {print $10; exit}')

echo "debian_fingerprint=$debian_fpr"
echo "ubuntu_fingerprint=$ubuntu_fpr"
echo "pinned_fingerprint=$PINNED_FPR"

[ "$debian_fpr" = "$PINNED_FPR" ] || { echo "Debian key fingerprint mismatch" >&2; exit 1; }
[ "$ubuntu_fpr" = "$PINNED_FPR" ] || { echo "Ubuntu key fingerprint mismatch" >&2; exit 1; }
[ "$debian_fpr" = "$ubuntu_fpr" ] || { echo "Debian/Ubuntu keys differ" >&2; exit 1; }

gpg --import "$DEBIAN_KEY"
gpg --fingerprint --with-colons | sed -n '1,20p'

echo "======== fetch official 2024.85 tarball + detached signature ========"
TAR="dropbear-${VERSION}.tar.bz2"
ASC="${TAR}.asc"
if [ ! -f "$TAR" ]; then
  curl -fsSL -o "$TAR" "https://matt.ucc.asn.au/dropbear/releases/${TAR}"
fi
if [ ! -f "$ASC" ]; then
  curl -fsSL -o "$ASC" "https://matt.ucc.asn.au/dropbear/releases/${ASC}"
fi

echo "======== GPG verify archive against Debian-imported key ========"
gpg --verify "$ASC" "$TAR"

SHA=$(sha256sum "$TAR" | awk '{print $1}')
echo "tarball_sha256=$SHA"
echo "tarball_bytes=$(wc -c < "$TAR" | tr -d ' ')"
[ "$SHA" = "$PINNED_SHA256" ] || {
  echo "tarball SHA-256 mismatch (got $SHA want $PINNED_SHA256)" >&2
  exit 1
}

# Record for the build pin file.
printf '%s\n' "$SHA" > "dropbear-${VERSION}.tar.bz2.sha256"
printf '%s\n' "$PINNED_FPR" > signing-key.fingerprint
cp "$DEBIAN_KEY" dropbear-debian-upstream-signing-key.asc

echo "PROVENANCE_OK version=$VERSION fingerprint=$PINNED_FPR sha256=$SHA"
