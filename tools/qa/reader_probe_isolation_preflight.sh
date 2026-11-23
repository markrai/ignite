#!/bin/sh
set -eu

fail() {
    printf 'READER_SANDBOX_PREFLIGHT_BLOCKED: %s\n' "$*" >&2
    exit 3
}

[ "$(id -u)" = 0 ] || fail "root required for namespace rehearsal"
[ "$(uname -m)" = armv7l ] || fail "ARMv7 Kindle required"
grep -q '^Kindle 5\.18\.6' /etc/prettyversion.txt || fail "unqualified Kindle firmware"

for tool in unshare mount umount chroot su mktemp readlink; do
    command -v "$tool" >/dev/null 2>&1 || fail "missing command: $tool"
done

QA_PARENT_MNT=$(readlink /proc/self/ns/mnt)
QA_PARENT_NET=$(readlink /proc/self/ns/net)
QA_PARENT_PID=$(readlink /proc/self/ns/pid)
export QA_PARENT_MNT QA_PARENT_NET QA_PARENT_PID

if ps | grep -E '[s]cribe-app|[s]cribe-lifecycle' >/dev/null 2>&1; then
    printf 'reader_sandbox_ignite_running=yes\n'
else
    printf 'reader_sandbox_ignite_running=no\n'
fi

unshare -m -n -p -f sh -c '
    set -eu

    [ "$(readlink /proc/self/ns/mnt)" != "$QA_PARENT_MNT" ] || exit 3
    [ "$(readlink /proc/self/ns/net)" != "$QA_PARENT_NET" ] || exit 3
    [ "$(readlink /proc/self/ns/pid)" != "$QA_PARENT_PID" ] || exit 3

    mount --make-rprivate / || exit 3
    root=$(mktemp -d /tmp/ignite-reader-isolation.XXXXXX) || exit 3

    cleanup() {
        umount "$root/proc" 2>/dev/null || true
        umount "$root/dev/null" 2>/dev/null || true
        umount "$root/usr/lib" 2>/dev/null || true
        umount "$root/lib" 2>/dev/null || true
        umount "$root" 2>/dev/null || true
        rmdir "$root" 2>/dev/null || true
    }

    trap cleanup 0 1 2 15
    mount -t tmpfs -o size=8m,nosuid,nodev tmpfs "$root" || exit 3
    mkdir -p "$root/bin" "$root/etc" "$root/lib" "$root/usr/lib" "$root/proc" \
        "$root/tmp" "$root/dev"

    mount --bind /lib "$root/lib" || exit 3
    mount -o remount,bind,ro "$root/lib" || exit 3
    mount --bind /usr/lib "$root/usr/lib" || exit 3
    mount -o remount,bind,ro "$root/usr/lib" || exit 3

    : > "$root/dev/null"
    mount --bind /dev/null "$root/dev/null" || exit 3
    mount -t proc -o nosuid,nodev,noexec proc "$root/proc" || exit 3

    cp /bin/busybox "$root/bin/busybox" || exit 3
    ln -s busybox "$root/bin/sh"
    ln -s busybox "$root/bin/id"
    printf "root:x:0:0:root:/root:/bin/sh\nnobody:x:99:99:nobody:/tmp:/bin/sh\n" > "$root/etc/passwd"
    printf "root:x:0:\nnobody:x:99:\n" > "$root/etc/group"
    chmod 0555 "$root" "$root/bin" "$root/etc" "$root/tmp" "$root/bin/busybox"
    chmod 0444 "$root/etc/passwd" "$root/etc/group"

    chroot "$root" /bin/busybox su -s /bin/sh nobody -c \
        "test \"\$(/bin/id -u)\" = 99 && test -r /proc/self/status && test ! -e /mnt/us/scribe-data && test ! -e /mnt/us/scribe-app" \
        || exit 3

    printf "reader_sandbox_mount_namespace=private\n"
    printf "reader_sandbox_network_namespace=private\n"
    printf "reader_sandbox_pid_namespace=private\n"
    printf "reader_sandbox_chroot_uid=99\n"
    printf "reader_sandbox_prod_paths=unavailable\n"
' || fail "isolation rehearsal failed"

printf 'reader_sandbox_reader_executed=no\n'
printf 'READER_SANDBOX_PREFLIGHT_PASS\n'
