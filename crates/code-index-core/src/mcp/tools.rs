// Реализации MCP-инструментов (v0.5+): read-only, с проверкой статуса папки у демона.
//
// Multi-repo: каждая функция принимает `&RepoEntry` (конкретный репозиторий, выбранный
// через `resolve_repo` в mod.rs по параметру `repo`). Диагностические инструменты
// `get_stats` и `health` принимают весь `&CodeIndexServer`, чтобы собрать сводку
// по всем подключённым репо.
//
// Перед каждым data-tool функция спрашивает у демона статус `root_path` этого репо.
// Если папка не `Ready` — возвращается `ToolUnavailable` JSON, и реальный запрос
// к БД не выполняется.

use super::{CodeIndexServer, RepoEntry};
use crate::daemon_core::client;
use crate::daemon_core::ipc::{PathStatus, ToolUnavailable};
use crate::storage::models::{ClassHit, ClassRecord, FunctionHit, FunctionRecord};

/// Soft-cap: число строк в одном `read_file` (по умолчанию).
pub(crate) const READ_FILE_SOFT_CAP_LINES: usize = 5_000;
/// Soft-cap: размер ответа `read_file` в байтах (по умолчанию).
pub(crate) const READ_FILE_SOFT_CAP_BYTES: usize = 500 * 1024;
/// Hard-cap: абсолютный максимум для `read_file`, даже с line_start/line_end.
pub(crate) const READ_FILE_HARD_CAP_BYTES: usize = 2 * 1024 * 1024;
/// Hard-cap: суммарный размер ответа grep_text/grep_body.
pub(crate) const GREP_TOTAL_BYTES_CAP: usize = 1 * 1024 * 1024;
/// Default-limit grep_text если path_glob и language не заданы.
pub(crate) const GREP_TEXT_FULL_SCAN_DEFAULT_LIMIT: usize = 100;
/// Лимит байт на тело одной функции/класса в get_file_summary.
/// Предотвращает взрыв токенов на файлах 100k+ символов (ProjectInstallService.php и подобные).
pub(crate) const GET_FILE_SUMMARY_BODY_CAP: usize = 2_000;

/// Сериализовать `ToolUnavailable` в JSON-строку.
pub fn format_unavailable(value: ToolUnavailable) -> String {
    match serde_json::to_string_pretty(&value) {
        Ok(s) => s,
        Err(e) => format!("{{\"status\":\"error\",\"message\":\"Сериализация: {}\"}}", e),
    }
}

/// Проверить у демона статус папки репо. `None` — папка Ready, можно продолжать.
/// `Some(json)` — нужно отдать клиенту этот ToolUnavailable-ответ вместо данных.
pub async fn check_path_status(entry: &RepoEntry) -> Option<String> {
    let Some(root) = entry.root_path.as_deref() else {
        return Some(format_unavailable(ToolUnavailable::Error {
            message: format!(
                "Dispatcher bug: remote repo (ip={}) reached a local tool handler.",
                entry.ip
            ),
        }));
    };
    match client::path_status_async(root).await {
        Ok(resp) => match resp.status {
            PathStatus::Ready => None,
            PathStatus::InitialIndexing | PathStatus::ReindexingBatch => Some(format_unavailable(
                ToolUnavailable::Indexing {
                    progress: resp.progress.unwrap_or_default(),
                    message: match resp.status {
                        PathStatus::InitialIndexing => "Первичная индексация в процессе".into(),
                        _ => "Применяется батч изменений".into(),
                    },
                },
            )),
            PathStatus::NotStarted => Some(format_unavailable(ToolUnavailable::NotStarted {
                message: format!(
                    "Путь {} не отслеживается демоном. Добавьте его в daemon.toml и вызовите 'code-index daemon reload'.",
                    root.display()
                ),
            })),
            PathStatus::Error => Some(format_unavailable(ToolUnavailable::Error {
                message: resp
                    .error
                    .unwrap_or_else(|| "Неизвестная ошибка индексации".into()),
            })),
        },
        Err(e) => Some(format_unavailable(ToolUnavailable::DaemonOffline {
            message: format!(
                "Демон code-index не доступен ({}). Запустите 'code-index daemon run' или Scheduled Task / systemd user unit.",
                e
            ),
        })),
    }
}

/// Макрос-хелпер: если папка не Ready — вернуть unavailable JSON немедленно.
macro_rules! bail_if_not_ready {
    ($entry:expr) => {{
        if let Some(json) = crate::mcp::tools::check_path_status($entry).await {
            return json;
        }
    }};
}

fn to_json<T: serde::Serialize>(value: &T) -> String {
    match serde_json::to_string_pretty(value) {
        Ok(s) => s,
        Err(e) => format!("{{\"error\": \"Сериализация: {}\"}}", e),
    }
}

// ── Event-based invalidation helpers (Phase 2) ──────────────────────────────

/// Завернуть результат tool'а в `{result, _meta: {dependent_files: [...]}}`.
///
/// Целевой потребитель — `mcp-cache-ci`: при cache-fill он парсит payload и
/// регистрирует связи `cache_key → file_path` в `reverse_index`. По
/// последующему `POST /invalidate {file_paths: [...]}` от daemon после
/// `transaction.commit()` SQLite (этап 3) cache-ci мгновенно сносит ровно те
/// entries, что зависят от изменённых файлов — не задевая соседних.
///
/// `dependent_files` пустой → entry попадёт в кэш без file-зависимостей и будет
/// чиститься только по TTL (как раньше). Это нормально для tools без явной
/// привязки к файлам (часть BSL-инструментов).
///
/// Дубликаты в `dependent_files` дедуплицируются (HashSet → Vec, без гарантии
/// порядка — cache-ci порядок не использует).
pub(crate) fn wrap_with_meta<T: serde::Serialize>(
    result: &T,
    dependent_files: Vec<String>,
) -> String {
    use std::collections::HashSet;
    let deps: Vec<String> = dependent_files
        .into_iter()
        .filter(|p| !p.is_empty())
        .collect::<HashSet<_>>()
        .into_iter()
        .collect();
    let result_value = match serde_json::to_value(result) {
        Ok(v) => v,
        Err(e) => return format!("{{\"error\": \"Сериализация result: {}\"}}", e),
    };
    let wrapped = serde_json::json!({
        "result": result_value,
        "_meta": { "dependent_files": deps },
    });
    serde_json::to_string_pretty(&wrapped)
        .unwrap_or_else(|e| format!("{{\"error\": \"Сериализация wrap: {}\"}}", e))
}

