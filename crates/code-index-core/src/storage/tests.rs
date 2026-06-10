use super::*;

fn make_file(path: &str) -> FileRecord {
    FileRecord {
        id: None,
        path: path.to_string(),
        content_hash: "abc123".to_string(),
        ast_hash: None,
        language: "python".to_string(),
        lines_total: 100,
        indexed_at: "2026-01-01T00:00:00".to_string(),
        mtime: None,
        file_size: None,
    }
}

fn make_function(file_id: i64, name: &str) -> FunctionRecord {
    FunctionRecord {
        id: None,
        file_id,
        name: name.to_string(),
        qualified_name: Some(format!("module.{name}")),
        line_start: 1,
        line_end: 10,
        args: Some("(x, y)".to_string()),
        return_type: Some("int".to_string()),
        docstring: Some(format!("Вычисляет {name}")),
        body: format!("def {name}(x, y):\n    return x + y"),
        is_async: false,
        node_hash: "hash123".to_string(),
        ..Default::default()
    }
}

#[test]
fn test_create_and_query_file() {
    let storage = Storage::open_in_memory().expect("Ошибка создания in-memory БД");
    let rec = make_file("/src/main.py");
    let id = storage.upsert_file(&rec).expect("upsert_file");
    assert!(id > 0);
    let found = storage.get_file_by_path("/src/main.py").expect("get_file_by_path").expect("должен существовать");
    assert_eq!(found.path, "/src/main.py");
    assert_eq!(found.language, "python");
    assert_eq!(found.lines_total, 100);
}

#[test]
fn test_upsert_updates_existing() {
    let storage = Storage::open_in_memory().expect("Ошибка создания in-memory БД");
    let rec = make_file("/src/utils.py");
    let id1 = storage.upsert_file(&rec).expect("первый upsert");
    let mut rec2 = rec.clone();
    rec2.content_hash = "newHash".to_string();
    rec2.lines_total = 200;
    let id2 = storage.upsert_file(&rec2).expect("второй upsert");
    assert_eq!(id1, id2);
    let found = storage.get_file_by_path("/src/utils.py").unwrap().unwrap();
    assert_eq!(found.content_hash, "newHash");
    assert_eq!(found.lines_total, 200);
}

#[test]
fn test_functions_crud() {
    let storage = Storage::open_in_memory().expect("Ошибка создания БД");
    let file_id = storage.upsert_file(&make_file("/src/funcs.py")).unwrap();
    storage.insert_functions(&[make_function(file_id, "add"), make_function(file_id, "subtract")]).expect("insert_functions");
    let found = storage.get_function_by_name("add").expect("get_function_by_name");
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].name, "add");
    storage.delete_functions_by_file(file_id).expect("delete_functions_by_file");
    assert!(storage.get_function_by_name("add").unwrap().is_empty());
}

#[test]
fn test_fts_search() {
    let storage = Storage::open_in_memory().expect("Ошибка создания БД");
    let file_id = storage.upsert_file(&make_file("/src/algo.py")).unwrap();
    let funcs = vec![
        FunctionRecord {
            id: None, file_id, name: "binary_search".to_string(), qualified_name: None,
            line_start: 1, line_end: 20, args: Some("(arr, target)".to_string()),
            return_type: Some("int".to_string()),
            docstring: Some("Бинарный поиск в отсортированном массиве".to_string()),
            body: "def binary_search(arr, target):\n    pass".to_string(),
            is_async: false, node_hash: "hs1".to_string(), ..Default::default()
        },
        FunctionRecord {
            id: None, file_id, name: "linear_scan".to_string(), qualified_name: None,
            line_start: 22, line_end: 30, args: None, return_type: None,
            docstring: Some("Линейный обход списка".to_string()),
            body: "def linear_scan():\n    pass".to_string(),
            is_async: false, node_hash: "hs2".to_string(), ..Default::default()
        },
    ];
    storage.insert_functions(&funcs).unwrap();
    let results = storage.search_functions("binary_search", 10, None).expect("search_functions");
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].name, "binary_search");
}

