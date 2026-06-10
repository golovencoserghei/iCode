#!/usr/bin/env python3
"""
iCode quality harness — меряет КОРРЕКТНОСТЬ ответов инструмента против известного
ground truth, а не токены/скорость (прокси).

Строит контролируемую фикстуру (PHP + Python) с заранее известными связями,
индексирует её настоящим бинарём iCode и проверяет батарею утверждений по
каждой способности: routing, inheritance, резолв вызовов по типу (typed-свойства,
self), find_existing («это уже есть»), find_unreachable (мёртвый кластер).

Это НИЖНЯЯ ГРАНИЦА качества: контролируемый фикстур, а не полная агентская сессия.
Но в отличие от token-бенчмарка — проверяет, что ответ ВЕРНЫЙ. Расширяйте реальными
репозиториями, добавляя задачи с эталоном.

Запуск:  python3 scripts/quality_harness.py [--bin path/to/icode]
Выход 0 — все задачи прошли; иначе число провалов.
"""
import argparse
import json
import os
import shutil
import sqlite3
import subprocess
import sys
import tempfile

PHP = {
    "routes/web.php": """<?php
Route::get('/users', [UserController::class, 'index']);
Route::resource('posts', PostController::class);
Route::prefix('admin')->group(function () {
    Route::get('/stats', [AdminController::class, 'stats']);
});
""",
    "app/BaseController.php": "<?php\nclass BaseController {}\n",
    "app/UserController.php": """<?php
class UserController extends BaseController {
    private UserRepo $repo;
    public function index() { $this->repo->all(); }
}
""",
    "app/UserRepo.php": "<?php\nclass UserRepo {\n public function all() {}\n}\n",
    "app/Dead.php": """<?php
class Dead {
    public function orphan() { $this->helper(); }
    public function helper() {}
}
""",
}
PY = {
    "svc.py": """class Account:
    def save(self):
        pass
    def run(self):
        self.save()
""",
}


def build_fixture(root):
    for rel, content in {**PHP, **PY}.items():
        p = os.path.join(root, rel)
        os.makedirs(os.path.dirname(p), exist_ok=True)
        with open(p, "w") as f:
            f.write(content)


def find_bin(explicit):
    if explicit:
        return explicit
    here = os.path.dirname(os.path.abspath(__file__))
    for cand in ("target/release/icode", "target/debug/icode"):
        p = os.path.join(here, "..", cand)
        if os.path.exists(p):
            return os.path.abspath(p)
    return shutil.which("icode") or "icode"


def run(bin_path, *args, root=None):
    out = subprocess.run([bin_path, *args], capture_output=True, text=True, cwd=root)
    return out.stdout, out.stderr, out.returncode


class Score:
    def __init__(self):
        self.passed = 0
        self.failed = 0
        self.rows = []

    def check(self, capability, desc, ok, detail=""):
        self.rows.append((capability, desc, ok, detail))
        if ok:
            self.passed += 1
        else:
            self.failed += 1

    def report(self):
        by_cap = {}
        for cap, _desc, ok, _d in self.rows:
            p, t = by_cap.get(cap, (0, 0))
            by_cap[cap] = (p + (1 if ok else 0), t + 1)
        print("\n=== iCode Quality Scorecard (корректность против ground truth) ===")
        for cap, (p, t) in sorted(by_cap.items()):
            mark = "OK " if p == t else "!! "
            print(f"  {mark}{cap:28} {p}/{t}")
        for cap, desc, ok, detail in self.rows:
            if not ok:
                print(f"   FAIL [{cap}] {desc}  {detail}")
        total = self.passed + self.failed
        pct = (self.passed * 100 // total) if total else 0
        print(f"\n  ИТОГО: {self.passed}/{total} ({pct}%)")
        return self.failed


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--bin", default=None)
    args = ap.parse_args()
    bin_path = find_bin(args.bin)

    root = tempfile.mkdtemp(prefix="icode_qh_")
    try:
        build_fixture(root)
        _, err, rc = run(bin_path, "index", root)
        if rc != 0:
            print(f"index failed: {err}", file=sys.stderr)
            return 2
        db = sqlite3.connect(os.path.join(root, ".icode", "index.db"))
        s = Score()

        # --- routing ---
        routes = {(m, p): h for m, p, h in db.execute(
            "SELECT method, path, COALESCE(handler_class,'')||'@'||COALESCE(handler_method,'') FROM routes")}
        s.check("routing", "GET /users -> UserController@index",
                routes.get(("GET", "/users")) == "UserController@index", str(routes.get(("GET", "/users"))))
        s.check("routing", "Route::resource posts -> 7 экшенов",
                sum(1 for (_m, p) in routes if p.startswith("/posts")) == 7,
                f"{sum(1 for (_m,p) in routes if p.startswith('/posts'))}")
        s.check("routing", "группа prefix: GET /admin/stats",
                ("GET", "/admin/stats") in routes, str([p for _m, p in routes]))

        # --- inheritance ---
        bases = dict(db.execute("SELECT name, COALESCE(bases,'') FROM classes"))
        s.check("inheritance", "UserController extends BaseController",
                "BaseController" in bases.get("UserController", ""), bases.get("UserController"))

        # --- резолв вызовов по типу (PHP typed-свойство) ---
        recv = {c: r for c, r in db.execute("SELECT callee, receiver FROM calls WHERE callee='all'")}
        s.check("call-resolution", "$this->repo->all() резолвится к UserRepo (typed-свойство)",
                recv.get("all") == "UserRepo", str(recv.get("all")))

        # --- резолв self (Python) ---
        pyrecv = {c: r for c, r in db.execute("SELECT callee, receiver FROM calls WHERE callee='save'")}
        s.check("call-resolution", "Python self.save() receiver=self",
                pyrecv.get("save") == "self", str(pyrecv.get("save")))

        # --- find_existing ---
        out, _e, _rc = run(bin_path, "find-existing", "all records", "--path", root, "--kind", "function")
        try:
            matches = json.loads(out)
        except Exception:
            matches = []
        s.check("find-existing", "'all records' находит UserRepo::all",
                any(m.get("name") == "all" for m in matches), f"{[m.get('name') for m in matches][:3]}")

        # --- find_unreachable (мёртвый кластер) ---
        out, _e, _rc = run(bin_path, "unreachable", "--path", root, "--language", "php", "--limit", "50")
        try:
            unreach = {m.get("name") for m in json.loads(out)}
        except Exception:
            unreach = set()
        s.check("unreachable", "orphan (не вызывается) недостижим", "orphan" in unreach, str(sorted(unreach)))
        s.check("unreachable", "helper (только из orphan) — мёртвый кластер", "helper" in unreach, str(sorted(unreach)))
        s.check("unreachable", "index (роут-хендлер) НЕ помечен мёртвым", "index" not in unreach, str(sorted(unreach)))

        failed = s.report()
        return failed
    finally:
        shutil.rmtree(root, ignore_errors=True)


if __name__ == "__main__":
    sys.exit(main())