/// Аналог `wrap_with_meta`, но добавляет `_meta.note` — произвольная строка-метка.
/// Используется для fuzzy_fallback и других нестандартных путей.
pub(crate) fn wrap_with_meta_note<T: serde::Serialize>(
    result: &T,
    dependent_files: Vec<String>,
    note: &str,
) -> String {
    use std::collections::HashSet;
    let deps: Vec<String> = dependent_files
        .into_iter()
        .filter(|p| !p.is_empty())
        .collect::<HashSet<_>>()
        .into_iter()
        .collect();
    let result_value = match serde_json::to_value(result) {
        Ok(v) => v,
        Err(e) => return format!("{{\"error\": \"Сериализация result: {}\"}}", e),
    };
    let wrapped = serde_json::json!({
        "result": result_value,
        "_meta": { "dependent_files": deps, "note": note },
    });
    serde_json::to_string_pretty(&wrapped)
        .unwrap_or_else(|e| format!("{{\"error\": \"Сериализация wrap: {}\"}}", e))
}

// ── Freshness / staleness (connect-time reconciliation, v0.11+) ──────────────

/// Сверить файлы из `deps` с рабочим деревом на диске и вернуть те, что
/// устарели в индексе (изменился размер/mtime либо файл удалён с диска).
///
/// Идея заимствована из codegraph: даже когда демон в статусе `Ready`,
/// конкретный файл мог измениться в окне debounce до того, как watcher
/// переиндексировал его. Сверка `(size, mtime)` ловит это без IPC к демону —
/// MCP-сервер сам знает индексные метаданные и сам читает диск.
///
/// `root=None` (remote-репо) или отсутствие метаданных → файл не помечается
/// (нет данных для сравнения — не шумим).
pub(crate) fn compute_stale(
    storage: &tokio::sync::MutexGuard<'_, crate::storage::Storage>,
    root: Option<&std::path::Path>,
    deps: &[String],
) -> Vec<String> {
    use std::time::UNIX_EPOCH;
    let Some(root) = root else { return Vec::new() };
    let mut stale = Vec::new();
    for rel in deps {
        if rel.is_empty() {
            continue;
        }
        let Ok(Some(rec)) = storage.get_file_by_path(rel) else { continue };
        match std::fs::metadata(root.join(rel)) {
            Ok(m) => {
                let disk_size = m.len() as i64;
                let disk_mtime = m
                    .modified()
                    .ok()
                    .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                    .map(|d| d.as_secs() as i64);
                let size_mismatch = rec.file_size.map(|s| s != disk_size).unwrap_or(false);
                let mtime_mismatch = match (rec.mtime, disk_mtime) {
                    (Some(a), Some(b)) => a != b,
                    _ => false,
                };
                if size_mismatch || mtime_mismatch {
                    stale.push(rel.clone());
                }
            }
            // Файл есть в индексе, но удалён с диска — данные устарели.
            Err(_) => stale.push(rel.clone()),
        }
    }
    stale
}

/// Как `wrap_with_meta`, но добавляет `_meta.stale` + предупреждающий баннер,
/// когда часть `dependent_files` устарела относительно диска. Используется
/// инструментами, по результатам которых агент действует (read_file,
/// get_function/get_class, get_file_summary/outline, get_symbol_context,
/// find_routes). При пустом `stale` поведение идентично `wrap_with_meta`.
pub(crate) fn wrap_with_meta_fresh<T: serde::Serialize>(
    result: &T,
    dependent_files: Vec<String>,
    stale: Vec<String>,
) -> String {
    use std::collections::HashSet;
    let deps: Vec<String> = dependent_files
        .into_iter()
        .filter(|p| !p.is_empty())
        .collect::<HashSet<_>>()
        .into_iter()
        .collect();
    let result_value = match serde_json::to_value(result) {
        Ok(v) => v,
        Err(e) => return format!("{{\"error\": \"Сериализация result: {}\"}}", e),
    };
    let mut meta = serde_json::json!({ "dependent_files": deps });
    if !stale.is_empty() {
        let stale_dedup: Vec<String> = stale
            .into_iter()
            .collect::<HashSet<_>>()
            .into_iter()
            .collect();
        meta["stale"] = serde_json::json!(stale_dedup);
        meta["stale_warning"] = serde_json::json!(
            "⚠️ Эти файлы изменились на диске после индексации — данные могут быть устаревшими. \
             Прочитайте их напрямую с диска для актуального содержимого."
        );
    }
    let wrapped = serde_json::json!({ "result": result_value, "_meta": meta });
    serde_json::to_string_pretty(&wrapped)
        .unwrap_or_else(|e| format!("{{\"error\": \"Сериализация wrap: {}\"}}", e))
}

/// Собрать `dependent_files` из vec'а записей через extractor file_id.
/// Применяется к Vec<FunctionRecord>, Vec<ClassRecord>, Vec<CallRecord> и т.п.
/// Дубликаты не нужно дедуплицировать здесь — `wrap_with_meta` сам сделает.
pub(crate) fn collect_paths_via<R>(
    storage: &tokio::sync::MutexGuard<'_, crate::storage::Storage>,
    records: &[R],
    extract: impl Fn(&R) -> i64,
) -> Vec<String> {
    records
        .iter()
        .map(|r| lookup_path(storage, extract(r)))
        .filter(|p| !p.is_empty())
        .collect()
}

// ── Phase 1 helpers ─────────────────────────────────────────────────────────

/// Скомпилировать glob → matcher через `globset`. Применяется к результатам
/// после SQL-выборки в search_*/get_*. Использует `storage::normalize_glob`
/// для приведения `**` к `*` (см. SQLite GLOB-семантику).
pub(crate) fn build_path_matcher(glob: &str) -> Result<globset::GlobMatcher, String> {
    let normalized = crate::storage::normalize_glob(glob);
    globset::Glob::new(&normalized)
        .map(|g| g.compile_matcher())
        .map_err(|e| format!("невалидный glob '{}': {}", glob, e))
}

/// Lookup пути по file_id через storage. Любая ошибка/отсутствие → пустая строка
/// (она не пройдёт ни один matcher, так что результат честно отбросится).
/// Storage уже заблокирован вызывающей стороной (передаётся через `&MutexGuard`).
pub(crate) fn lookup_path(
    storage: &tokio::sync::MutexGuard<'_, crate::storage::Storage>,
    file_id: i64,
) -> String {
    storage
        .get_path_by_file_id(file_id)
        .ok()
        .flatten()
        .unwrap_or_default()
}

pub(crate) fn matches_with(matcher: &globset::GlobMatcher, path: &str) -> bool {
    if path.is_empty() {
        return false;
    }
    matcher.is_match(path)
}

// ── Lean-проекции (token-efficiency, v0.11+) ────────────────────────────────
//
// По умолчанию поиск (search_function/search_class/find_symbol) отдаёт сигнатуру +
// строки + путь БЕЗ тел функций — этого хватает чтобы локализовать символ, а тело
// тянется точечно через get_function/read_file. `include_body=true` возвращает
// полные записи (старое поведение). На «толстых» функциях это экономит 5–10×.

