#!/bin/sh
set -eu
echo "=== TOUCH_EVENT4_OWNERS ==="
for pid in $(ls -d /proc/[0-9]* 2>/dev/null | sed 's|/proc/||'); do
    [ -d "/proc/$pid/fd" ] || continue
    for fd in /proc/$pid/fd/*; do
        t=$(readlink "$fd" 2>/dev/null || true)
        case "$t" in
            /dev/input/event4|*/event4)
                echo "pid=$pid exe=$(readlink -f /proc/$pid/exe 2>/dev/null) -> $t"
                ;;
        esac
    done
done

echo "=== EVENT0_2_OWNERS_SUMMARY ==="
for n in 0 1 2 4 5; do
    echo "-- event$n --"
    for pid in $(ls -d /proc/[0-9]* 2>/dev/null | sed 's|/proc/||'); do
        [ -d "/proc/$pid/fd" ] || continue
        for fd in /proc/$pid/fd/*; do
            t=$(readlink "$fd" 2>/dev/null || true)
            case "$t" in
                /dev/input/event$n)
                    echo "pid=$pid exe=$(readlink -f /proc/$pid/exe 2>/dev/null)"
                    ;;
            esac
        done
    done
done

echo "=== NOTEBOOKAPPMGR_PROC ==="
for pid in $(ls -d /proc/[0-9]* 2>/dev/null | sed 's|/proc/||'); do
    exe=$(readlink -f "/proc/$pid/exe" 2>/dev/null || true)
    cmd=$(tr '\000' ' ' <"/proc/$pid/cmdline" 2>/dev/null || true)
    case "$exe$cmd" in
        *notebook*|*Notebook*|*KPPMainApp*)
            echo "pid=$pid exe=$exe cmd=$cmd"
            ;;
    esac
done

echo "=== WETINK_LIPC ==="
lipc-probe -a com.lab126.wetinkhandler 2>&1 | head -40 || true
