/// Инструменты глубокого анализа: транзитивный call-граф, реализации, мёртвый код.
use super::oop::OopModel;
use super::{normalize_glob, Storage};
use super::models::*;
use anyhow::Result;
use rusqlite::params;

impl Storage {
    /// Транзитивные вызыватели функции (BFS, защита от циклов).
    /// Возвращает плоский список с полем `depth` (1 = прямой caller).
    pub fn get_callers_transitive(
        &self,
        function_name: &str,
        max_depth: usize,
        language: Option<&str>,
    ) -> Result<Vec<CallTreeNode>> {
        use std::collections::{HashSet, VecDeque};
        let mut result: Vec<CallTreeNode> = Vec::new();
        let mut visited: HashSet<String> = HashSet::new();
        let mut queue: VecDeque<(String, usize)> = VecDeque::new();

        visited.insert(function_name.to_string());
        queue.push_back((function_name.to_string(), 0));

        while let Some((name, depth)) = queue.pop_front() {
            if depth >= max_depth { continue; }
            for call in self.get_callers(&name, language)? {
                let path = self.get_path_by_file_id(call.file_id)?.unwrap_or_default();
                result.push(CallTreeNode { name: call.caller.clone(), file_path: path, line: call.line, depth: depth + 1 });
                if !visited.contains(&call.caller) {
                    visited.insert(call.caller.clone());
                    queue.push_back((call.caller, depth + 1));
                }
            }
        }
        Ok(result)
    }

    /// Транзитивные вызываемые функции (BFS, защита от циклов).
    pub fn get_callees_transitive(
        &self,
        function_name: &str,
        max_depth: usize,
        language: Option<&str>,
    ) -> Result<Vec<CallTreeNode>> {
        use std::collections::{HashSet, VecDeque};
        let mut result: Vec<CallTreeNode> = Vec::new();
        let mut visited: HashSet<String> = HashSet::new();
        let mut queue: VecDeque<(String, usize)> = VecDeque::new();

        visited.insert(function_name.to_string());
        queue.push_back((function_name.to_string(), 0));

        while let Some((name, depth)) = queue.pop_front() {
            if depth >= max_depth { continue; }
            for call in self.get_callees(&name, language)? {
                let path = self.get_path_by_file_id(call.file_id)?.unwrap_or_default();
                result.push(CallTreeNode { name: call.callee.clone(), file_path: path, line: call.line, depth: depth + 1 });
                if !visited.contains(&call.callee) {
                    visited.insert(call.callee.clone());
                    queue.push_back((call.callee, depth + 1));
                }
            }
        }
        Ok(result)
    }

    /// Найти классы, наследующие / реализующие данный базовый класс или интерфейс.
    /// LIKE-поиск по полю `bases` с точным word-match в post-filter.
    pub fn get_implementations(
        &self,
        class_name: &str,
        language: Option<&str>,
    ) -> Result<Vec<ImplementationRecord>> {
        let like_pattern = format!("%{}%", class_name);
        let sql = match language {
            Some(_) =>
                "SELECT c.name, fi.path, c.line_start, c.line_end, c.bases, c.docstring
                 FROM classes c JOIN files fi ON fi.id = c.file_id
                 WHERE c.bases IS NOT NULL AND c.bases LIKE ?1 AND fi.language = ?2
                 ORDER BY fi.path, c.line_start",
            None =>
                "SELECT c.name, fi.path, c.line_start, c.line_end, c.bases, c.docstring
                 FROM classes c JOIN files fi ON fi.id = c.file_id
                 WHERE c.bases IS NOT NULL AND c.bases LIKE ?1
                 ORDER BY fi.path, c.line_start",
        };
        let row_mapper = |row: &rusqlite::Row| -> rusqlite::Result<ImplementationRecord> {
            Ok(ImplementationRecord {
                name:       row.get(0)?,
                file_path:  row.get(1)?,
                line_start: row.get::<_, i64>(2)? as usize,
                line_end:   row.get::<_, i64>(3)? as usize,
                bases:      row.get(4)?,
                docstring:  row.get(5)?,
            })
        };
        let raw: Vec<ImplementationRecord> = match language {
            Some(lang) => {
                let mut stmt = self.conn.prepare(sql)?;
                let result = stmt.query_map(params![like_pattern, lang], row_mapper)?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                result
            }
            None => {
                let mut stmt = self.conn.prepare(sql)?;
                let result = stmt.query_map(params![like_pattern], row_mapper)?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                result
            }
        };
        // Post-filter: точный word-match в списке bases (разделитель — запятая)
        let filtered = raw.into_iter().filter(|r| {
            r.bases.as_deref().map(|bases| {
                bases.split(',').map(|s| s.trim()).any(|token| {
                    token == class_name
                        || token.ends_with(&format!("::{}", class_name))
                        || token.ends_with(&format!("\\{}", class_name))
                        || token.ends_with(&format!("/{}", class_name))
                })
            }).unwrap_or(false)
        }).collect();
        Ok(filtered)
    }

