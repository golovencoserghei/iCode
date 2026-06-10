#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""
Воспроизводимый бенчмарк iCode vs «grep + чтение файлов».

Зачем: как и codegraph, мы хотим измеримо показать выигрыш индекса —
сколько токенов и операций экономит навигационный запрос через iCode по
сравнению с наивным подходом AI-агента (grep по дереву + чтение файлов,
в которых нашлось совпадение).

Что меряется на каждый символ (задача «понять символ X: где определён,
кто вызывает»):
  * iCode    : `icode query X --json` + `icode get-callers X --json`
               (2 быстрые команды, открывают .icode/index.db напрямую —
                демон не нужен).
  * baseline : `rg X <repo>` (или `grep -rn`) + чтение файлов, где нашлось
               совпадение (с ограничениями — агент читает целые файлы,
               чтобы разобраться). Это то, что делает модель без iCode.

Метрики: оценка токенов (≈ символы/4), wall-time, число операций.
Печатает таблицу по символам + агрегат (медиана/сумма) и опционально JSON.

Использование:
    python3 scripts/benchmark.py --repo /path/to/project
    python3 scripts/benchmark.py --repo . --symbols UserService,handle,index
    python3 scripts/benchmark.py --repo . --icode-bin target/release/icode --json out.json

Зависимости: только стандартная библиотека Python 3.8+. Желателен `ripgrep`
(rg) — иначе используется `grep -rn`. Бинарь `icode` ищется автоматически в
target/release и target/debug, либо задаётся через --icode-bin.
"""

import argparse
import json
import os
import re
import shutil
import statistics
import subprocess
import sys
import time
from pathlib import Path

# Эвристическая оценка токенов: ~4 символа на токен (как у большинства BPE-токенайзеров).
def est_tokens(text: str) -> int:
    return (len(text) + 3) // 4


# Ограничения baseline-чтения: имитируем разумного агента, а не «прочитать весь репо».
BASELINE_MAX_FILES = 20          # сколько файлов агент откроет, найдя совпадения
BASELINE_MAX_BYTES_PER_FILE = 64 * 1024  # cap на файл (агент не читает мегабайтные дампы целиком)

# Паттерны определений для авто-выбора символов, если --symbols не задан.
DEF_PATTERNS = [
    re.compile(r"\bfunction\s+([A-Za-z_]\w+)\s*\("),   # php / js
    re.compile(r"\bdef\s+([A-Za-z_]\w+)\s*\("),         # python
    re.compile(r"\bfn\s+([A-Za-z_]\w+)\s*[<(]"),        # rust
    re.compile(r"\bfunc\s+([A-Za-z_]\w+)\s*\("),        # go
    re.compile(r"\bclass\s+([A-Za-z_]\w+)"),            # oop
]


def find_icode_bin(explicit: str | None) -> str:
    if explicit:
        if not Path(explicit).exists():
            sys.exit(f"icode не найден: {explicit}")
        return explicit
    here = Path(__file__).resolve().parent.parent
    for cand in (here / "target/release/icode", here / "target/debug/icode"):
        if cand.exists():
            return str(cand)
    found = shutil.which("icode")
    if found:
        return found
    sys.exit("Не найден бинарь icode. Соберите `cargo build --release -p icode` или укажите --icode-bin.")


def grep_cmd(symbol: str, repo: str) -> list[str]:
    rg = shutil.which("rg")
    if rg:
        # --no-heading -n : path:line:text; -S smart-case; ограничим бинарники.
        return [rg, "--no-heading", "-n", "-S", "--", symbol, repo]
    return ["grep", "-rnI", "--", symbol, repo]


def run(cmd: list[str], cwd: str | None = None) -> tuple[str, float]:
    t0 = time.perf_counter()
    try:
        out = subprocess.run(
            cmd, cwd=cwd, capture_output=True, text=True, timeout=120
        )
        body = out.stdout + out.stderr
    except subprocess.TimeoutExpired:
        body = "<timeout>"
    return body, time.perf_counter() - t0


def discover_symbols(repo: str, n: int) -> list[str]:
    """Насэмплировать до n имён определений, прогнав grep по дереву."""
    names: list[str] = []
    seen: set[str] = set()
    grep = shutil.which("rg")
    # Берём небольшой срез текста репо и вытаскиваем имена определений.
    if grep:
        cmd = [grep, "--no-heading", "-N", "-S", "-e", r"\b(function|def|fn|func|class)\s+\w+", repo]
    else:
        cmd = ["grep", "-rnI", "-E", r"(function|def|fn|func|class)[[:space:]]+[A-Za-z_]", repo]
    body, _ = run(cmd)
    for line in body.splitlines():
        for pat in DEF_PATTERNS:
            m = pat.search(line)
            if m:
                name = m.group(1)
                # Пропускаем слишком общие/короткие и магические.
                if len(name) < 4 or name.startswith("__"):
                    continue
                if name not in seen:
                    seen.add(name)
                    names.append(name)
        if len(names) >= n:
            break
    return names[:n]


def baseline_cost(symbol: str, repo: str) -> dict:
    """grep по дереву + чтение файлов с совпадениями (имитация агента без iCode)."""
    body, t_grep = run(grep_cmd(symbol, repo))
    files: list[str] = []
    seen: set[str] = set()
    for line in body.splitlines():
        # rg/grep: path:line:text
        parts = line.split(":", 1)
        p = parts[0]
        if p and p not in seen and os.path.isfile(p):
            seen.add(p)
            files.append(p)
        if len(files) >= BASELINE_MAX_FILES:
            break

    total = len(body)
    t0 = time.perf_counter()
    reads = 0
    for p in files:
        try:
            with open(p, "rb") as fh:
                chunk = fh.read(BASELINE_MAX_BYTES_PER_FILE)
            total += len(chunk)
            reads += 1
        except OSError:
            pass
    t_read = time.perf_counter() - t0

    return {
        "tokens": est_tokens_bytes(total),
        "ops": 1 + reads,                  # 1 grep + N чтений файлов
        "seconds": round(t_grep + t_read, 4),
        "files_read": reads,
    }


def est_tokens_bytes(nbytes: int) -> int:
    return (nbytes + 3) // 4


def icode_cost(icode: str, symbol: str, repo: str) -> dict:
    """iCode: query (find_symbol) + get-callers — обе команды читают индекс напрямую."""
    body1, t1 = run([icode, "query", symbol, "--path", repo, "--json"])
    body2, t2 = run([icode, "get-callers", symbol, "--path", repo, "--json"])
    blob = body1 + body2
    return {
        "tokens": est_tokens(blob),
        "ops": 2,                          # 2 точечных запроса к индексу
        "seconds": round(t1 + t2, 4),
    }


def pct(base: float, new: float) -> str:
    if base <= 0:
        return "n/a"
    return f"{(1 - new / base) * 100:+.0f}%"


def main() -> int:
    ap = argparse.ArgumentParser(description="Бенчмарк iCode vs grep+read")
    ap.add_argument("--repo", required=True, help="Путь к индексируемому проекту")
    ap.add_argument("--symbols", help="Список символов через запятую (иначе авто-выбор)")
    ap.add_argument("--count", type=int, default=8, help="Сколько символов при авто-выборе (default 8)")
    ap.add_argument("--icode-bin", help="Путь к бинарю icode (иначе авто)")
    ap.add_argument("--reindex", action="store_true", help="Принудительно переиндексировать перед прогоном")
    ap.add_argument("--json", help="Записать сырые результаты в JSON-файл")
    args = ap.parse_args()

    repo = str(Path(args.repo).resolve())
    if not os.path.isdir(repo):
        sys.exit(f"Не директория: {repo}")
    icode = find_icode_bin(args.icode_bin)

    # Индексация (idempotent: пропустит неизменённые файлы).
    print(f"[bench] icode = {icode}")
    print(f"[bench] repo  = {repo}")
    print("[bench] индексация ...")
    idx_cmd = [icode, "index", repo] + (["--force"] if args.reindex else [])
    idx_body, idx_t = run(idx_cmd)
    print(f"[bench] индекс готов за {idx_t:.2f}s")

    if args.symbols:
        symbols = [s.strip() for s in args.symbols.split(",") if s.strip()]
    else:
        print("[bench] авто-выбор символов ...")
        symbols = discover_symbols(repo, args.count)
    if not symbols:
        sys.exit("Не удалось определить символы. Задайте --symbols.")
    print(f"[bench] символы ({len(symbols)}): {', '.join(symbols)}\n")

    rows = []
    for sym in symbols:
        ic = icode_cost(icode, sym, repo)
        bl = baseline_cost(sym, repo)
        rows.append({"symbol": sym, "icode": ic, "baseline": bl})

    # Таблица.
    hdr = f"{'symbol':<24} {'iCode tok':>10} {'base tok':>10} {'tok save':>9} {'iCode ops':>10} {'base ops':>9}"
    print(hdr)
    print("-" * len(hdr))
    for r in rows:
        print(
            f"{r['symbol']:<24} "
            f"{r['icode']['tokens']:>10} {r['baseline']['tokens']:>10} "
            f"{pct(r['baseline']['tokens'], r['icode']['tokens']):>9} "
            f"{r['icode']['ops']:>10} {r['baseline']['ops']:>9}"
        )

    # Агрегат.
    ic_tok = [r["icode"]["tokens"] for r in rows]
    bl_tok = [r["baseline"]["tokens"] for r in rows]
    ic_ops = [r["icode"]["ops"] for r in rows]
    bl_ops = [r["baseline"]["ops"] for r in rows]
    print("-" * len(hdr))
    print(f"ИТОГО токенов:  iCode={sum(ic_tok)}  baseline={sum(bl_tok)}  экономия={pct(sum(bl_tok), sum(ic_tok))}")
    print(f"Медиана токенов/символ: iCode={int(statistics.median(ic_tok))}  baseline={int(statistics.median(bl_tok))}")
    print(f"ИТОГО операций: iCode={sum(ic_ops)}  baseline={sum(bl_ops)}  экономия={pct(sum(bl_ops), sum(ic_ops))}")

    if args.json:
        Path(args.json).write_text(
            json.dumps(
                {
                    "repo": repo,
                    "icode_bin": icode,
                    "rows": rows,
                    "totals": {
                        "icode_tokens": sum(ic_tok),
                        "baseline_tokens": sum(bl_tok),
                        "icode_ops": sum(ic_ops),
                        "baseline_ops": sum(bl_ops),
                    },
                },
                ensure_ascii=False,
                indent=2,
            ),
            encoding="utf-8",
        )
        print(f"\n[bench] JSON записан в {args.json}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