#[test]
fn test_cascade_delete() {
    let storage = Storage::open_in_memory().expect("Ошибка создания БД");
    let file_id = storage.upsert_file(&make_file("/src/cascade.py")).unwrap();
    storage.insert_functions(&[make_function(file_id, "foo")]).unwrap();
    storage.insert_classes(&[ClassRecord {
        id: None, file_id, name: "Bar".into(), line_start: 1, line_end: 5,
        bases: None, docstring: None, body: "class Bar: pass".into(), node_hash: "h".into(),
    }]).unwrap();
    storage.delete_file(file_id).unwrap();
    assert!(storage.get_function_by_name("foo").unwrap().is_empty());
    assert!(storage.get_class_by_name("Bar").unwrap().is_empty());
}

#[test]
fn test_find_symbol() {
    let storage = Storage::open_in_memory().expect("Ошибка создания БД");
    let file_id = storage.upsert_file(&make_file("/src/symbols.py")).unwrap();
    storage.insert_functions(&[make_function(file_id, "compute")]).unwrap();
    storage.insert_variables(&[VariableRecord { id: None, file_id, name: "compute".into(), value: Some("42".into()), line: 5 }]).unwrap();
    let result = storage.find_symbol("compute", None).expect("find_symbol");
    assert_eq!(result.functions.len(), 1);
    assert_eq!(result.variables.len(), 1);
    assert!(result.classes.is_empty());
}

#[test]
fn test_stats() {
    let storage = Storage::open_in_memory().expect("Ошибка создания БД");
    assert_eq!(storage.get_stats().expect("get_stats").total_files, 0);
    let file_id = storage.upsert_file(&make_file("/src/stats.py")).unwrap();
    storage.insert_functions(&[make_function(file_id, "f1"), make_function(file_id, "f2")]).unwrap();
    storage.insert_calls(&[CallRecord { id: None, file_id, caller: "f1".into(), callee: "f2".into(), line: 5, receiver: None }]).unwrap();
    let stats = storage.get_stats().expect("get_stats после вставки");
    assert_eq!(stats.total_files, 1);
    assert_eq!(stats.total_functions, 2);
    assert_eq!(stats.total_calls, 1);
}

#[test]
fn test_language_filter() {
    let storage = Storage::open_in_memory().expect("Ошибка создания БД");
    let py_id = storage.upsert_file(&make_file("/src/algo.py")).unwrap();
    let rs_id = storage.upsert_file(&FileRecord {
        id: None, path: "/src/main.rs".into(), content_hash: "rustHash".into(),
        ast_hash: None, language: "rust".into(), lines_total: 50,
        indexed_at: "2026-01-01T00:00:00".into(), mtime: None, file_size: None,
    }).unwrap();
    storage.insert_functions(&[make_function(py_id, "py_func")]).unwrap();
    storage.insert_functions(&[make_function(rs_id, "rs_func")]).unwrap();
    assert_eq!(storage.search_functions("func", 10, None).unwrap().len(), 2);
    let py = storage.search_functions("func", 10, Some("python")).unwrap();
    assert_eq!(py.len(), 1);
    assert_eq!(py[0].name, "py_func");
    let rs = storage.search_functions("func", 10, Some("rust")).unwrap();
    assert_eq!(rs.len(), 1);
    assert_eq!(rs[0].name, "rs_func");
}

#[test]
fn test_fts_with_dashes() {
    let storage = Storage::open_in_memory().expect("Ошибка создания БД");
    let file_id = storage.upsert_file(&make_file("/src/deps.py")).unwrap();
    let func = FunctionRecord {
        id: None, file_id, name: "use_tree_sitter".to_string(), qualified_name: None,
        line_start: 1, line_end: 5, args: None, return_type: None,
        docstring: Some("Использует tree-sitter-python для разбора".to_string()),
        body: "def use_tree_sitter(): pass".to_string(),
        is_async: false, node_hash: "h_ts".to_string(), ..Default::default()
    };
    storage.insert_functions(&[func]).unwrap();
    let results = storage.search_functions("tree-sitter-python", 10, None).expect("поиск с дефисом");
    assert_eq!(results.len(), 1);
}