/// Завернуть функции: lean (FunctionHit) по умолчанию, full (FunctionRecord) при include_body.
fn emit_functions(
    storage: &tokio::sync::MutexGuard<'_, crate::storage::Storage>,
    records: Vec<FunctionRecord>,
    deps: Vec<String>,
    include_body: bool,
    note: Option<&str>,
) -> String {
    if include_body {
        match note {
            Some(n) => wrap_with_meta_note(&records, deps, n),
            None => wrap_with_meta(&records, deps),
        }
    } else {
        let hits: Vec<FunctionHit> = records
            .iter()
            .map(|f| FunctionHit::from_record(f, lookup_path(storage, f.file_id)))
            .collect();
        match note {
            Some(n) => wrap_with_meta_note(&hits, deps, n),
            None => wrap_with_meta(&hits, deps),
        }
    }
}

/// Завернуть классы: lean (ClassHit) по умолчанию, full (ClassRecord) при include_body.
fn emit_classes(
    storage: &tokio::sync::MutexGuard<'_, crate::storage::Storage>,
    records: Vec<ClassRecord>,
    deps: Vec<String>,
    include_body: bool,
) -> String {
    if include_body {
        wrap_with_meta(&records, deps)
    } else {
        let hits: Vec<ClassHit> = records
            .iter()
            .map(|c| ClassHit::from_record(c, lookup_path(storage, c.file_id)))
            .collect();
        wrap_with_meta(&hits, deps)
    }
}

// ── Реализации инструментов ─────────────────────────────────────────────────

pub async fn search_function(
    entry: &RepoEntry,
    query: String,
    limit: Option<usize>,
    language: Option<String>,
    path_glob: Option<String>,
    include_body: Option<bool>,
) -> String {
    bail_if_not_ready!(entry);
    let storage = entry.local_storage().lock().await;
    let want = limit.unwrap_or(20);
    let full = include_body.unwrap_or(false);
    // Если path_glob задан — берём с запасом (5×, до 500), потом фильтруем по пути,
    // потом обрезаем до want. Это компромисс между точностью и нагрузкой.
    let sql_limit = if path_glob.is_some() {
        (want.saturating_mul(5)).min(500)
    } else {
        want
    };
    match storage.search_functions(&query, sql_limit, language.as_deref()) {
        Ok(mut r) => {
            if let Some(ref g) = path_glob {
                let matcher = match build_path_matcher(g) {
                    Ok(m) => m,
                    Err(e) => return format!("{{\"error\": \"path_glob: {}\"}}", e),
                };
                r.retain(|fr| matches_with(&matcher, &lookup_path(&storage, fr.file_id)));
                r.truncate(want);
            }
            if r.is_empty() {
                // FTS вернул 0 — пробуем fuzzy по токенам camelCase/snake_case
                if let Ok(fuzzy) = storage.search_functions_fuzzy(&query, want) {
                    if !fuzzy.is_empty() {
                        let deps = collect_paths_via(&storage, &fuzzy, |fr| fr.file_id);
                        return emit_functions(&storage, fuzzy, deps, full, Some("fuzzy_fallback"));
                    }
                }
            }
            let deps = collect_paths_via(&storage, &r, |fr| fr.file_id);
            emit_functions(&storage, r, deps, full, None)
        }
        Err(e) => format!("{{\"error\": \"search_function: {}\"}}", e),
    }
}

pub async fn search_class(
    entry: &RepoEntry,
    query: String,
    limit: Option<usize>,
    language: Option<String>,
    path_glob: Option<String>,
    include_body: Option<bool>,
) -> String {
    bail_if_not_ready!(entry);
    let storage = entry.local_storage().lock().await;
    let want = limit.unwrap_or(20);
    let full = include_body.unwrap_or(false);
    let sql_limit = if path_glob.is_some() {
        (want.saturating_mul(5)).min(500)
    } else {
        want
    };
    match storage.search_classes(&query, sql_limit, language.as_deref()) {
        Ok(mut r) => {
            if let Some(ref g) = path_glob {
                let matcher = match build_path_matcher(g) {
                    Ok(m) => m,
                    Err(e) => return format!("{{\"error\": \"path_glob: {}\"}}", e),
                };
                r.retain(|cr| matches_with(&matcher, &lookup_path(&storage, cr.file_id)));
                r.truncate(want);
            }
            let deps = collect_paths_via(&storage, &r, |cr| cr.file_id);
            emit_classes(&storage, r, deps, full)
        }
        Err(e) => format!("{{\"error\": \"search_class: {}\"}}", e),
    }
}

pub async fn get_function(
    entry: &RepoEntry,
    name: String,
    path_glob: Option<String>,
) -> String {
    bail_if_not_ready!(entry);
    let storage = entry.local_storage().lock().await;
    match storage.get_function_by_name(&name) {
        Ok(mut r) => {
            if let Some(ref g) = path_glob {
                let matcher = match build_path_matcher(g) {
                    Ok(m) => m,
                    Err(e) => return format!("{{\"error\": \"path_glob: {}\"}}", e),
                };
                r.retain(|fr| matches_with(&matcher, &lookup_path(&storage, fr.file_id)));
            }
            let deps = collect_paths_via(&storage, &r, |fr| fr.file_id);
            let stale = compute_stale(&storage, entry.root_path.as_deref(), &deps);
            wrap_with_meta_fresh(&r, deps, stale)
        }
        Err(e) => format!("{{\"error\": \"get_function: {}\"}}", e),
    }
}

pub async fn get_class(
    entry: &RepoEntry,
    name: String,
    path_glob: Option<String>,
) -> String {
    bail_if_not_ready!(entry);
    let storage = entry.local_storage().lock().await;
    match storage.get_class_by_name(&name) {
        Ok(mut r) => {
            if let Some(ref g) = path_glob {
                let matcher = match build_path_matcher(g) {
                    Ok(m) => m,
                    Err(e) => return format!("{{\"error\": \"path_glob: {}\"}}", e),
                };
                r.retain(|cr| matches_with(&matcher, &lookup_path(&storage, cr.file_id)));
            }
            let deps = collect_paths_via(&storage, &r, |cr| cr.file_id);
            let stale = compute_stale(&storage, entry.root_path.as_deref(), &deps);
            wrap_with_meta_fresh(&r, deps, stale)
        }
        Err(e) => format!("{{\"error\": \"get_class: {}\"}}", e),
    }
}

/// Дефолтный кап рёбер call-графа — без него «толстая» функция (десятки call-site)
/// раздувает ответ. 50 хватает для навигации; полнее — get_callers_tree / language-фильтр.
const CALL_EDGES_DEFAULT_LIMIT: usize = 50;

