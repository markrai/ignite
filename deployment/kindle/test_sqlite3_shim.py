#!/usr/bin/env python3
"""Minimal sqlite3 CLI for host tests of register-helpers.sh.

Supports: sqlite3 [-readonly] DB [SQL] and SQL on stdin.
Prints SELECT/PRAGMA rows in sqlite3 list mode (|).
"""

from __future__ import annotations

import sqlite3
import sys


def _split_sql(sql: str) -> list[str]:
    stmts: list[str] = []
    buf: list[str] = []
    in_str = False
    i = 0
    while i < len(sql):
        c = sql[i]
        if c == "'" and not in_str:
            in_str = True
            buf.append(c)
        elif c == "'" and in_str:
            if i + 1 < len(sql) and sql[i + 1] == "'":
                buf.append("''")
                i += 1
            else:
                in_str = False
                buf.append(c)
        elif c == ";" and not in_str:
            text = "".join(buf).strip()
            if text:
                stmts.append(text)
            buf = []
        else:
            buf.append(c)
        i += 1
    text = "".join(buf).strip()
    if text:
        stmts.append(text)
    return stmts


def _strip_dot_commands(sql: str) -> tuple[str, bool]:
    bail = False
    kept: list[str] = []
    for line in sql.splitlines():
        stripped = line.strip()
        if stripped.startswith("."):
            if stripped.lower() == ".bail on":
                bail = True
            continue
        kept.append(line)
    return "\n".join(kept), bail


def main(argv: list[str]) -> int:
    args = argv[1:]
    readonly = False
    if args and args[0] == "-readonly":
        readonly = True
        args = args[1:]
    if not args:
        return 2
    db = args[0]
    sql_arg = " ".join(args[1:]).strip()
    sql_stdin = ""
    if not sys.stdin.isatty():
        sql_stdin = sys.stdin.read()
    sql = "\n".join(part for part in (sql_arg, sql_stdin) if part).strip()
    if not sql:
        return 0
    sql, bail = _strip_dot_commands(sql)
    if readonly:
        con = sqlite3.connect(f"file:{db}?mode=ro", uri=True, timeout=8.0)
    else:
        con = sqlite3.connect(db, timeout=8.0)
    con.isolation_level = None
    cur = None
    try:
        cur = con.cursor()
        for stmt in _split_sql(sql):
            try:
                cur.execute(stmt)
            except sqlite3.Error:
                try:
                    cur.execute("ROLLBACK")
                except sqlite3.Error:
                    pass
                if bail:
                    raise
                raise
            if cur.description:
                for row in cur.fetchall():
                    print("|".join("" if col is None else str(col) for col in row))
        return 0
    except sqlite3.Error as exc:
        if cur is not None:
            try:
                cur.execute("ROLLBACK")
            except sqlite3.Error:
                pass
        print(str(exc), file=sys.stderr)
        return 1
    finally:
        con.close()


if __name__ == "__main__":
    raise SystemExit(main(sys.argv))