#[test]
fn test_flush_to_disk() {
    let tmp = tempfile::TempDir::new().unwrap();
    let db_path = tmp.path().join("test.db");
    let storage = Storage::open_in_memory().unwrap();
    let rec = FileRecord {
        id: None, path: "test.py".into(), content_hash: "abc".into(),
        ast_hash: None, language: "python".into(), lines_total: 10,
        indexed_at: "2026-01-01".into(), mtime: None, file_size: None,
    };
    storage.upsert_file(&rec).unwrap();
    storage.flush_to_disk(&db_path).unwrap();
    assert!(db_path.exists());
    let storage2 = Storage::open_file(&db_path).unwrap();
    let file = storage2.get_file_by_path("test.py").unwrap();
    assert!(file.is_some());
    assert_eq!(file.unwrap().content_hash, "abc");
}

#[test]
fn test_open_auto_in_memory_for_new_db() {
    let tmp = tempfile::TempDir::new().unwrap();
    let db_path = tmp.path().join("index.db");
    let config = memory::StorageConfig { mode: "auto".to_string(), memory_max_percent: 25 };
    let storage = Storage::open_auto(&db_path, &config).expect("open_auto");
    storage.upsert_file(&make_file("/hello.py")).unwrap();
    assert!(storage.get_file_by_path("/hello.py").unwrap().is_some());
}

#[test]
fn test_open_auto_disk_mode() {
    let tmp = tempfile::TempDir::new().unwrap();
    let db_path = tmp.path().join("index.db");
    let config = memory::StorageConfig { mode: "disk".to_string(), memory_max_percent: 25 };
    let storage = Storage::open_auto(&db_path, &config).expect("open_auto disk");
    storage.upsert_file(&make_file("/hello.rs")).unwrap();
    assert!(db_path.exists());
}

#[test]
fn test_open_auto_loads_existing_db() {
    let tmp = tempfile::TempDir::new().unwrap();
    let db_path = tmp.path().join("index.db");
    { let s = Storage::open_file(&db_path).unwrap(); s.upsert_file(&make_file("/existing.py")).unwrap(); }
    let config = memory::StorageConfig { mode: "memory".to_string(), memory_max_percent: 25 };
    let storage = Storage::open_auto(&db_path, &config).unwrap();
    assert!(storage.get_file_by_path("/existing.py").unwrap().is_some());
}

// ── Phase 1 тесты ──────────────────────────────────────────────────────

fn make_file_full(path: &str, language: &str, lines: usize) -> FileRecord {
    FileRecord {
        id: None, path: path.to_string(), content_hash: format!("hash_{}", path),
        ast_hash: None, language: language.to_string(), lines_total: lines,
        indexed_at: "2026-04-28T12:00:00".to_string(),
        mtime: Some(1714305600), file_size: Some((lines * 50) as i64),
    }
}

#[test]
fn test_normalize_glob_replaces_double_star() {
    assert_eq!(normalize_glob("**/*.py"), "*/*.py");
    assert_eq!(normalize_glob("src/**/file.rs"), "src/*/file.rs");
    assert_eq!(normalize_glob("***/foo"), "*/foo");
    assert_eq!(normalize_glob("*.py"), "*.py");
}

#[test]
fn test_slice_with_caps_full_file() {
    let (body, n, truncated) = slice_with_caps("line1\nline2\nline3\nline4\nline5", None, None, 100, 1000, 10_000).unwrap();
    assert_eq!(n, 5); assert!(!truncated);
    assert_eq!(body, "line1\nline2\nline3\nline4\nline5");
}