/// Общая обёртка для callers/callees с ООП-резолвом: кап, provenance-нота,
/// предупреждение о коллизии имени (когда class не задан).
fn emit_resolved_calls(
    resolved: anyhow::Result<(Vec<crate::storage::models::ResolvedCall>, usize, bool)>,
    cap: usize,
    class_given: bool,
    name: &str,
    tool: &str,
) -> String {
    match resolved {
        Ok((mut r, total_raw, ambiguous)) => {
            let matched = r.len();
            // С class-фильтром r — все совпадения (могло быть > cap); без фильтра r
            // уже усечён ранним лимитом в storage до cap.
            if matched > cap {
                r.truncate(cap);
            }
            let deps: Vec<String> = r.iter().map(|c| c.file_path.clone()).collect();
            let mut notes: Vec<String> = Vec::new();
            if ambiguous && !class_given {
                notes.push(format!(
                    "имя '{}' определено в нескольких классах — рёбра смешаны; уточните параметр class",
                    name
                ));
            }
            let shown = r.len();
            if class_given {
                if matched > shown {
                    notes.push(format!("показано {} из {} (фильтр по class); увеличьте limit", shown, matched));
                }
            } else if total_raw > shown {
                notes.push(format!("показано {} из {}; уточните class/language или увеличьте limit", shown, total_raw));
            }
            if notes.is_empty() {
                wrap_with_meta(&r, deps)
            } else {
                wrap_with_meta_note(&r, deps, &notes.join("; "))
            }
        }
        Err(e) => format!("{{\"error\": \"{}: {}\"}}", tool, e),
    }
}

pub async fn get_callers(
    entry: &RepoEntry,
    function_name: String,
    language: Option<String>,
    limit: Option<usize>,
    class: Option<String>,
) -> String {
    bail_if_not_ready!(entry);
    let storage = entry.local_storage().lock().await;
    let cap = limit.unwrap_or(CALL_EDGES_DEFAULT_LIMIT);
    let resolved = storage.get_callers_resolved(&function_name, class.as_deref(), language.as_deref(), cap);
    emit_resolved_calls(resolved, cap, class.is_some(), &function_name, "get_callers")
}

pub async fn get_callees(
    entry: &RepoEntry,
    function_name: String,
    language: Option<String>,
    limit: Option<usize>,
    class: Option<String>,
) -> String {
    bail_if_not_ready!(entry);
    let storage = entry.local_storage().lock().await;
    let cap = limit.unwrap_or(CALL_EDGES_DEFAULT_LIMIT);
    let resolved = storage.get_callees_resolved(&function_name, class.as_deref(), language.as_deref(), cap);
    emit_resolved_calls(resolved, cap, class.is_some(), &function_name, "get_callees")
}

pub async fn find_symbol(
    entry: &RepoEntry,
    name: String,
    language: Option<String>,
    path_glob: Option<String>,
    include_body: Option<bool>,
) -> String {
    bail_if_not_ready!(entry);
    let storage = entry.local_storage().lock().await;
    match storage.find_symbol(&name, language.as_deref()) {
        Ok(mut r) => {
            if let Some(ref g) = path_glob {
                let matcher = match build_path_matcher(g) {
                    Ok(m) => m,
                    Err(e) => return format!("{{\"error\": \"path_glob: {}\"}}", e),
                };
                r.functions
                    .retain(|fr| matches_with(&matcher, &lookup_path(&storage, fr.file_id)));
                r.classes
                    .retain(|cr| matches_with(&matcher, &lookup_path(&storage, cr.file_id)));
                r.variables
                    .retain(|vr| matches_with(&matcher, &lookup_path(&storage, vr.file_id)));
                r.imports
                    .retain(|ir| matches_with(&matcher, &lookup_path(&storage, ir.file_id)));
            }
            let mut deps = collect_paths_via(&storage, &r.functions, |fr| fr.file_id);
            deps.extend(collect_paths_via(&storage, &r.classes, |cr| cr.file_id));
            deps.extend(collect_paths_via(&storage, &r.variables, |vr| vr.file_id));
            deps.extend(collect_paths_via(&storage, &r.imports, |ir| ir.file_id));
            if include_body.unwrap_or(false) {
                wrap_with_meta(&r, deps)
            } else {
                // Lean: функции/классы без тел (locate). Тело — точечно get_function.
                let lean = crate::storage::models::SymbolSearchLean {
                    functions: r
                        .functions
                        .iter()
                        .map(|f| FunctionHit::from_record(f, lookup_path(&storage, f.file_id)))
                        .collect(),
                    classes: r
                        .classes
                        .iter()
                        .map(|c| ClassHit::from_record(c, lookup_path(&storage, c.file_id)))
                        .collect(),
                    variables: r.variables,
                    imports: r.imports,
                };
                wrap_with_meta(&lean, deps)
            }
        }
        Err(e) => format!("{{\"error\": \"find_symbol: {}\"}}", e),
    }
}

pub async fn get_imports(
    entry: &RepoEntry,
    file_id: Option<i64>,
    module: Option<String>,
    language: Option<String>,
) -> String {
    bail_if_not_ready!(entry);
    let storage = entry.local_storage().lock().await;
    if let Some(fid) = file_id {
        return match storage.get_imports_by_file(fid) {
            Ok(r) => {
                let deps = collect_paths_via(&storage, &r, |ir| ir.file_id);
                wrap_with_meta(&r, deps)
            }
            Err(e) => format!("{{\"error\": \"get_imports_by_file: {}\"}}", e),
        };
    }
    if let Some(ref m) = module {
        return match storage.get_imports_by_module(m, language.as_deref()) {
            Ok(r) => {
                let deps = collect_paths_via(&storage, &r, |ir| ir.file_id);
                wrap_with_meta(&r, deps)
            }
            Err(e) => format!("{{\"error\": \"get_imports_by_module: {}\"}}", e),
        };
    }
    "{\"error\": \"Укажите file_id или module\"}".to_string()
}

pub async fn get_file_summary(entry: &RepoEntry, path: String) -> String {
    bail_if_not_ready!(entry);
    let storage = entry.local_storage().lock().await;
    match storage.get_file_summary(&path, GET_FILE_SUMMARY_BODY_CAP) {
        Ok(Some(s)) => {
            let deps = vec![path.clone()];
            let stale = compute_stale(&storage, entry.root_path.as_deref(), &deps);
            wrap_with_meta_fresh(&s, deps, stale)
        }
        Ok(None) => format!("{{\"error\": \"Файл '{}' не найден\"}}", path),
        Err(e) => format!("{{\"error\": \"get_file_summary: {}\"}}", e),
    }
}

/// Лёгкий скелет файла: имена символов и строки, без тел функций.
/// Стоит в 10–400x меньше токенов чем get_file_summary.
pub async fn get_file_outline(entry: &RepoEntry, path: String) -> String {
    bail_if_not_ready!(entry);
    let storage = entry.local_storage().lock().await;
    match storage.get_file_outline(&path) {
        Ok(Some(outline)) => {
            let deps = vec![path.clone()];
            let stale = compute_stale(&storage, entry.root_path.as_deref(), &deps);
            wrap_with_meta_fresh(&outline, deps, stale)
        }
        Ok(None) => format!("{{\"error\": \"Файл '{}' не найден\"}}", path),
        Err(e) => format!("{{\"error\": \"get_file_outline: {}\"}}", e),
    }
}

