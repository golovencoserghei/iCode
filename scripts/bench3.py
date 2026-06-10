#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""
Тройное сравнение на ОДНИХ задачах: baseline (grep+чтение) vs codegraph vs iCode.

Одна методика для всех трёх: на каждый символ — «понять символ» = три retrieval-
операции (locate + callers + callees), токены = символы/4 по payload, который
увидел бы агент.
  * baseline : grep по дереву + чтение файлов с совпадениями (то, что делает
               агент без индекса).
  * codegraph: codegraph query/callers/callees -j.
  * iCode    : icode query --json / get-callers / get-callees.

Замечание о методике: это retrieval-уровень (не агентская сессия целиком, как в
опубликованном бенчмарке codegraph). Зато все три инструмента меряются одинаково
и честно — «сколько токенов и времени стоит один и тот же ответ».

Использование:
    python3 scripts/bench3.py --repo /path --symbols a,b,c
Зависимости: stdlib; бинари icode и codegraph; rg (иначе grep).
"""

import argparse
import os
import re
import shutil
import statistics
import subprocess
import sys
import time
from pathlib import Path

ANSI = re.compile(r"\x1b\[[0-9;]*[A-Za-z]")
BASELINE_MAX_FILES = 20
BASELINE_MAX_BYTES_PER_FILE = 64 * 1024


def est(s: str) -> int:
    return (len(ANSI.sub("", s)) + 3) // 4


def run(cmd, cwd=None):
    t0 = time.perf_counter()
    try:
        r = subprocess.run(cmd, cwd=cwd, capture_output=True, text=True, timeout=180)
        out = r.stdout
    except subprocess.TimeoutExpired:
        out = ""
    return out, time.perf_counter() - t0


def find_bin(explicit, names, hint):
    if explicit:
        return explicit
    # Для icode сначала берём свежую локальную сборку — на PATH может висеть
    # устаревший установленный бинарь (~/.local/bin/icode), который исказит замер.
    if "icode" in names:
        here = Path(__file__).resolve().parent.parent
        for c in (here / "target/release/icode", here / "target/debug/icode"):
            if c.exists():
                return str(c)
    for n in names:
        f = shutil.which(n)
        if f:
            return f
    sys.exit(f"не найден бинарь: {hint}")


def grep_cmd(sym, repo):
    rg = shutil.which("rg")
    if rg:
        return [rg, "--no-heading", "-n", "-S", "--", sym, repo]
    return ["grep", "-rnI", "--", sym, repo]


# ── per-tool: locate / callers / callees ────────────────────────────────────
def icode_cost(b, sym, repo):
    tok, sec = 0, 0.0
    for cmd in (
        [b, "query", sym, "--path", repo, "--json"],
        [b, "get-callers", sym, "--path", repo],
        [b, "get-callees", sym, "--path", repo],
    ):
        out, t = run(cmd)
        tok += est(out)
        sec += t
    return tok, round(sec, 3)


def codegraph_cost(b, sym, repo):
    tok, sec = 0, 0.0
    for cmd in (
        [b, "query", sym, "-p", repo, "-j"],
        [b, "callers", sym, "-p", repo, "-j"],
        [b, "callees", sym, "-p", repo, "-j"],
    ):
        out, t = run(cmd)
        tok += est(out)
        sec += t
    return tok, round(sec, 3)


def baseline_cost(sym, repo):
    body, t_grep = run(grep_cmd(sym, repo))
    files, seen = [], set()
    for line in body.splitlines():
        p = line.split(":", 1)[0]
        if p and p not in seen and os.path.isfile(p):
            seen.add(p)
            files.append(p)
        if len(files) >= BASELINE_MAX_FILES:
            break
    total = len(body)
    t0 = time.perf_counter()
    for p in files:
        try:
            with open(p, "rb") as fh:
                total += len(fh.read(BASELINE_MAX_BYTES_PER_FILE))
        except OSError:
            pass
    return (total + 3) // 4, round(t_grep + (time.perf_counter() - t0), 3)


def index_icode(b, repo):
    _, t = run([b, "index", repo])
    return round(t, 2)


def index_codegraph(b, repo):
    if (Path(repo) / ".codegraph").exists():
        _, t = run([b, "index", "."], cwd=repo)
    else:
        _, t = run([b, "init", "-i"], cwd=repo)
    return round(t, 2)


def save(base, new):
    return "n/a" if base <= 0 else f"{(1 - new / base) * 100:+.0f}%"


def main():
    ap = argparse.ArgumentParser(description="baseline vs codegraph vs iCode")
    ap.add_argument("--repo", required=True)
    ap.add_argument("--symbols", required=True)
    ap.add_argument("--icode-bin")
    ap.add_argument("--codegraph-bin")
    args = ap.parse_args()

    repo = str(Path(args.repo).resolve())
    icode = find_bin(args.icode_bin, ["icode"], "icode (--icode-bin)")
    codeg = find_bin(args.codegraph_bin, ["codegraph"], "codegraph (npm i -g @colbymchenry/codegraph)")
    syms = [s.strip() for s in args.symbols.split(",") if s.strip()]

    print(f"[bench3] repo={repo}")
    print(f"[bench3] символы ({len(syms)}): {', '.join(syms)}\n[bench3] индексация ...")
    ti = index_icode(icode, repo)
    tc = index_codegraph(codeg, repo)
    print(f"[bench3] index: iCode={ti}s  codegraph={tc}s\n")

    rows = []
    for s in syms:
        bt, bs = baseline_cost(s, repo)
        ct, cs = codegraph_cost(codeg, s, repo)
        it, is_ = icode_cost(icode, s, repo)
        rows.append((s, bt, ct, it, bs, cs, is_))

    h = f"{'symbol':<24}{'baseline':>10}{'codegraph':>11}{'iCode':>9} | {'cg/base':>8}{'iC/base':>8}{'iC/cg':>7}"
    print(h)
    print("-" * len(h))
    for s, bt, ct, it, *_ in rows:
        print(f"{s:<24}{bt:>10}{ct:>11}{it:>9} | {save(bt,ct):>8}{save(bt,it):>8}{save(ct,it):>7}")
    print("-" * len(h))

    B = sum(r[1] for r in rows); C = sum(r[2] for r in rows); I = sum(r[3] for r in rows)
    print(f"{'ИТОГО токенов':<24}{B:>10}{C:>11}{I:>9} | {save(B,C):>8}{save(B,I):>8}{save(C,I):>7}")
    med = lambda i: int(statistics.median([r[i] for r in rows]))
    print(f"{'медиана/символ':<24}{med(1):>10}{med(2):>11}{med(3):>9}")
    Bt = sum(r[4] for r in rows); Ct = sum(r[5] for r in rows); It = sum(r[6] for r in rows)
    print(f"{'время запросов, с':<24}{Bt:>10.2f}{Ct:>11.2f}{It:>9.2f}")
    print(f"{'индексация, с':<24}{'—':>10}{tc:>11}{ti:>9}")
    print("\nОперации/символ: baseline=1 grep + N чтений; codegraph=3; iCode=3.")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