    /// Сколько функций с данным именем определено в индексе. >1 → имя
    /// неоднозначно (provenance: рёбра call-графа по этому имени эвристичны).
    pub fn function_definition_count(&self, name: &str) -> usize {
        self.conn
            .query_row(
                "SELECT COUNT(*) FROM functions WHERE name = ?1",
                params![name],
                |r| r.get::<_, i64>(0),
            )
            .map(|n| n as usize)
            .unwrap_or(0)
    }

    /// Лёгкий резолв для provenance: (0 | 1 | 2=«≥2», qualified_name если ровно одно).
    /// НЕ тянет тела (в отличие от get_function_by_name) — дёшево на горячем пути.
    pub fn function_defs_lite(&self, name: &str) -> (usize, Option<String>) {
        let mut stmt = match self
            .conn
            .prepare("SELECT qualified_name FROM functions WHERE name = ?1 LIMIT 2")
        {
            Ok(s) => s,
            Err(_) => return (0, None),
        };
        let qns: Vec<Option<String>> = match stmt.query_map(params![name], |r| r.get::<_, Option<String>>(0)) {
            Ok(it) => it.filter_map(Result::ok).collect(),
            Err(_) => return (0, None),
        };
        let count = qns.len();
        let qn = if count == 1 { qns.into_iter().next().flatten() } else { None };
        (count, qn)
    }