/// Статистика по одному репо: читает локальный SQLite. Для remote — паника
/// (диспатчер не должен сюда попадать). get_stats остаётся диагностическим:
/// возвращает данные даже если папка не Ready.
async fn local_stats(alias: &str, entry: &RepoEntry) -> serde_json::Value {
    let root = entry.local_root();
    let path_info = client::path_status_async(root).await.ok();
    let storage = entry.local_storage().lock().await;
    match storage.get_stats() {
        Ok(mut stats) => {
            stats.indexing_status = None;
            serde_json::json!({
                "repo": alias,
                "db": stats,
                "path": root.display().to_string(),
                "daemon": path_info,
            })
        }
        Err(e) => serde_json::json!({
            "repo": alias,
            "error": format!("get_stats: {}", e),
            "path": root.display().to_string(),
        }),
    }
}

/// Запрос статистики у удалённого serve через `/federate/get_stats` с таймаутом.
async fn remote_stats(
    server: &CodeIndexServer,
    alias: &str,
    entry: &RepoEntry,
) -> serde_json::Value {
    use tokio::time::{timeout, Duration};

    let fut = crate::federation::dispatcher::dispatch_remote_value(
        &server.clients,
        &entry.ip,
        entry.port,
        "get_stats",
        serde_json::json!({ "repo": alias }),
    );
    let body = match timeout(Duration::from_secs(5), fut).await {
        Ok(b) => b,
        Err(_) => {
            return serde_json::json!({
                "repo": alias,
                "ip": entry.ip,
                "status": "unreachable",
                "error": "timeout 5s",
            });
        }
    };
    // Удалённый сервер отвечает строкой JSON (тот же формат, что local_stats).
    // Если парсинг падает — остаётся хотя бы raw для диагностики.
    serde_json::from_str::<serde_json::Value>(&body).unwrap_or_else(|_| {
        serde_json::json!({
            "repo": alias,
            "ip": entry.ip,
            "status": "parse_error",
            "raw": body,
        })
    })
}

/// Диспатч одного запроса по `repo` (с учётом is_local). Используется и через
/// MCP-tool, и через `/federate/get_stats` для конкретного алиаса.
pub async fn one_stats(
    server: &CodeIndexServer,
    alias: &str,
    entry: &RepoEntry,
) -> serde_json::Value {
    if entry.is_local {
        local_stats(alias, entry).await
    } else {
        remote_stats(server, alias, entry).await
    }
}

/// Полная сводка: для одного `repo` или fan-out по всем подключённым.
pub async fn get_stats(server: &CodeIndexServer, repo: Option<String>) -> String {
    if let Some(alias) = repo {
        return match server.repos.get(&alias) {
            Some(entry) => to_json(&one_stats(server, &alias, entry).await),
            None => format_unavailable(ToolUnavailable::NotStarted {
                message: format!(
                    "Неизвестный repo '{}'. Доступные: {:?}.",
                    alias,
                    server.repo_aliases()
                ),
            }),
        };
    }

    // Fan-out по всем репо. Параллельно через JoinSet, удалённые с таймаутом 5с.
    let mut set = tokio::task::JoinSet::new();
    for alias in server.repos.keys().cloned().collect::<Vec<_>>() {
        let server_clone = server.clone();
        set.spawn(async move {
            let entry = server_clone
                .repos
                .get(&alias)
                .expect("alias только что взят из repos.keys()");
            one_stats(&server_clone, &alias, entry).await
        });
    }

    let mut all = Vec::new();
    while let Some(res) = set.join_next().await {
        match res {
            Ok(v) => all.push(v),
            Err(e) => all.push(serde_json::json!({
                "status": "join_error",
                "error": e.to_string(),
            })),
        }
    }
    // JoinSet не сохраняет порядок — сортируем по `repo` для стабильности вывода.
    all.sort_by(|a, b| {
        let ka = a.get("repo").and_then(|v| v.as_str()).unwrap_or("");
        let kb = b.get("repo").and_then(|v| v.as_str()).unwrap_or("");
        ka.cmp(kb)
    });
    to_json(&serde_json::json!({ "repos": all }))
}

pub async fn search_text(
    entry: &RepoEntry,
    query: String,
    limit: Option<usize>,
    language: Option<String>,
    path_glob: Option<String>,
) -> String {
    bail_if_not_ready!(entry);
    let storage = entry.local_storage().lock().await;
    let want = limit.unwrap_or(20);
    let sql_limit = if path_glob.is_some() {
        (want.saturating_mul(5)).min(500)
    } else {
        want
    };
    match storage.search_text(&query, sql_limit, language.as_deref()) {
        Ok(mut results) => {
            if let Some(ref g) = path_glob {
                let matcher = match build_path_matcher(g) {
                    Ok(m) => m,
                    Err(e) => return format!("{{\"error\": \"path_glob: {}\"}}", e),
                };
                results.retain(|(p, _)| matches_with(&matcher, p));
                results.truncate(want);
            }
            let deps: Vec<String> = results.iter().map(|(p, _)| p.clone()).collect();
            let items: Vec<serde_json::Value> = results
                .into_iter()
                .map(|(path, snippet)| serde_json::json!({ "path": path, "snippet": snippet }))
                .collect();
            wrap_with_meta(&items, deps)
        }
        Err(e) => format!("{{\"error\": \"search_text: {}\"}}", e),
    }
}

pub async fn grep_body(
    entry: &RepoEntry,
    pattern: Option<String>,
    regex: Option<String>,
    language: Option<String>,
    limit: Option<usize>,
    path_glob: Option<String>,
    context_lines: Option<usize>,
) -> String {
    bail_if_not_ready!(entry);
    let storage = entry.local_storage().lock().await;
    // Если есть либо path_glob, либо context_lines — идём через grep_body_with_options.
    // Иначе старый grep_body для обратной совместимости с CHANGELOG / тестами.
    let ctx = context_lines.unwrap_or(0);
    if path_glob.is_some() || ctx > 0 {
        match storage.grep_body_with_options(
            pattern.as_deref(),
            regex.as_deref(),
            language.as_deref(),
            path_glob.as_deref(),
            limit.unwrap_or(100),
            ctx,
            GREP_TOTAL_BYTES_CAP,
        ) {
            Ok(r) => {
                let deps: Vec<String> = r.iter().map(|m| m.file_path.clone()).collect();
                wrap_with_meta(&r, deps)
            }
            Err(e) => format!("{{\"error\": \"grep_body: {}\"}}", e),
        }
    } else {
        match storage.grep_body(
            pattern.as_deref(),
            regex.as_deref(),
            language.as_deref(),
            limit.unwrap_or(100),
        ) {
            Ok(r) => {
                let deps: Vec<String> = r.iter().map(|m| m.file_path.clone()).collect();
                wrap_with_meta(&r, deps)
            }
            Err(e) => format!("{{\"error\": \"grep_body: {}\"}}", e),
        }
    }
}