#[test]
fn test_slice_with_caps_range() {
    let (body, n, truncated) = slice_with_caps("a\nb\nc\nd\ne", Some(2), Some(4), 100, 1000, 10_000).unwrap();
    assert_eq!(n, 3); assert!(!truncated); assert_eq!(body, "b\nc\nd");
}

#[test]
fn test_slice_with_caps_soft_cap_lines() {
    let content = (1..=10).map(|i| format!("line{}", i)).collect::<Vec<_>>().join("\n");
    let (body, n, truncated) = slice_with_caps(&content, None, None, 3, 1000, 10_000).unwrap();
    assert_eq!(n, 3); assert!(truncated); assert_eq!(body, "line1\nline2\nline3");
}

#[test]
fn test_slice_with_caps_hard_cap() {
    let content: String = "x".repeat(1000);
    assert!(slice_with_caps(&content, None, None, 10_000, 100_000, 100).is_err());
}

#[test]
fn test_stat_file_meta_existing_text() {
    let storage = Storage::open_in_memory().unwrap();
    let id = storage.upsert_file(&make_file_full("/cfg.yaml", "yaml", 50)).unwrap();
    storage.update_file_metadata("/cfg.yaml", 1714305600, 2500).unwrap();
    storage.insert_text_file(&TextFileRecord { id: None, file_id: id, content: "key: value\n".repeat(50) }).unwrap();
    let r = storage.stat_file_meta("/cfg.yaml").unwrap();
    assert!(r.exists); assert_eq!(r.language.as_deref(), Some("yaml"));
    assert_eq!(r.category.as_deref(), Some("text"));
}

#[test]
fn test_stat_file_meta_existing_code() {
    let storage = Storage::open_in_memory().unwrap();
    storage.upsert_file(&make_file_full("/lib.py", "python", 30)).unwrap();
    let r = storage.stat_file_meta("/lib.py").unwrap();
    assert!(r.exists); assert_eq!(r.category.as_deref(), Some("code"));
}

#[test]
fn test_stat_file_meta_missing() {
    let storage = Storage::open_in_memory().unwrap();
    let r = storage.stat_file_meta("/nonexistent").unwrap();
    assert!(!r.exists); assert!(r.language.is_none());
}

#[test]
fn test_list_files_pattern_glob() {
    let storage = Storage::open_in_memory().unwrap();
    storage.upsert_file(&make_file_full("/src/auth/login.py", "python", 10)).unwrap();
    storage.upsert_file(&make_file_full("/src/utils/helpers.py", "python", 20)).unwrap();
    storage.upsert_file(&make_file_full("/docs/readme.md", "markdown", 30)).unwrap();
    let py = storage.list_files_filtered(Some("**/*.py"), None, None, 100).unwrap();
    assert_eq!(py.len(), 2);
    let auth = storage.list_files_filtered(Some("/src/auth/*"), None, None, 100).unwrap();
    assert_eq!(auth.len(), 1);
    assert_eq!(auth[0].path, "/src/auth/login.py");
}

#[test]
fn test_list_files_path_prefix() {
    let storage = Storage::open_in_memory().unwrap();
    storage.upsert_file(&make_file_full("/src/a.py", "python", 1)).unwrap();
    storage.upsert_file(&make_file_full("/src/b.py", "python", 1)).unwrap();
    storage.upsert_file(&make_file_full("/test/c.py", "python", 1)).unwrap();
    let r = storage.list_files_filtered(None, Some("/src/"), None, 100).unwrap();
    assert_eq!(r.len(), 2);
}

#[test]
fn test_list_files_language_filter() {
    let storage = Storage::open_in_memory().unwrap();
    storage.upsert_file(&make_file_full("/a.py", "python", 1)).unwrap();
    storage.upsert_file(&make_file_full("/b.rs", "rust", 1)).unwrap();
    storage.upsert_file(&make_file_full("/c.py", "python", 1)).unwrap();
    let r = storage.list_files_filtered(None, None, Some("rust"), 100).unwrap();
    assert_eq!(r.len(), 1); assert_eq!(r[0].language, "rust");
}

