# Ignite Server firewall integration (sourced by install.sh / uninstall.sh). bash.
#
# Opens exactly what the product needs, ONLY on the eligible LAN interfaces reported by the
# shared network policy (`scribe-server lan-interfaces`; never tailscale*/wg*/docker*/br-*/
# veth*/…):
#   TCP <port>                     the Ignite Server listener (ignite-web + sync + pairing)
#   UDP 5353 to 224.0.0.251        mDNS queries, so Kindles/browsers can discover the serve
# The application's own accept policy stays authoritative even without any firewall.
#
# Ownership invariant: remove ONLY rules we can prove we own.
#   ufw        every owned rule carries the comment "scribe-server" (stable tag; do not rename
#              casually — uninstall must find rules installed by prior releases);
#              removal deletes exactly those numbered entries, highest number first.
#   firewalld  owns the service definition "scribe-server" and removes it only from the
#              zones where it is enabled; no other service/port is touched.
# Supported managers: ufw (active), firewalld (running). A custom restrictive nftables/iptables
# ruleset is reported, never edited. Nothing is ever disabled or flushed.

SCRIBE_FW_TAG="scribe-server"

fw_detect() {
  if command -v ufw >/dev/null 2>&1 && ufw status 2>/dev/null | grep -q "^Status: active"; then
    echo ufw
  elif command -v firewall-cmd >/dev/null 2>&1 && firewall-cmd --state >/dev/null 2>&1; then
    echo firewalld
  elif command -v nft >/dev/null 2>&1 && nft list ruleset 2>/dev/null | grep -Eq "hook input .*policy drop"; then
    echo custom-nftables
  elif command -v iptables >/dev/null 2>&1 && iptables -S INPUT 2>/dev/null | grep -q "^-P INPUT DROP"; then
    echo custom-iptables
  else
    echo none
  fi
}

fw_ufw_owned_numbers() {
  ufw status numbered 2>/dev/null \
    | sed -n "s/^\[ *\([0-9][0-9]*\)\].*# ${SCRIBE_FW_TAG}\$/\1/p" \
    | sort -rn
}

fw_ufw_foreign_exists() {
  local to_re="$1" ifc="$2"
  ufw status numbered 2>/dev/null \
    | grep -E "^\[ *[0-9]+\] ${to_re} on ${ifc} +ALLOW IN" \
    | grep -vq "# ${SCRIBE_FW_TAG}\$"
}

fw_ufw_remove_owned() {
  local n
  for n in $(fw_ufw_owned_numbers); do
    ufw --force delete "$n" >/dev/null
  done
}

fw_firewalld_owned_zones() {
  local z
  for z in $(firewall-cmd --permanent --get-zones 2>/dev/null); do
    firewall-cmd --permanent --zone="$z" --query-service="${SCRIBE_FW_TAG}" >/dev/null 2>&1 && echo "$z"
  done
}

fw_firewalld_zone_of() {
  local z
  z="$(firewall-cmd --get-zone-of-interface="$1" 2>/dev/null || true)"
  [ -n "$z" ] || z="$(firewall-cmd --get-default-zone)"
  echo "$z"
}

fw_plan() {
  local port="$1"; shift
  local mech ifc
  mech="$(fw_detect)"
  case "$mech" in
    ufw)
      for ifc in "$@"; do
        echo "ufw allow in on ${ifc} proto tcp to any port ${port} comment ${SCRIBE_FW_TAG}"
        echo "ufw allow in on ${ifc} proto udp to 224.0.0.251 port 5353 comment ${SCRIBE_FW_TAG}"
      done
      ;;
    firewalld)
      echo "install /etc/firewalld/services/${SCRIBE_FW_TAG}.xml (tcp ${port}, udp 5353)"
      for ifc in "$@"; do echo "firewall-cmd --permanent --zone=$(fw_firewalld_zone_of "$ifc") --add-service=${SCRIBE_FW_TAG}"; done
      ;;
    none) echo "# no active firewall: nothing to change" ;;
    *) echo "# ${mech}: custom restrictive ruleset detected; not modified automatically" ;;
  esac
}

fw_open() {
  local port="$1"; shift
  local mech ifc zones=""
  mech="$(fw_detect)"
  if [ "$#" -eq 0 ] && [ "$mech" != none ]; then
    echo "WARNING: no eligible LAN interface found; firewall not changed." >&2
    echo "skipped ${port}"
    return
  fi
  case "$mech" in
    ufw)
      fw_ufw_remove_owned
      for ifc in "$@"; do
        fw_ufw_foreign_exists "${port}/tcp" "$ifc" \
          || ufw allow in on "$ifc" proto tcp to any port "$port" comment "${SCRIBE_FW_TAG}" >/dev/null
        fw_ufw_foreign_exists "224\.0\.0\.251 5353/udp" "$ifc" \
          || ufw allow in on "$ifc" proto udp to 224.0.0.251 port 5353 comment "${SCRIBE_FW_TAG}" >/dev/null
      done
      echo "ufw ${port} $*"
      ;;
    firewalld)
      cat >"/etc/firewalld/services/${SCRIBE_FW_TAG}.xml" <<XML
<?xml version="1.0" encoding="utf-8"?>
<service>
  <short>Ignite Server</short>
  <description>Ignite Server listener and mDNS discovery (trusted LAN).</description>
  <port protocol="tcp" port="${port}"/>
  <port protocol="udp" port="5353"/>
</service>
XML
      firewall-cmd --reload >/dev/null
      for ifc in "$@"; do
        local z; z="$(fw_firewalld_zone_of "$ifc")"
        firewall-cmd --permanent --zone="$z" --add-service="${SCRIBE_FW_TAG}" >/dev/null
        zones="${zones:+$zones,}$z"
      done
      firewall-cmd --reload >/dev/null
      echo "firewalld ${port} ${zones}"
      ;;
    none) echo "none ${port}" ;;
    *)
      echo "WARNING: ${mech} firewall detected. Ignite Server needs TCP ${port} and UDP 5353 (mDNS) from the LAN interface(s) $*; the installer does not edit custom rulesets." >&2
      echo "skipped ${port}"
      ;;
  esac
}

fw_close() {
  local mech
  read -r mech _ <<<"$1"
  case "$mech" in
    ufw)
      command -v ufw >/dev/null 2>&1 && fw_ufw_remove_owned
      ;;
    firewalld)
      if command -v firewall-cmd >/dev/null 2>&1 && firewall-cmd --state >/dev/null 2>&1; then
        local z
        for z in $(fw_firewalld_owned_zones); do
          firewall-cmd --permanent --zone="$z" --remove-service="${SCRIBE_FW_TAG}" >/dev/null 2>&1 || true
        done
        firewall-cmd --reload >/dev/null 2>&1 || true
      fi
      rm -f "/etc/firewalld/services/${SCRIBE_FW_TAG}.xml"
      command -v firewall-cmd >/dev/null 2>&1 && firewall-cmd --reload >/dev/null 2>&1 || true
      ;;
    *) ;;
  esac
}
