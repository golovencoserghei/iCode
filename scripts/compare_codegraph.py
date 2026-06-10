#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""
Head-to-head: iCode vs codegraph на ОДНИХ задачах (retrieval-уровень).

⚠️ Важно о методике. Это НЕ повторение опубликованного бенчмарка codegraph —
тот меряет полную сессию AI-агента (с рассуждениями LLM) на 7 репозиториях.
Здесь — честное прямое сравнение двух инструментов на retrieval-примитивах:
один и тот же репозиторий, один и тот же набор символов, одни и те же три
операции на символ (query / callers / callees), один и тот же подсчёт токенов
(символы/4 по stdout-payload). Это отвечает на вопрос «сколько токенов и
времени стоит один и тот же ответ у каждого инструмента», а не «насколько
дешевле агентская сессия целиком».

Оба CLI почти идентичны:
  iCode      : icode query S --json | icode get-callers S | icode get-callees S
  codegraph  : codegraph query S -j | codegraph callers S -j | codegraph callees S -j

Использование:
    python3 scripts/compare_codegraph.py --repo /path/to/code \
        --symbols full_reindex,handle_index,get_callers

Зависимости: стандартная библиотека Python 3.8+, установленные бинари
`icode` (target/release|debug) и `codegraph` (npm i -g @colbymchenry/codegraph).
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


def est_tokens(s: str) -> int:
    return (len(ANSI.sub("", s)) + 3) // 4


def run(cmd, cwd=None):
    t0 = time.perf_counter()
    try:
        r = subprocess.run(cmd, cwd=cwd, capture_output=True, text=True, timeout=180)
        # Считаем только stdout — это «ответ»; логи/прогресс у обоих идут в stderr.
        out = r.stdout
        ok = r.returncode == 0
    except subprocess.TimeoutExpired:
        out, ok = "<timeout>", False
    return out, time.perf_counter() - t0, ok


def find_icode(explicit):
    if explicit:
        return explicit
    here = Path(__file__).resolve().parent.parent
    for c in (here / "target/release/icode", here / "target/debug/icode"):
        if c.exists():
            return str(c)
    return shutil.which("icode") or sys.exit("icode не найден (--icode-bin)")


def find_codegraph(explicit):
    return explicit or shutil.which("codegraph") or sys.exit(
        "codegraph не найден: npm i -g @colbymchenry/codegraph (или --codegraph-bin)"
    )


# Команды per-tool: (op) -> argv-builder(binary, symbol, repo)
SPECS = {
    "icode": {
        "query":   lambda b, s, p: [b, "query", s, "--path", p, "--json"],
        "callers": lambda b, s, p: [b, "get-callers", s, "--path", p],
        "callees": lambda b, s, p: [b, "get-callees", s, "--path", p],
    },
    "codegraph": {
        "query":   lambda b, s, p: [b, "query", s, "-p", p, "-j"],
        "callers": lambda b, s, p: [b, "callers", s, "-p", p, "-j"],
        "callees": lambda b, s, p: [b, "callees", s, "-p", p, "-j"],
    },
}


def index_icode(binary, repo):
    _, t, _ = run([binary, "index", repo])
    return t


def index_codegraph(binary, repo):
    # codegraph требует init перед index; init -i делает и то и другое.
    if (Path(repo) / ".codegraph").exists():
        _, t, _ = run([binary, "index", "."], cwd=repo)
    else:
        _, t, _ = run([binary, "init", "-i"], cwd=repo)
    return t


def bench(tool, binary, repo, symbols):
    spec = SPECS[tool]
    rows = []
    for s in symbols:
        tok = 0
        secs = 0.0
        for op in ("query", "callers", "callees"):
            out, t, _ = run(spec[op](binary, s, repo))
            tok += est_tokens(out)
            secs += t
        rows.append({"symbol": s, "tokens": tok, "ops": 3, "seconds": round(secs, 3)})
    return rows


def pct(base, new):
    if base <= 0:
        return "n/a"
    return f"{(1 - new / base) * 100:+.0f}%"


def main():
    ap = argparse.ArgumentParser(description="iCode vs codegraph на одних задачах")
    ap.add_argument("--repo", required=True)
    ap.add_argument("--symbols", required=True, help="символы через запятую")
    ap.add_argument("--icode-bin")
    ap.add_argument("--codegraph-bin")
    args = ap.parse_args()

    repo = str(Path(args.repo).resolve())
    icode = find_icode(args.icode_bin)
    codeg = find_codegraph(args.codegraph_bin)
    symbols = [s.strip() for s in args.symbols.split(",") if s.strip()]

    print(f"[cmp] repo      = {repo}")
    print(f"[cmp] icode     = {icode}")
    print(f"[cmp] codegraph = {codeg}")
    print(f"[cmp] символы ({len(symbols)}): {', '.join(symbols)}\n")

    print("[cmp] индексация ...")
    t_ic = index_icode(icode, repo)
    t_cg = index_codegraph(codeg, repo)
    print(f"[cmp] index: iCode={t_ic:.2f}s  codegraph={t_cg:.2f}s\n")

    ic = bench("icode", icode, repo, symbols)
    cg = bench("codegraph", codeg, repo, symbols)

    hdr = f"{'symbol':<24} {'iCode tok':>10} {'codegr tok':>11} {'iCode<cg':>9} {'iCode s':>8} {'codegr s':>9}"
    print(hdr)
    print("-" * len(hdr))
    for a, b in zip(ic, cg):
        print(
            f"{a['symbol']:<24} {a['tokens']:>10} {b['tokens']:>11} "
            f"{pct(b['tokens'], a['tokens']):>9} {a['seconds']:>8.3f} {b['seconds']:>9.3f}"
        )

    ic_tok, cg_tok = sum(r["tokens"] for r in ic), sum(r["tokens"] for r in cg)
    ic_s, cg_s = sum(r["tokens"] for r in ic), sum(r["tokens"] for r in cg)  # noqa
    ic_sec = sum(r["seconds"] for r in ic)
    cg_sec = sum(r["seconds"] for r in cg)
    print("-" * len(hdr))
    print(f"ИТОГО токенов:  iCode={ic_tok}  codegraph={cg_tok}  (iCode vs codegraph: {pct(cg_tok, ic_tok)})")
    print(f"Медиана ток/симв: iCode={int(statistics.median([r['tokens'] for r in ic]))}  "
          f"codegraph={int(statistics.median([r['tokens'] for r in cg]))}")
    print(f"ИТОГО время запросов: iCode={ic_sec:.2f}s  codegraph={cg_sec:.2f}s")
    print(f"Индексация: iCode={t_ic:.2f}s  codegraph={t_cg:.2f}s")
    print("\nОперации идентичны (по 3 на символ у обоих). Оси сравнения — токены и время.")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