#[test]
fn test_read_file_text_full() {
    let storage = Storage::open_in_memory().unwrap();
    let id = storage.upsert_file(&make_file_full("/r.txt", "text", 3)).unwrap();
    storage.insert_text_file(&TextFileRecord { id: None, file_id: id, content: "alpha\nbeta\ngamma".into() }).unwrap();
    let r = storage.read_file_text("/r.txt", None, None, 100, 10_000, 100_000, None).unwrap().unwrap();
    assert_eq!(r.category, "text"); assert_eq!(r.lines_returned, 3); assert_eq!(r.content, "alpha\nbeta\ngamma");
}

#[test]
fn test_read_file_text_range() {
    let storage = Storage::open_in_memory().unwrap();
    let id = storage.upsert_file(&make_file_full("/r.txt", "text", 5)).unwrap();
    storage.insert_text_file(&TextFileRecord { id: None, file_id: id, content: "1\n2\n3\n4\n5".into() }).unwrap();
    let r = storage.read_file_text("/r.txt", Some(2), Some(4), 100, 10_000, 100_000, None).unwrap().unwrap();
    assert_eq!(r.lines_returned, 3); assert_eq!(r.content, "2\n3\n4");
}

#[test]
fn test_read_file_text_code_returns_empty_category_code() {
    let storage = Storage::open_in_memory().unwrap();
    storage.upsert_file(&make_file_full("/lib.py", "python", 10)).unwrap();
    let r = storage.read_file_text("/lib.py", None, None, 100, 10_000, 100_000, None).unwrap().unwrap();
    assert_eq!(r.category, "code"); assert!(r.content.is_empty());
}

#[test]
fn test_read_file_text_missing() {
    let storage = Storage::open_in_memory().unwrap();
    assert!(storage.read_file_text("/nope", None, None, 100, 10_000, 100_000, None).unwrap().is_none());
}

#[test]
fn test_grep_text_basic_match() {
    let storage = Storage::open_in_memory().unwrap();
    let id = storage.upsert_file(&make_file_full("/cfg.yaml", "yaml", 5)).unwrap();
    storage.insert_text_file(&TextFileRecord { id: None, file_id: id, content: "host: 10.0.0.1\nport: 8080\nname: example\n".into() }).unwrap();
    let m = storage.grep_text_filtered(r"port:\s*\d+", None, None, 100, 0, 1_000_000).unwrap();
    assert_eq!(m.len(), 1); assert_eq!(m[0].line, 2);
}

#[test]
fn test_grep_text_with_context() {
    let storage = Storage::open_in_memory().unwrap();
    let id = storage.upsert_file(&make_file_full("/log.txt", "text", 5)).unwrap();
    storage.insert_text_file(&TextFileRecord { id: None, file_id: id, content: "a\nb\nFOUND\nd\ne".into() }).unwrap();
    let m = storage.grep_text_filtered(r"FOUND", None, None, 100, 1, 1_000_000).unwrap();
    assert_eq!(m.len(), 1); assert_eq!(m[0].context.len(), 3);
}

#[test]
fn test_grep_text_path_glob_filters() {
    let storage = Storage::open_in_memory().unwrap();
    let id1 = storage.upsert_file(&make_file_full("/a.yaml", "yaml", 1)).unwrap();
    let id2 = storage.upsert_file(&make_file_full("/b.json", "json", 1)).unwrap();
    storage.insert_text_file(&TextFileRecord { id: None, file_id: id1, content: "key: 42".into() }).unwrap();
    storage.insert_text_file(&TextFileRecord { id: None, file_id: id2, content: "{\"key\": 42}".into() }).unwrap();
    let m = storage.grep_text_filtered(r"42", Some("*.yaml"), None, 100, 0, 1_000_000).unwrap();
    assert_eq!(m.len(), 1); assert_eq!(m[0].path, "/a.yaml");
}