    /// Найти УЖЕ СУЩЕСТВУЮЩИЕ символы, похожие на запрос — чтобы агент не писал
    /// дубль того, что уже реализовано. Токенный матч (camelCase/snake_case + слова
    /// из docstring), без эмбеддингов. kind: "function"|"class"|"all" (по умолч. all).
    pub fn find_existing(
        &self,
        query: &str,
        kind: Option<&str>,
        language: Option<&str>,
        limit: usize,
    ) -> Result<Vec<ExistingMatch>> {
        let qtokens: std::collections::HashSet<String> = tokenize(query).into_iter().collect();
        if qtokens.is_empty() {
            return Ok(Vec::new());
        }
        if let Some(k) = kind {
            if !matches!(k, "function" | "class" | "all") {
                return Err(anyhow::anyhow!(
                    "find_existing: kind должен быть function|class|all (получено '{}')",
                    k
                ));
            }
        }
        let qnorm: String = query.to_lowercase().chars().filter(|c| c.is_alphanumeric()).collect();
        let want_fn = kind.map(|k| k == "function" || k == "all").unwrap_or(true);
        let want_cls = kind.map(|k| k == "class" || k == "all").unwrap_or(true);

        // Скоринг: токены имени (вес 3) / docstring (вес 1) + бонус за подстроку имени
        // (только для имён ≥3 символов — иначе `id`/`is` шумят на любом запросе).
        let score_one = |name: &str, qn: Option<&str>, doc: &str| -> i64 {
            let mut name_tokens: std::collections::HashSet<String> = tokenize(name).into_iter().collect();
            if let Some(q) = qn {
                for t in tokenize(q) {
                    name_tokens.insert(t);
                }
            }
            let doc_tokens: std::collections::HashSet<String> = tokenize(doc).into_iter().collect();
            let mut score = 0i64;
            for t in &qtokens {
                if name_tokens.contains(t) {
                    score += 3;
                } else if doc_tokens.contains(t) {
                    score += 1;
                }
            }
            let nnorm: String = name.to_lowercase().chars().filter(|c| c.is_alphanumeric()).collect();
            if nnorm.len() >= 3 && (nnorm.contains(&qnorm) || qnorm.contains(&nnorm)) {
                score += 4;
            }
            score
        };

        // Копим (кандидат, file_id) и резолвим путь ТОЛЬКО для топ-N после усечения
        // (иначе на частом токене — тысячи лишних SELECT path по совпадениям).
        let mut out: Vec<(ExistingMatch, i64)> = Vec::new();
        let lang_clause = if language.is_some() { " JOIN files fi ON fi.id = t.file_id WHERE fi.language = ?1" } else { "" };

        if want_fn {
            let sql = format!(
                "SELECT t.name, t.qualified_name, t.args, substr(COALESCE(t.docstring,''),1,240), t.file_id, t.line_start FROM functions t{}",
                lang_clause
            );
            let mut stmt = self.conn.prepare(&sql)?;
            let map_row = |r: &rusqlite::Row| -> rusqlite::Result<(String, Option<String>, Option<String>, String, i64, i64)> {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?))
            };
            let rows: Vec<_> = if let Some(l) = language {
                stmt.query_map(params![l], map_row)?.collect::<rusqlite::Result<Vec<_>>>()?
            } else {
                stmt.query_map([], map_row)?.collect::<rusqlite::Result<Vec<_>>>()?
            };
            for (name, qn, args, doc, file_id, line) in rows {
                let score = score_one(&name, qn.as_deref(), &doc);
                if score > 0 {
                    out.push((
                        ExistingMatch {
                            kind: "function".to_string(),
                            name,
                            qualified_name: qn,
                            file_path: String::new(),
                            line: line as usize,
                            signature: args.filter(|s| !s.is_empty()),
                            doc: (!doc.is_empty()).then_some(doc),
                            score,
                        },
                        file_id,
                    ));
                }
            }
        }

        if want_cls {
            let sql = format!(
                "SELECT t.name, substr(COALESCE(t.docstring,''),1,240), t.file_id, t.line_start FROM classes t{}",
                lang_clause
            );
            let mut stmt = self.conn.prepare(&sql)?;
            let map_row = |r: &rusqlite::Row| -> rusqlite::Result<(String, String, i64, i64)> {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
            };
            let rows: Vec<_> = if let Some(l) = language {
                stmt.query_map(params![l], map_row)?.collect::<rusqlite::Result<Vec<_>>>()?
            } else {
                stmt.query_map([], map_row)?.collect::<rusqlite::Result<Vec<_>>>()?
            };
            for (name, doc, file_id, line) in rows {
                let score = score_one(&name, None, &doc);
                if score > 0 {
                    out.push((
                        ExistingMatch {
                            kind: "class".to_string(),
                            name,
                            qualified_name: None,
                            file_path: String::new(),
                            line: line as usize,
                            signature: None,
                            doc: (!doc.is_empty()).then_some(doc),
                            score,
                        },
                        file_id,
                    ));
                }
            }
        }

        out.sort_by(|a, b| b.0.score.cmp(&a.0.score).then(a.0.name.cmp(&b.0.name)));
        out.truncate(limit);
        Ok(out
            .into_iter()
            .map(|(mut m, file_id)| {
                m.file_path = self.get_path_by_file_id(file_id).ok().flatten().unwrap_or_default();
                m
            })
            .collect())
    }

    /// Класс функции по (имя, file_id) из qualified_name (`Class::method`).
    fn class_of_fn(&self, fn_name: &str, file_id: i64) -> Option<String> {
        self.conn
            .query_row(
                "SELECT qualified_name FROM functions WHERE name = ?1 AND file_id = ?2 LIMIT 1",
                params![fn_name, file_id],
                |r| r.get::<_, Option<String>>(0),
            )
            .ok()
            .flatten()
            .and_then(|qn| qn.rsplit_once("::").map(|(c, _)| c.to_string()))
    }

    /// Резолв цели вызова по получателю + классу вызывающего + ООП-иерархии.
    /// Возвращает (resolution, target_class). Тот же принцип, что в get_symbol_context.
    fn resolve_call_target(
        &self,
        oop: Option<&OopModel>,
        source_class: Option<&str>,
        receiver: Option<&str>,
        callee: &str,
    ) -> (String, Option<String>) {
        if let Some(o) = oop {
            let (target, ancestors_only): (Option<String>, bool) = match receiver {
                Some("$this") | Some("self") | Some("static") => (source_class.map(str::to_string), false),
                Some("parent") => (source_class.map(str::to_string), true),
                Some(r) if !r.starts_with('$') && o.knows_class(r) => (Some(r.to_string()), false),
                _ => (None, false),
            };
            if let Some(cls) = target {
                let def = if ancestors_only {
                    o.resolve_in_ancestors(&cls, callee)
                } else {
                    o.resolve_method(&cls, callee)
                };
                if let Some(d) = def {
                    let kind = if d == cls { "own" } else { "inherited" };
                    return (kind.to_string(), Some(d));
                }
                // Получатель — известный класс, но метод в индексе не найден: цель всё равно класс.
                if receiver.map(|r| !r.starts_with('$')).unwrap_or(false) {
                    return ("by_name".to_string(), Some(cls));
                }
            }
        }
        match self.function_defs_lite(callee) {
            (1, qn) => ("exact".to_string(), qn.and_then(|q| q.rsplit_once("::").map(|(c, _)| c.to_string()))),
            (0, _) => (String::new(), None),
            _ => ("by_name".to_string(), None),
        }
    }

    /// Вызыватели функции с ООП-резолвом и опц. фильтром по целевому классу
    /// (`class` = «кто вызывает class::name»). Возвращает (рёбра, всего_до_фильтра, имя_неоднозначно).
    pub fn get_callers_resolved(
        &self,
        name: &str,
        class_filter: Option<&str>,
        language: Option<&str>,
        limit: usize,
    ) -> Result<(Vec<ResolvedCall>, usize, bool)> {
        let oop = self.build_oop_model().ok();
        let ambiguous = self.function_definition_count(name) > 1;
        let raw = self.get_callers(name, language)?;
        let total = raw.len();
        let mut out = Vec::new();
        for rec in raw {
            // Без class-фильтра незачем резолвить рёбра сверх лимита (на «толстых»
            // функциях это тысячи лишних точечных запросов). С фильтром — сканируем
            // всё, совпадений мало.
            if class_filter.is_none() && out.len() >= limit {
                break;
            }
            let src_class = self.class_of_fn(&rec.caller, rec.file_id);
            let (resolution, target) =
                self.resolve_call_target(oop.as_ref(), src_class.as_deref(), rec.receiver.as_deref(), name);
            if let Some(cf) = class_filter {
                if target.as_deref() != Some(cf) {
                    continue;
                }
            }
            let path = self.get_path_by_file_id(rec.file_id).ok().flatten().unwrap_or_default();
            out.push(ResolvedCall { name: rec.caller, file_path: path, line: rec.line, resolution, target_class: target });
        }
        Ok((out, total, ambiguous))
    }

    /// Вызываемые функцией с ООП-резолвом и опц. фильтром по классу-источнику
    /// (`class` = «что вызывает class::name» — какое из определений name брать).
    pub fn get_callees_resolved(
        &self,
        name: &str,
        class_filter: Option<&str>,
        language: Option<&str>,
        limit: usize,
    ) -> Result<(Vec<ResolvedCall>, usize, bool)> {
        let oop = self.build_oop_model().ok();
        let ambiguous = self.function_definition_count(name) > 1;
        let raw = self.get_callees(name, language)?;
        let total = raw.len();
        let mut out = Vec::new();
        for rec in raw {
            if class_filter.is_none() && out.len() >= limit {
                break;
            }
            let src_class = self.class_of_fn(name, rec.file_id);
            if let Some(cf) = class_filter {
                if src_class.as_deref() != Some(cf) {
                    continue;
                }
            }
            let (resolution, target) =
                self.resolve_call_target(oop.as_ref(), src_class.as_deref(), rec.receiver.as_deref(), &rec.callee);
            let path = self.get_path_by_file_id(rec.file_id).ok().flatten().unwrap_or_default();
            out.push(ResolvedCall { name: rec.callee, file_path: path, line: rec.line, resolution, target_class: target });
        }
        Ok((out, total, ambiguous))
    }

    /// Полный контекст символа за один вызов: definition + callers + callees +
    /// file_outline + file_imports + routes. При неоднозначном имени —
    /// kind="ambiguous" + candidates.
    pub fn get_symbol_context(
        &self,
        name: &str,
        file_hint: Option<&str>,
        language: Option<&str>,
    ) -> Result<SymbolContext> {
        let mut functions = self.get_function_by_name(name)?;
        let mut classes = self.get_class_by_name(name)?;

        if let Some(hint) = file_hint {
            let matcher = globset::Glob::new(&normalize_glob(hint)).ok().map(|g| g.compile_matcher());
            functions.retain(|f| {
                let path = self.get_path_by_file_id(f.file_id).ok().flatten().unwrap_or_default();
                if let Some(ref m) = matcher { m.is_match(&path) } else { path.contains(hint) }
            });
            classes.retain(|c| {
                let path = self.get_path_by_file_id(c.file_id).ok().flatten().unwrap_or_default();
                if let Some(ref m) = matcher { m.is_match(&path) } else { path.contains(hint) }
            });
        }

        let total = functions.len() + classes.len();
        if total == 0 {
            return Ok(SymbolContext {
                kind: "not_found".to_string(), candidates: vec![], definition: None,
                callers: vec![], callees: vec![], file_outline: None, file_imports: vec![],
                routes: vec![], inheritance: None,
            });
        }
        if total > 1 {
            let mut candidates = Vec::new();
            for f in &functions {
                let path = self.get_path_by_file_id(f.file_id)?.unwrap_or_default();
                candidates.push(SymbolCandidate { name: f.name.clone(), kind: "function".to_string(), file_path: path, line_start: f.line_start, qualified_name: f.qualified_name.clone() });
            }
            for c in &classes {
                let path = self.get_path_by_file_id(c.file_id)?.unwrap_or_default();
                candidates.push(SymbolCandidate { name: c.name.clone(), kind: "class".to_string(), file_path: path, line_start: c.line_start, qualified_name: None });
            }
            return Ok(SymbolContext {
                kind: "ambiguous".to_string(), candidates, definition: None,
                callers: vec![], callees: vec![], file_outline: None, file_imports: vec![],
                routes: vec![], inheritance: None,
            });
        }

        // route_lookup: для функции/метода — (handler_class, handler_method) для
        // поиска связанных веб-маршрутов. Класс берём из qualified_name (`Cls::method`).
        let (file_id, kind, definition, route_lookup) = if !functions.is_empty() {
            let f = functions.into_iter().next().unwrap();
            let handler_class = f.qualified_name.as_deref()
                .and_then(|qn| qn.rsplit_once("::").map(|(cls, _)| cls.to_string()));
            let lookup = Some((handler_class, f.name.clone()));
            (f.file_id, "function".to_string(), Some(serde_json::to_value(&f).unwrap_or_default()), lookup)
        } else {
            let c = classes.into_iter().next().unwrap();
            (c.file_id, "class".to_string(), Some(serde_json::to_value(&c).unwrap_or_default()), None)
        };

        // ООП-модель строим один раз — переиспользуем для резолва вызовов и inheritance.
        let oop = self.build_oop_model().ok();
        let self_class: Option<&str> = route_lookup.as_ref().and_then(|(c, _)| c.as_deref());

        let callers = self.get_callers(name, language)?.into_iter().take(30).map(|rec| {
            let path = self.get_path_by_file_id(rec.file_id).ok().flatten().unwrap_or_default();
            // Provenance вызывателя: уникально ли его имя в индексе.
            let resolution = match self.function_definition_count(&rec.caller) {
                1 => "exact",
                n if n > 1 => "by_name",
                _ => "",
            }.to_string();
            CallerInfo { caller: rec.caller, file_path: path, line: rec.line, resolution }
        }).collect();

        let callees = self.get_callees(name, language)?.into_iter().take(30).map(|rec| {
            let path = self.get_path_by_file_id(rec.file_id).ok().flatten().unwrap_or_default();
            // OOP-резолв по ПОЛУЧАТЕЛЮ (точно, без ложных «inherited»):
            //   * $this/self/static → класс символа → MRO (own/inherited);
            //   * parent → ТОЛЬКО предки (минуя свой класс), всегда inherited;
            //   * известное имя класса (Foo::m()) → класс Foo → MRO;
            //   * $other / None → НЕ резолвим по иерархии (нельзя утверждать, что это
            //     метод нашего класса) → падаем в резолв по уникальности имени.
            let oop_hit = oop.as_ref().and_then(|o| {
                // (целевой класс, искать только в предках?)
                let (target, ancestors_only): (Option<String>, bool) = match rec.receiver.as_deref() {
                    Some("$this") | Some("self") | Some("static") => (self_class.map(str::to_string), false),
                    Some("parent") => (self_class.map(str::to_string), true),
                    Some(r) if !r.starts_with('$') && o.knows_class(r) => (Some(r.to_string()), false),
                    _ => (None, false),
                };
                target.and_then(|cls| {
                    let def = if ancestors_only {
                        o.resolve_in_ancestors(&cls, &rec.callee)
                    } else {
                        o.resolve_method(&cls, &rec.callee)
                    };
                    def.map(|d| {
                        let kind = if d == cls { "own" } else { "inherited" };
                        (kind.to_string(), Some(format!("{}::{}", d, rec.callee)))
                    })
                })
            });
            let (resolution, resolved_to) = match oop_hit {
                Some(x) => x,
                None => match self.function_defs_lite(&rec.callee) {
                    (0, _) => (String::new(), None),       // внешний/builtin
                    (1, qn) => ("exact".to_string(), qn),  // уникальное имя
                    _ => ("by_name".to_string(), None),    // ≥2 определения
                },
            };
            CalleeInfo { callee: rec.callee, file_path: path, line: rec.line, resolution, resolved_to }
        }).collect();

        let file_path = self.get_path_by_file_id(file_id)?.unwrap_or_default();
        let file_outline = if file_path.is_empty() { None } else { self.get_file_outline(&file_path)? };
        let file_imports = self.get_imports_by_file(file_id)?;

        // Связанные веб-маршруты (framework-aware routing): маршруты, чей
        // хендлер — этот символ. Пусто если символ не контроллер-метод.
        let routes = match &route_lookup {
            Some((class, method)) => self
                .routes_for_handler(class.as_deref(), method)
                .unwrap_or_default(),
            None => vec![],
        };

        // ООП-контекст: для метода (Class::method) — кого переопределяет/реализует
        // и кто переопределяет его. Резолвится по иерархии типов из индекса.
        let inheritance = match (&route_lookup, oop.as_ref()) {
            (Some((Some(class), method)), Some(oop)) if oop.knows_class(class) => {
                let overrides = oop.overrides_of(class, method);
                let overridden_by = oop.overridden_by(class, method);
                if overrides.is_empty() && overridden_by.is_empty() {
                    None
                } else {
                    Some(InheritanceInfo { class: class.clone(), overrides, overridden_by })
                }
            }
            _ => None,
        };

        Ok(SymbolContext {
            kind, candidates: vec![], definition, callers, callees,
            file_outline, file_imports, routes, inheritance,
        })
    }

    /// Найти функции без callers в индексе (потенциально мёртвый код).
    /// Исключает тесты, конструкторы, точки входа. Результат приблизителен.
    pub fn find_dead_code(
        &self,
        limit: usize,
        path_glob: Option<&str>,
        language: Option<&str>,
    ) -> Result<Vec<DeadCodeEntry>> {
        let mut conds: Vec<String> = vec![
            "f.name NOT IN (SELECT DISTINCT callee FROM calls)".to_string(),
            "f.name NOT LIKE '__init__%'".to_string(),
            "f.name NOT LIKE 'test%'".to_string(),
            "f.name NOT LIKE '%_test'".to_string(),
            "f.name NOT IN ('main','run','start','execute','handle','setup','teardown','new','init','__init__','__new__','create','destroy','delete')".to_string(),
        ];
        // Роут-хендлеры достижимы через маршрут (не по имени) — НЕ мёртвый код.
        // Кросс-ссылка на таблицу routes; пропускаем условие, если её нет (старая readonly-БД).
        if self.conn.prepare("SELECT 1 FROM routes LIMIT 0").is_ok() {
            conds.push(
                "f.qualified_name NOT IN (SELECT handler_class || '::' || handler_method \
                 FROM routes WHERE handler_class IS NOT NULL AND handler_method IS NOT NULL)"
                    .to_string(),
            );
        }
        let mut params_dyn: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();
        if let Some(g) = path_glob {
            conds.push("fi.path GLOB ?".to_string());
            params_dyn.push(Box::new(normalize_glob(g)));
        }
        if let Some(l) = language {
            conds.push("fi.language = ?".to_string());
            params_dyn.push(Box::new(l.to_string()));
        }
        // Берём пул кандидатов с запасом: дальше отфильтруем ООП-переопределения,
        // поэтому до финального усечения нужно больше, чем limit.
        let fetch = (limit.saturating_mul(4)).max(200) as i64;
        params_dyn.push(Box::new(fetch));
        let sql = format!(
            "SELECT f.name, f.qualified_name, fi.path, f.line_start, f.line_end
             FROM functions f JOIN files fi ON fi.id = f.file_id
             WHERE {} ORDER BY fi.path, f.line_start LIMIT ?",
            conds.join(" AND ")
        );
        let params_refs: Vec<&dyn rusqlite::ToSql> =
            params_dyn.iter().map(|b| &**b as &dyn rusqlite::ToSql).collect();
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map(params_refs.as_slice(), |row| {
            Ok(DeadCodeEntry {
                name:           row.get(0)?,
                qualified_name: row.get(1)?,
                file_path:      row.get(2)?,
                line_start:     row.get::<_, i64>(3)? as usize,
                line_end:       row.get::<_, i64>(4)? as usize,
            })
        })?;
        let candidates: Vec<DeadCodeEntry> = rows.collect::<rusqlite::Result<Vec<_>>>()?;

        // ООП-фильтр: метод, который переопределяет/реализует метод предка
        // (включая интерфейс/трейт/абстрактный), вызывается полиморфно и НЕ может
        // быть мёртвым. Это убирает массовые ложные срабатывания на реализациях
        // интерфейсов (PhpParser::file_extensions, *Controller-методы и т.п.).
        let oop = self.build_oop_model()?;
        let filtered: Vec<DeadCodeEntry> = candidates
            .into_iter()
            .filter(|e| {
                match e.qualified_name.as_deref().and_then(|qn| qn.rsplit_once("::")) {
                    Some((class, method)) => !oop.is_override(class, method),
                    None => true, // свободная функция — ООП-фильтр не применим
                }
            })
            .take(limit)
            .collect();
        Ok(filtered)
    }
}

/// Разбить идентификатор/фразу на токены: границы camelCase, snake_case, не-буквы.
/// `sendWelcomeEmail`/`send_welcome_email`/`send welcome email` → [send, welcome, email].
/// Токены короче 2 символов отбрасываются.
pub(crate) fn tokenize(s: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut prev_lower_or_digit = false;
    for ch in s.chars() {
        if ch.is_alphanumeric() {
            if ch.is_uppercase() && prev_lower_or_digit && !cur.is_empty() {
                out.push(std::mem::take(&mut cur));
            }
            for lc in ch.to_lowercase() {
                cur.push(lc);
            }
            prev_lower_or_digit = ch.is_lowercase() || ch.is_numeric();
        } else {
            if !cur.is_empty() {
                out.push(std::mem::take(&mut cur));
            }
            prev_lower_or_digit = false;
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out.retain(|t| t.len() >= 2);
    out
}