// ── Phase 1 tool-handlers ───────────────────────────────────────────────────

pub async fn stat_file(entry: &RepoEntry, path: String) -> String {
    bail_if_not_ready!(entry);
    let storage = entry.local_storage().lock().await;
    // stat_file намеренно НЕ заворачиваем в `_meta` — он non-cacheable по
    // policy (всегда быстрая прямая выборка, к тому же быстро меняется на
    // тонких операциях типа `oversize` после реиндексации). Прокси даже не
    // увидит этот ответ в кэше.
    match storage.stat_file_meta(&path) {
        Ok(r) => to_json(&r),
        Err(e) => format!("{{\"error\": \"stat_file: {}\"}}", e),
    }
}

pub async fn list_files(
    entry: &RepoEntry,
    pattern: Option<String>,
    path_prefix: Option<String>,
    language: Option<String>,
    limit: Option<usize>,
) -> String {
    bail_if_not_ready!(entry);
    let storage = entry.local_storage().lock().await;
    match storage.list_files_filtered(
        pattern.as_deref(),
        path_prefix.as_deref(),
        language.as_deref(),
        limit.unwrap_or(500),
    ) {
        Ok(r) => {
            let deps: Vec<String> = r.iter().map(|lf| lf.path.clone()).collect();
            wrap_with_meta(&r, deps)
        }
        Err(e) => format!("{{\"error\": \"list_files: {}\"}}", e),
    }
}

pub async fn read_file(
    entry: &RepoEntry,
    path: String,
    line_start: Option<usize>,
    line_end: Option<usize>,
) -> String {
    bail_if_not_ready!(entry);
    let storage = entry.local_storage().lock().await;
    match storage.read_file_text(
        &path,
        line_start,
        line_end,
        READ_FILE_SOFT_CAP_LINES,
        READ_FILE_SOFT_CAP_BYTES,
        READ_FILE_HARD_CAP_BYTES,
        // size_limit_bytes для hint в oversize-ответе. MCP-слой не знает per-repo
        // лимит daemon'а — передаём None, hint будет короткий «файл превышает лимит».
        // file_size в ответе всё равно показывается, оператор может сравнить.
        None,
    ) {
        Ok(Some(r)) => {
            let deps = vec![path.clone()];
            let stale = compute_stale(&storage, entry.root_path.as_deref(), &deps);
            wrap_with_meta_fresh(&r, deps, stale)
        }
        Ok(None) => format!("{{\"error\": \"Файл '{}' не найден в индексе\"}}", path),
        Err(e) => format!("{{\"error\": \"read_file: {}\"}}", e),
    }
}

pub async fn grep_text(
    entry: &RepoEntry,
    regex: String,
    path_glob: Option<String>,
    language: Option<String>,
    limit: Option<usize>,
    context_lines: Option<usize>,
) -> String {
    bail_if_not_ready!(entry);
    let storage = entry.local_storage().lock().await;
    let want = limit.unwrap_or_else(|| {
        // Без path_glob и language full-scan может быть тяжёлым — занижаем default.
        if path_glob.is_none() && language.is_none() {
            GREP_TEXT_FULL_SCAN_DEFAULT_LIMIT
        } else {
            500
        }
    });
    match storage.grep_text_filtered(
        &regex,
        path_glob.as_deref(),
        language.as_deref(),
        want,
        context_lines.unwrap_or(0),
        GREP_TOTAL_BYTES_CAP,
    ) {
        Ok(r) => {
            let deps: Vec<String> = r.iter().map(|m| m.path.clone()).collect();
            wrap_with_meta(&r, deps)
        }
        Err(e) => format!("{{\"error\": \"grep_text: {}\"}}", e),
    }
}

/// grep_code (Phase 2, v0.8.0): regex-поиск по содержимому **code-файлов** через
/// `file_contents` (zstd). Закрывает слепые зоны `grep_body` (ищет только в телах
/// функций/классов): module-level код, имена символов как идентификаторы,
/// комментарии вне тел, макросы, use-импорты. Файлы с `oversize=true` пропускаются —
/// для них нет content в индексе, нужно увеличить `max_code_file_size_bytes` либо
/// читать с диска.
pub async fn grep_code(
    entry: &RepoEntry,
    regex: String,
    path_glob: Option<String>,
    language: Option<String>,
    limit: Option<usize>,
    context_lines: Option<usize>,
) -> String {
    bail_if_not_ready!(entry);
    let storage = entry.local_storage().lock().await;
    let want = limit.unwrap_or_else(|| {
        // Без path_glob/language full-scan по всему репо может быть тяжёлым:
        // distinct от grep_text здесь сильнее, потому что zstd-decode на каждый
        // файл — full-scan на 100K файлов реально дорогой. Занижаем default.
        if path_glob.is_none() && language.is_none() {
            GREP_TEXT_FULL_SCAN_DEFAULT_LIMIT
        } else {
            500
        }
    });
    match storage.grep_code_filtered(
        &regex,
        path_glob.as_deref(),
        language.as_deref(),
        want,
        context_lines.unwrap_or(0),
        GREP_TOTAL_BYTES_CAP,
    ) {
        Ok(r) => {
            let deps: Vec<String> = r.iter().map(|m| m.path.clone()).collect();
            wrap_with_meta(&r, deps)
        }
        Err(e) => format!("{{\"error\": \"grep_code: {}\"}}", e),
    }
}

/// Живость MCP + демон по каждому репо.
pub async fn health(server: &CodeIndexServer) -> String {
    let daemon_info = client::runtime_info();

    // Сводка по репо: для local — статус пути у демона; для remote —
    // короткая запись без HTTP-ping (ping вне rc6).
    let mut repos = Vec::new();
    for (alias, entry) in server.repos.iter() {
        if !entry.is_local {
            repos.push(serde_json::json!({
                "repo": alias,
                "ip": entry.ip,
                "kind": "remote",
            }));
            continue;
        }
        let root = entry.local_root();
        let path_status = match client::path_status_async(root).await {
            Ok(s) => serde_json::to_value(s).unwrap_or(serde_json::Value::Null),
            Err(e) => serde_json::json!({ "error": e.to_string() }),
        };
        repos.push(serde_json::json!({
            "repo": alias,
            "root_path": root.display().to_string(),
            "path_status": path_status,
        }));
    }

    let daemon_health = match daemon_info {
        Some(_) => serde_json::json!({ "status": "online" }),
        None => serde_json::json!({
            "status": "offline",
            "message": "Демон не запущен (runtime-info отсутствует)",
        }),
    };

    let graph_status = match &server.graph_client {
        Some(_) => serde_json::json!({ "status": "connected" }),
        None => serde_json::json!({ "status": "disabled", "hint": "Add [graph] section to daemon.toml to enable" }),
    };

    let obj = serde_json::json!({
        "mcp": {
            "status": "ok",
            "version": env!("CARGO_PKG_VERSION"),
            "repos": server.repo_aliases(),
        },
        "daemon": daemon_health,
        "graph": graph_status,
        "repos": repos,
    });
    to_json(&obj)
}