#[test]
fn test_grep_body_with_options_context() {
    let storage = Storage::open_in_memory().unwrap();
    let file_id = storage.upsert_file(&make_file_full("/code.py", "python", 30)).unwrap();
    let mut fr = make_function(file_id, "do_thing");
    fr.line_start = 10; fr.line_end = 14;
    fr.body = "def do_thing():\n    target = 1\n    other = 2\n    return target".to_string();
    storage.insert_functions(&[fr]).unwrap();
    let m = storage.grep_body_with_options(Some("target"), None, None, None, 50, 1, 1_000_000).unwrap();
    assert_eq!(m.len(), 1); assert!(!m[0].context.is_empty());
}

#[test]
fn test_get_path_by_file_id() {
    let storage = Storage::open_in_memory().unwrap();
    let id = storage.upsert_file(&make_file_full("/some/path.py", "python", 1)).unwrap();
    assert_eq!(storage.get_path_by_file_id(id).unwrap(), Some("/some/path.py".to_string()));
    assert_eq!(storage.get_path_by_file_id(99999).unwrap(), None);
}

// ── Phase 2 тесты ──────────────────────────────────────────────────────

#[test]
fn test_upsert_file_content_round_trip() {
    let storage = Storage::open_in_memory().unwrap();
    let file_id = storage.upsert_file(&make_file_full("/src/app.py", "python", 5)).unwrap();
    storage.upsert_file_content(file_id, "hello world", 1024).unwrap();
    assert_eq!(storage.read_file_content(file_id).unwrap(), Some((Some("hello world".to_string()), false)));
    assert!(storage.has_file_content(file_id).unwrap());
}

#[test]
fn test_upsert_file_content_oversize() {
    let storage = Storage::open_in_memory().unwrap();
    let file_id = storage.upsert_file(&make_file_full("/big.py", "python", 1000)).unwrap();
    storage.upsert_file_content(file_id, &"x".repeat(100), 50).unwrap();
    assert_eq!(storage.read_file_content(file_id).unwrap(), Some((None, true)));
}

#[test]
fn test_upsert_file_content_idempotent_replace() {
    let storage = Storage::open_in_memory().unwrap();
    let file_id = storage.upsert_file(&make_file_full("/mod.py", "python", 10)).unwrap();
    storage.upsert_file_content(file_id, "first content", 4096).unwrap();
    storage.upsert_file_content(file_id, "second content", 4096).unwrap();
    assert_eq!(storage.read_file_content(file_id).unwrap(), Some((Some("second content".to_string()), false)));
}

#[test]
fn test_read_file_content_missing_returns_none() {
    let storage = Storage::open_in_memory().unwrap();
    let file_id = storage.upsert_file(&make_file_full("/norecord.py", "python", 5)).unwrap();
    assert!(storage.read_file_content(file_id).unwrap().is_none());
    assert!(!storage.has_file_content(file_id).unwrap());
}

#[test]
fn test_delete_file_content_removes_entry() {
    let storage = Storage::open_in_memory().unwrap();
    let file_id = storage.upsert_file(&make_file_full("/del.py", "python", 3)).unwrap();
    storage.upsert_file_content(file_id, "some code", 4096).unwrap();
    storage.delete_file_content(file_id).unwrap();
    assert!(storage.read_file_content(file_id).unwrap().is_none());
    assert!(!storage.has_file_content(file_id).unwrap());
}

#[test]
fn test_get_file_id_by_path_found_and_missing() {
    let storage = Storage::open_in_memory().unwrap();
    let id = storage.upsert_file(&make_file_full("/exists.py", "python", 1)).unwrap();
    assert_eq!(storage.get_file_id_by_path("/exists.py").unwrap(), Some(id));
    assert!(storage.get_file_id_by_path("/missing.py").unwrap().is_none());
}

