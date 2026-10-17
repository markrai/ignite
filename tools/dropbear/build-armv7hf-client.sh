#!/bin/sh
# Cross-compile a client-only Dropbear MULTI binary for Kindle (ARMv7hf, glibc 2.31 toolchain → runs on glibc 2.35).
# Expects verify-source.sh to have populated $WORKDIR with a verified tarball.
set -eu

VERSION="2024.85"
WORKDIR="${1:-/work/dropbear-src}"
OUTDIR="${2:-/work/dropbear-out}"
SCRIPT_DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)

sh "$SCRIPT_DIR/verify-source.sh" "$WORKDIR"

cd "$WORKDIR"
TAR="dropbear-${VERSION}.tar.bz2"
rm -rf "dropbear-${VERSION}"
tar -xjf "$TAR"
cd "dropbear-${VERSION}"

# Client-only; keep dbclient, pubkey auth, ED25519, -B netcat-alike, strict host keys.
# Do not compile the SSH server or scp.
cat > localoptions.h <<'EOF'
#define DROPBEAR_CLI_PASSWORD_AUTH 0
#define DROPBEAR_SVR_PASSWORD_AUTH 0
#define DROPBEAR_SVR_PAM_AUTH 0
EOF

./configure \
  --host=arm-linux-gnueabihf \
  --disable-zlib \
  --disable-pam \
  --disable-lastlog \
  --disable-utmp \
  --disable-utmpx \
  --disable-wtmp \
  --disable-wtmpx \
  --disable-loginfunc \
  --disable-pututline \
  --disable-pututxline \
  --enable-bundled-libtom

make -j"$(nproc)" PROGRAMS="dbclient" MULTI=1
arm-linux-gnueabihf-strip dropbearmulti

mkdir -p "$OUTDIR"
cp -a dropbearmulti "$OUTDIR/dropbearmulti"
file "$OUTDIR/dropbearmulti"
arm-linux-gnueabihf-readelf -h "$OUTDIR/dropbearmulti" | sed -n '1,20p'
echo '--- interpreter ---'
arm-linux-gnueabihf-readelf -l "$OUTDIR/dropbearmulti" | grep -E 'INTERP|Requesting' || true
echo '--- needed libs ---'
arm-linux-gnueabihf-readelf -d "$OUTDIR/dropbearmulti" | grep NEEDED || true
echo '--- glibc symbols ---'
arm-linux-gnueabihf-objdump -T "$OUTDIR/dropbearmulti" 2>/dev/null | grep -oE 'GLIBC_[0-9.]+' | sort -u
ELF_SHA=$(sha256sum "$OUTDIR/dropbearmulti" | awk '{print $1}')
ELF_BYTES=$(wc -c < "$OUTDIR/dropbearmulti" | tr -d ' ')
echo "elf_sha256=$ELF_SHA"
echo "elf_bytes=$ELF_BYTES"

# Help text smoke via qemu if present; never a network connection.
if command -v qemu-arm-static >/dev/null 2>&1; then
  echo '--- qemu dbclient -h (excerpt) ---'
  qemu-arm-static "$OUTDIR/dropbearmulti" dbclient -h 2>&1 | sed -n '1,80p' || true
fi

TARBALL_SHA=$(cat "$WORKDIR/dropbear-${VERSION}.tar.bz2.sha256")
FPR=$(cat "$WORKDIR/signing-key.fingerprint")
{
  echo "name=dropbearmulti"
  echo "role=ignite-recovery-ssh-client"
  echo "upstream_version=$VERSION"
  echo "tarball_sha256=$TARBALL_SHA"
  echo "signing_key_fingerprint=$FPR"
  echo "elf_sha256=$ELF_SHA"
  echo "elf_bytes=$ELF_BYTES"
  echo "program=dbclient"
  echo "multi=1"
  echo "host=arm-linux-gnueabihf"
} > "$OUTDIR/provenance.txt"

printf '%s\n' "$ELF_SHA" > "$OUTDIR/elf.sha256"
echo "BUILD_OK $OUTDIR/dropbearmulti"