// ── Граф-инструменты (v0.10+) ─────────────────────────────────────────────────

fn graph_unavailable() -> String {
    to_json(&serde_json::json!({
        "error": "graph_unavailable",
        "message": "Граф-слой не настроен. Добавьте секцию [graph] в daemon.toml и перезапустите демон.",
    }))
}

pub(crate) fn graph_unavailable_remote() -> String {
    to_json(&serde_json::json!({
        "error": "graph_remote_unsupported",
        "message": "Граф-инструменты не поддерживают удалённые репо",
    }))
}

pub async fn find_dependencies(server: &CodeIndexServer, entry: &RepoEntry, path: String, depth: i64) -> String {
    bail_if_not_ready!(entry);
    let Some(client) = &server.graph_client else {
        return graph_unavailable();
    };
    match client.find_dependencies(&entry.alias, &path, depth).await {
        Ok(deps) => {
            let count = deps.len();
            to_json(&serde_json::json!({
                "path": path,
                "depth": depth,
                "dependencies": deps,
                "count": count,
            }))
        }
        Err(e) => to_json(&serde_json::json!({ "error": e.to_string() })),
    }
}

pub async fn impact_analysis(server: &CodeIndexServer, entry: &RepoEntry, path: String, depth: i64) -> String {
    bail_if_not_ready!(entry);
    let Some(client) = &server.graph_client else {
        return graph_unavailable();
    };
    match client.impact_analysis(&entry.alias, &path, depth).await {
        Ok(impacted) => {
            let count = impacted.len();
            to_json(&serde_json::json!({
                "path": path,
                "depth": depth,
                "impacted_files": impacted,
                "count": count,
            }))
        }
        Err(e) => to_json(&serde_json::json!({ "error": e.to_string() })),
    }
}

pub async fn get_call_chain(server: &CodeIndexServer, entry: &RepoEntry, from_fn: String, to_fn: String) -> String {
    bail_if_not_ready!(entry);
    let Some(client) = &server.graph_client else {
        return graph_unavailable();
    };
    match client.get_call_chain(&entry.alias, &from_fn, &to_fn).await {
        Ok(Some(chain)) => to_json(&serde_json::json!({
            "from": from_fn,
            "to": to_fn,
            "chain": chain,
            "hops": chain.len().saturating_sub(1),
        })),
        Ok(None) => to_json(&serde_json::json!({
            "from": from_fn,
            "to": to_fn,
            "chain": null,
            "message": "Путь вызовов не найден",
        })),
        Err(e) => to_json(&serde_json::json!({ "error": e.to_string() })),
    }
}

// ── Deep analysis tools ────────────────────────────────────────────────────

pub async fn get_symbol_context(
    entry: &RepoEntry,
    name: String,
    file_hint: Option<String>,
    language: Option<String>,
) -> String {
    bail_if_not_ready!(entry);
    let storage = entry.local_storage().lock().await;
    match storage.get_symbol_context(&name, file_hint.as_deref(), language.as_deref()) {
        Ok(ctx) => {
            let mut deps: Vec<String> = ctx.callers.iter().map(|c| c.file_path.clone()).collect();
            deps.extend(ctx.callees.iter().map(|c| c.file_path.clone()));
            if let Some(ref outline) = ctx.file_outline {
                deps.push(outline.path.clone());
            }
            for r in &ctx.routes {
                deps.push(r.file_path.clone());
            }
            let stale = compute_stale(&storage, entry.root_path.as_deref(), &deps);
            wrap_with_meta_fresh(&ctx, deps, stale)
        }
        Err(e) => format!("{{\"error\": \"get_symbol_context: {}\"}}", e),
    }
}

pub async fn get_callers_tree(
    entry: &RepoEntry,
    function_name: String,
    depth: Option<usize>,
    language: Option<String>,
) -> String {
    bail_if_not_ready!(entry);
    let storage = entry.local_storage().lock().await;
    let max_depth = depth.unwrap_or(3).min(10);
    match storage.get_callers_transitive(&function_name, max_depth, language.as_deref()) {
        Ok(nodes) => {
            let deps: Vec<String> = nodes.iter().map(|n| n.file_path.clone()).collect();
            let result = serde_json::json!({
                "function": function_name,
                "max_depth": max_depth,
                "total": nodes.len(),
                "nodes": nodes,
            });
            wrap_with_meta(&result, deps)
        }
        Err(e) => format!("{{\"error\": \"get_callers_tree: {}\"}}", e),
    }
}

pub async fn get_callees_tree(
    entry: &RepoEntry,
    function_name: String,
    depth: Option<usize>,
    language: Option<String>,
) -> String {
    bail_if_not_ready!(entry);
    let storage = entry.local_storage().lock().await;
    let max_depth = depth.unwrap_or(3).min(10);
    match storage.get_callees_transitive(&function_name, max_depth, language.as_deref()) {
        Ok(nodes) => {
            let deps: Vec<String> = nodes.iter().map(|n| n.file_path.clone()).collect();
            let result = serde_json::json!({
                "function": function_name,
                "max_depth": max_depth,
                "total": nodes.len(),
                "nodes": nodes,
            });
            wrap_with_meta(&result, deps)
        }
        Err(e) => format!("{{\"error\": \"get_callees_tree: {}\"}}", e),
    }
}

pub async fn get_implementations(
    entry: &RepoEntry,
    name: String,
    language: Option<String>,
) -> String {
    bail_if_not_ready!(entry);
    let storage = entry.local_storage().lock().await;
    match storage.get_implementations(&name, language.as_deref()) {
        Ok(records) => {
            let deps: Vec<String> = records.iter().map(|r| r.file_path.clone()).collect();
            let result = serde_json::json!({
                "base_class": name,
                "count": records.len(),
                "implementations": records,
            });
            wrap_with_meta(&result, deps)
        }
        Err(e) => format!("{{\"error\": \"get_implementations: {}\"}}", e),
    }
}