#[test]
fn test_has_text_file_true_for_text_files() {
    let storage = Storage::open_in_memory().unwrap();
    let text_id = storage.upsert_file(&make_file_full("/readme.md", "markdown", 10)).unwrap();
    let code_id = storage.upsert_file(&make_file_full("/lib.rs", "rust", 20)).unwrap();
    storage.insert_text_file(&TextFileRecord { id: None, file_id: text_id, content: "# README\n".into() }).unwrap();
    assert!(storage.has_text_file(text_id).unwrap());
    assert!(!storage.has_text_file(code_id).unwrap());
}

#[test]
fn test_read_file_text_for_code_returns_decoded() {
    let storage = Storage::open_in_memory().unwrap();
    let file_id = storage.upsert_file(&make_file_full("/src/utils.py", "python", 3)).unwrap();
    let source = "def hello():\n    pass\n# конец";
    storage.upsert_file_content(file_id, source, 4096).unwrap();
    let r = storage.read_file_text("/src/utils.py", None, None, 1000, 1_000_000, 10_000_000, None).unwrap().unwrap();
    assert_eq!(r.category, "code"); assert_eq!(r.content, source); assert!(!r.oversize);
}

#[test]
fn test_read_file_text_for_code_oversize_returns_hint() {
    let storage = Storage::open_in_memory().unwrap();
    let file_id = storage.upsert_file(&make_file_full("/huge.bsl", "bsl", 500)).unwrap();
    storage.update_file_metadata("/huge.bsl", 1714305600, 200).unwrap();
    storage.upsert_file_content(file_id, &"a".repeat(100), 50).unwrap();
    let r = storage.read_file_text("/huge.bsl", None, None, 1000, 1_000_000, 10_000_000, Some(50)).unwrap().unwrap();
    assert!(r.oversize);
    let hint = r.hint.expect("hint должен быть");
    assert!(hint.contains("200") && hint.contains("50"), "hint: {hint}");
}

#[test]
fn test_read_file_text_for_code_no_record_returns_transitional_hint() {
    let storage = Storage::open_in_memory().unwrap();
    storage.upsert_file(&make_file_full("/old.py", "python", 20)).unwrap();
    let r = storage.read_file_text("/old.py", None, None, 1000, 1_000_000, 10_000_000, None).unwrap().unwrap();
    assert_eq!(r.category, "code"); assert!(!r.oversize);
    assert!(r.hint.expect("hint").to_lowercase().contains("backfill"));
}

#[test]
fn test_stat_file_for_code_with_oversize() {
    let storage = Storage::open_in_memory().unwrap();
    let file_id = storage.upsert_file(&make_file_full("/heavy.rs", "rust", 200)).unwrap();
    storage.upsert_file_content(file_id, &"r".repeat(100), 10).unwrap();
    let r = storage.stat_file_meta("/heavy.rs").unwrap();
    assert_eq!(r.oversize, Some(true));
}

#[test]
fn test_stat_file_for_code_normal_oversize_false() {
    let storage = Storage::open_in_memory().unwrap();
    let file_id = storage.upsert_file(&make_file_full("/small.rs", "rust", 10)).unwrap();
    storage.upsert_file_content(file_id, "fn main() {}", 4096).unwrap();
    let r = storage.stat_file_meta("/small.rs").unwrap();
    assert_eq!(r.oversize, Some(false));
}

#[test]
fn test_stat_file_for_text_no_oversize() {
    let storage = Storage::open_in_memory().unwrap();
    let file_id = storage.upsert_file(&make_file_full("/config.yaml", "yaml", 20)).unwrap();
    storage.insert_text_file(&TextFileRecord { id: None, file_id, content: "key: value\n".into() }).unwrap();
    let r = storage.stat_file_meta("/config.yaml").unwrap();
    assert!(r.oversize.is_none());
}

#[test]
fn test_grep_code_finds_pattern() {
    let storage = Storage::open_in_memory().unwrap();
    let id1 = storage.upsert_file(&make_file_full("/a.py", "python", 3)).unwrap();
    let id2 = storage.upsert_file(&make_file_full("/b.py", "python", 3)).unwrap();
    storage.upsert_file_content(id1, "def foo():\n    specific_word\n", 4096).unwrap();
    storage.upsert_file_content(id2, "def bar():\n    nothing_here\n", 4096).unwrap();
    let m = storage.grep_code_filtered("specific_word", None, None, 100, 0, 1_000_000).unwrap();
    assert_eq!(m.len(), 1); assert_eq!(m[0].path, "/a.py");
}