pub async fn find_dead_code(
    entry: &RepoEntry,
    language: Option<String>,
    path_glob: Option<String>,
    limit: Option<usize>,
) -> String {
    bail_if_not_ready!(entry);
    let storage = entry.local_storage().lock().await;
    let want = limit.unwrap_or(50);
    match storage.find_dead_code(want, path_glob.as_deref(), language.as_deref()) {
        Ok(entries) => {
            let deps: Vec<String> = entries.iter().map(|e| e.file_path.clone()).collect();
            let result = serde_json::json!({
                "count": entries.len(),
                "note": "Результат приблизителен: рефлексия и event-handlers не видны в индексе",
                "dead_code": entries,
            });
            wrap_with_meta(&result, deps)
        }
        Err(e) => format!("{{\"error\": \"find_dead_code: {}\"}}", e),
    }
}

pub async fn find_unreachable(
    entry: &RepoEntry,
    language: Option<String>,
    path_glob: Option<String>,
    limit: Option<usize>,
) -> String {
    bail_if_not_ready!(entry);
    let storage = entry.local_storage().lock().await;
    let want = limit.unwrap_or(50);
    match storage.find_unreachable(want, path_glob.as_deref(), language.as_deref()) {
        Ok(entries) => {
            let deps: Vec<String> = entries.iter().map(|e| e.file_path.clone()).collect();
            let result = serde_json::json!({
                "count": entries.len(),
                "note": "Недостижимо от точек входа (маршруты/main/handle/тесты). НЕ видит рефлексию/динамику/строковые колбэки — проверяйте кандидатов",
                "unreachable": entries,
            });
            wrap_with_meta(&result, deps)
        }
        Err(e) => format!("{{\"error\": \"find_unreachable: {}\"}}", e),
    }
}

/// get_repo_map (v0.11+): архитектурная карта репо за один дешёвый вызов.
pub async fn get_repo_map(entry: &RepoEntry, top: Option<usize>) -> String {
    bail_if_not_ready!(entry);
    let storage = entry.local_storage().lock().await;
    match storage.repo_map(top.unwrap_or(12)) {
        // Агрегатная карта — без dependent_files (зависит от всего репо).
        Ok(m) => wrap_with_meta(&m, vec![]),
        Err(e) => format!("{{\"error\": \"get_repo_map: {}\"}}", e),
    }
}

/// find_complex_functions (v0.11+): ранжирование функций по сложности.
pub async fn find_complex_functions(
    entry: &RepoEntry,
    limit: Option<usize>,
    path_glob: Option<String>,
    language: Option<String>,
) -> String {
    bail_if_not_ready!(entry);
    let storage = entry.local_storage().lock().await;
    match storage.find_complex_functions(limit.unwrap_or(15), path_glob.as_deref(), language.as_deref()) {
        Ok(r) => {
            let deps: Vec<String> = r.iter().map(|f| f.file_path.clone()).collect();
            wrap_with_meta(&r, deps)
        }
        Err(e) => format!("{{\"error\": \"find_complex_functions: {}\"}}", e),
    }
}

/// find_routes (v0.11+): веб-маршруты фреймворка (framework-aware routing).
pub async fn find_routes(
    entry: &RepoEntry,
    method: Option<String>,
    path: Option<String>,
    handler: Option<String>,
    limit: Option<usize>,
) -> String {
    bail_if_not_ready!(entry);
    let storage = entry.local_storage().lock().await;
    let want = limit.unwrap_or(100);
    match storage.find_routes(method.as_deref(), path.as_deref(), handler.as_deref(), want) {
        Ok(routes) => {
            let deps: Vec<String> = routes.iter().map(|r| r.file_path.clone()).collect();
            let result = serde_json::json!({
                "count": routes.len(),
                "routes": routes,
            });
            let stale = compute_stale(&storage, entry.root_path.as_deref(), &deps);
            wrap_with_meta_fresh(&result, deps, stale)
        }
        Err(e) => format!("{{\"error\": \"find_routes: {}\"}}", e),
    }
}

pub async fn find_existing(
    entry: &RepoEntry,
    query: String,
    kind: Option<String>,
    language: Option<String>,
    limit: Option<usize>,
) -> String {
    bail_if_not_ready!(entry);
    let storage = entry.local_storage().lock().await;
    let want = limit.unwrap_or(15);
    match storage.find_existing(&query, kind.as_deref(), language.as_deref(), want) {
        Ok(matches) => {
            let deps: Vec<String> = matches.iter().map(|m| m.file_path.clone()).collect();
            let result = serde_json::json!({ "count": matches.len(), "matches": matches });
            let stale = compute_stale(&storage, entry.root_path.as_deref(), &deps);
            wrap_with_meta_fresh(&result, deps, stale)
        }
        Err(e) => format!("{{\"error\": \"find_existing: {}\"}}", e),
    }
}

// ── Тесты freshness/staleness ────────────────────────────────────────────────

#[cfg(test)]
mod fresh_tests {
    use super::*;
    use crate::storage::models::FileRecord;
    use crate::storage::Storage;
    use tokio::sync::Mutex;

    fn put_file(storage: &Storage, path: &str, mtime: i64, size: i64) {
        storage
            .upsert_file(&FileRecord {
                id: None,
                path: path.to_string(),
                content_hash: "h".to_string(),
                ast_hash: None,
                language: "text".to_string(),
                lines_total: 1,
                indexed_at: String::new(),
                mtime: Some(mtime),
                file_size: Some(size),
            })
            .unwrap();
        // upsert_file сам персистит mtime/file_size из FileRecord (см. write.rs).
    }

    #[tokio::test]
    async fn compute_stale_flags_changed_and_missing_but_not_fresh() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();

        let fpath = root.join("a.txt");
        std::fs::write(&fpath, "hello").unwrap();
        let meta = std::fs::metadata(&fpath).unwrap();
        let mtime = meta
            .modified()
            .unwrap()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        let size = meta.len() as i64;

        let storage = Storage::open_in_memory().unwrap();
        put_file(&storage, "a.txt", mtime, size); // совпадает с диском → свежий
        put_file(&storage, "gone.txt", 123, 5); // нет на диске → устарел

        let m = Mutex::new(storage);
        let guard = m.lock().await;

        let stale = compute_stale(&guard, Some(root), &["a.txt".into(), "gone.txt".into()]);
        assert!(stale.contains(&"gone.txt".to_string()), "удалённый с диска файл → stale");
        assert!(!stale.contains(&"a.txt".to_string()), "совпадающий файл не должен быть stale");

        // Изменяем содержимое на диске → размер отличается → stale.
        std::fs::write(&fpath, "hello, a much longer content").unwrap();
        let stale2 = compute_stale(&guard, Some(root), &["a.txt".into()]);
        assert!(stale2.contains(&"a.txt".to_string()), "изменённый на диске файл → stale");
    }

    #[tokio::test]
    async fn compute_stale_empty_without_root() {
        let storage = Storage::open_in_memory().unwrap();
        let m = Mutex::new(storage);
        let guard = m.lock().await;
        // root=None (remote-репо) → ничего не помечаем.
        let stale = compute_stale(&guard, None, &["whatever.txt".into()]);
        assert!(stale.is_empty());
    }
}