#[test]
fn test_grep_code_skips_oversize() {
    let storage = Storage::open_in_memory().unwrap();
    let id1 = storage.upsert_file(&make_file_full("/normal.py", "python", 3)).unwrap();
    let id2 = storage.upsert_file(&make_file_full("/giant.py", "python", 10000)).unwrap();
    storage.upsert_file_content(id1, "TARGET_PATTERN in normal file", 4096).unwrap();
    storage.upsert_file_content(id2, "TARGET_PATTERN in oversize", 1).unwrap();
    let m = storage.grep_code_filtered("TARGET_PATTERN", None, None, 100, 0, 1_000_000).unwrap();
    assert_eq!(m.len(), 1); assert_eq!(m[0].path, "/normal.py");
}

#[test]
fn test_grep_code_path_glob_filter() {
    let storage = Storage::open_in_memory().unwrap();
    let id1 = storage.upsert_file(&make_file_full("/src/match.py", "python", 2)).unwrap();
    let id2 = storage.upsert_file(&make_file_full("/test/no_match.py", "python", 2)).unwrap();
    storage.upsert_file_content(id1, "NEEDLE found here", 4096).unwrap();
    storage.upsert_file_content(id2, "NEEDLE found here too", 4096).unwrap();
    let m = storage.grep_code_filtered("NEEDLE", Some("/src/*"), None, 100, 0, 1_000_000).unwrap();
    assert_eq!(m.len(), 1); assert_eq!(m[0].path, "/src/match.py");
}

#[test]
fn test_grep_code_context_lines() {
    let storage = Storage::open_in_memory().unwrap();
    let file_id = storage.upsert_file(&make_file_full("/ctx.py", "python", 5)).unwrap();
    storage.upsert_file_content(file_id, "before1\nbefore2\nMATCH\nafter1\nafter2", 4096).unwrap();
    let m = storage.grep_code_filtered("MATCH", None, None, 100, 1, 1_000_000).unwrap();
    assert_eq!(m.len(), 1); assert_eq!(m[0].line, 3); assert_eq!(m[0].context.len(), 3);
}

#[test]
fn test_grep_code_respects_limit() {
    let storage = Storage::open_in_memory().unwrap();
    for i in 0..5 {
        let path = format!("/file{}.py", i);
        let file_id = storage.upsert_file(&make_file_full(&path, "python", 1)).unwrap();
        storage.upsert_file_content(file_id, &format!("COMMON_PATTERN in file {}", i), 4096).unwrap();
    }
    let m = storage.grep_code_filtered("COMMON_PATTERN", None, None, 2, 0, 1_000_000).unwrap();
    assert_eq!(m.len(), 2);
}

#[test]
fn test_migrate_v4_idempotent() {
    use crate::storage::schema;
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    conn.execute_batch("PRAGMA foreign_keys=ON;").unwrap();
    schema::initialize(&conn).unwrap();
    schema::migrate_v4(&conn).unwrap();
    conn.execute("INSERT INTO files (path, content_hash, language) VALUES ('/t.py', 'h', 'python')", []).unwrap();
    let file_id: i64 = conn.query_row("SELECT id FROM files WHERE path = '/t.py'", [], |r| r.get(0)).unwrap();
    conn.execute("INSERT INTO file_contents (file_id, content_blob, oversize) VALUES (?1, NULL, 1)", rusqlite::params![file_id]).unwrap();
    let oversize: i64 = conn.query_row("SELECT oversize FROM file_contents WHERE file_id = ?1", rusqlite::params![file_id], |r| r.get(0)).unwrap();
    assert_eq!(oversize, 1);
}
