//! OOP-модель из индекса: иерархия типов + методы, для inheritance-aware анализа.
//!
//! Строится на лету из таблиц `classes` (имя + `bases`) и `functions`
//! (`qualified_name = Class::method`). Не требует изменений схемы и записи —
//! резолвится в момент запроса, поэтому всегда согласована с индексом.
//!
//! Зачем: голый call-граф по имени не знает, что `PhpParser::file_extensions`
//! реализует `LanguageParser::file_extensions` и вызывается полиморфно. Модель
//! даёт ответы «кого этот метод переопределяет / реализует» и «кто переопределяет
//! его» — это убирает ложный «мёртвый код» и повышает точность анализа.

use std::collections::{HashMap, HashSet, VecDeque};

use anyhow::Result;

use super::Storage;

/// Иерархия типов + карта методов, построенная из индекса.
pub struct OopModel {
    /// Нормализованное имя типа → прямые базовые типы (extends/implements/супертрейты).
    bases: HashMap<String, Vec<String>>,
    /// Нормализованное имя типа → прямые наследники/реализации (реверс `bases`).
    children: HashMap<String, Vec<String>>,
    /// Нормализованное имя типа → множество объявленных в нём методов.
    methods: HashMap<String, HashSet<String>>,
}

/// Срезать префикс вида `interface `/`trait `/`enum `/`class ` (PHP пишет их в
/// `classes.name`), чтобы имя совпадало с тем, как тип упомянут в `bases` и
/// `qualified_name`.
fn norm_class(name: &str) -> &str {
    for p in ["interface ", "trait ", "enum ", "class ", "abstract "] {
        if let Some(rest) = name.strip_prefix(p) {
            return rest.trim();
        }
    }
    name.trim()
}

/// Разобрать поле `bases` в список имён базовых типов.
///   * PHP: `extends BaseController implements Foo, Bar` → [BaseController, Foo, Bar]
///   * простое `BaseRepository` → [BaseRepository]
///   * Rust-маркеры `trait`/`enum` (не отношения) → отфильтровываются
fn parse_bases(bases: &str) -> Vec<String> {
    bases
        .split(|c: char| !(c.is_alphanumeric() || c == '_' || c == '\\'))
        .filter(|t| !t.is_empty())
        .filter(|t| {
            !matches!(
                *t,
                "extends" | "implements" | "trait" | "enum" | "class" | "interface" | "use"
            )
        })
        .map(|t| t.rsplit('\\').next().unwrap_or(t).to_string())
        // Тип-подобные имена: начинаются с заглавной (отсекает ключевые слова/дженерики низом).
        .filter(|t| t.chars().next().map(|c| c.is_uppercase()).unwrap_or(false))
        .collect()
}

impl OopModel {
    /// Прямые предки нет — транзитивное замыкание базовых типов (с защитой от циклов).
    fn ancestors(&self, class: &str) -> Vec<String> {
        let mut out = Vec::new();
        let mut seen = HashSet::new();
        let mut q = VecDeque::new();
        seen.insert(class.to_string());
        if let Some(bs) = self.bases.get(class) {
            for b in bs {
                if seen.insert(b.clone()) {
                    q.push_back(b.clone());
                }
            }
        }
        while let Some(c) = q.pop_front() {
            out.push(c.clone());
            if let Some(bs) = self.bases.get(&c) {
                for b in bs {
                    if seen.insert(b.clone()) {
                        q.push_back(b.clone());
                    }
                }
            }
        }
        out
    }

    /// Транзитивные потомки (кто наследует/реализует этот тип).
    fn descendants(&self, class: &str) -> Vec<String> {
        let mut out = Vec::new();
        let mut seen = HashSet::new();
        let mut q = VecDeque::new();
        seen.insert(class.to_string());
        if let Some(cs) = self.children.get(class) {
            for c in cs {
                if seen.insert(c.clone()) {
                    q.push_back(c.clone());
                }
            }
        }
        while let Some(c) = q.pop_front() {
            out.push(c.clone());
            if let Some(cs) = self.children.get(&c) {
                for ch in cs {
                    if seen.insert(ch.clone()) {
                        q.push_back(ch.clone());
                    }
                }
            }
        }
        out
    }

    fn class_has_method(&self, class: &str, method: &str) -> bool {
        self.methods.get(class).map(|m| m.contains(method)).unwrap_or(false)
    }

    /// Известен ли тип модели (есть ли он среди `classes`).
    pub fn knows_class(&self, class: &str) -> bool {
        self.bases.contains_key(class) || self.methods.contains_key(class)
    }

    /// `Class::method` переопределяет/реализует метод предка → вызывается
    /// полиморфно, «мёртвым» считаться не может.
    pub fn is_override(&self, class: &str, method: &str) -> bool {
        self.ancestors(class)
            .iter()
            .any(|a| self.class_has_method(a, method))
    }

    /// Предковые объявления метода (`Ancestor::method`) — что переопределяет/реализует.
    pub fn overrides_of(&self, class: &str, method: &str) -> Vec<String> {
        self.ancestors(class)
            .into_iter()
            .filter(|a| self.class_has_method(a, method))
            .map(|a| format!("{}::{}", a, method))
            .collect()
    }

    /// MRO-резолв: класс символа или ближайший предок, где объявлен метод.
    /// `None` — ни сам класс, ни его предки метод не объявляют.
    pub fn resolve_method(&self, class: &str, method: &str) -> Option<String> {
        if self.class_has_method(class, method) {
            return Some(class.to_string());
        }
        self.ancestors(class)
            .into_iter()
            .find(|a| self.class_has_method(a, method))
    }

    /// Резолв ТОЛЬКО по предкам (минуя сам класс) — для `parent::method()`:
    /// даже если класс переопределяет метод, `parent::` обращается к родителю.
    pub fn resolve_in_ancestors(&self, class: &str, method: &str) -> Option<String> {
        self.ancestors(class)
            .into_iter()
            .find(|a| self.class_has_method(a, method))
    }

    /// Потомки, переопределяющие этот метод (`Descendant::method`).
    pub fn overridden_by(&self, class: &str, method: &str) -> Vec<String> {
        self.descendants(class)
            .into_iter()
            .filter(|d| self.class_has_method(d, method))
            .map(|d| format!("{}::{}", d, method))
            .collect()
    }
}

impl Storage {
    /// Построить OOP-модель из текущего индекса (classes + functions).
    pub fn build_oop_model(&self) -> Result<OopModel> {
        let mut bases: HashMap<String, Vec<String>> = HashMap::new();
        let mut children: HashMap<String, Vec<String>> = HashMap::new();
        let mut methods: HashMap<String, HashSet<String>> = HashMap::new();

        // Иерархия из classes.
        let mut stmt = self
            .conn
            .prepare("SELECT name, bases FROM classes WHERE bases IS NOT NULL AND bases != ''")?;
        let rows = stmt.query_map([], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
        })?;
        for row in rows {
            let (name, raw_bases) = row?;
            let cls = norm_class(&name).to_string();
            let parsed = parse_bases(&raw_bases);
            for b in &parsed {
                children.entry(b.clone()).or_default().push(cls.clone());
            }
            if !parsed.is_empty() {
                bases.entry(cls).or_default().extend(parsed);
            }
        }

        // Методы из functions.qualified_name (Class::method).
        let mut stmt = self
            .conn
            .prepare("SELECT qualified_name FROM functions WHERE qualified_name LIKE '%::%'")?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
        for qn in rows {
            let qn = qn?;
            if let Some(pos) = qn.rfind("::") {
                let cls = norm_class(&qn[..pos]).to_string();
                let method = qn[pos + 2..].to_string();
                if !method.is_empty() {
                    methods.entry(cls).or_default().insert(method);
                }
            }
        }

        // Сигнатуры зависимостей (vendor/node_modules) — наследование от фреймворка.
        // Толерантны к отсутствию таблиц (readonly-БД до v8 без миграции): skip.
        if let Ok(mut stmt) = self.conn.prepare("SELECT name, bases FROM ext_classes") {
            if let Ok(rows) =
                stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?)))
            {
                for row in rows.flatten() {
                    let (name, raw_bases) = row;
                    let cls = norm_class(&name).to_string();
                    if let Some(b) = raw_bases {
                        let parsed = parse_bases(&b);
                        for base in &parsed {
                            children.entry(base.clone()).or_default().push(cls.clone());
                        }
                        if !parsed.is_empty() {
                            bases.entry(cls).or_default().extend(parsed);
                        }
                    }
                }
            }
        }
        if let Ok(mut stmt) = self.conn.prepare("SELECT class, method FROM ext_methods") {
            if let Ok(rows) =
                stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
            {
                for row in rows.flatten() {
                    let (cls, m) = row;
                    methods.entry(norm_class(&cls).to_string()).or_default().insert(m);
                }
            }
        }

        Ok(OopModel { bases, children, methods })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn model(classes: &[(&str, &str)], qnames: &[&str]) -> OopModel {
        let storage = Storage::open_in_memory().unwrap();
        for (i, (name, bases)) in classes.iter().enumerate() {
            storage
                .conn
                .execute(
                    "INSERT INTO files (id, path, content_hash, language) VALUES (?1, ?2, 'h', 'php')",
                    rusqlite::params![i as i64 + 1, format!("f{i}.php")],
                )
                .ok();
            storage
                .conn
                .execute(
                    "INSERT INTO classes (file_id, name, bases) VALUES (?1, ?2, ?3)",
                    rusqlite::params![i as i64 + 1, name, bases],
                )
                .unwrap();
        }
        for qn in qnames {
            storage
                .conn
                .execute(
                    "INSERT INTO functions (file_id, name, qualified_name) VALUES (1, ?1, ?2)",
                    rusqlite::params![qn.rsplit("::").next().unwrap(), qn],
                )
                .unwrap();
        }
        storage.build_oop_model().unwrap()
    }

    #[test]
    fn detects_interface_implementation_as_override() {
        // PhpParser implements LanguageParser; оба объявляют file_extensions.
        let m = model(
            &[
                ("interface LanguageParser", "interface"),
                ("PhpParser", "implements LanguageParser"),
            ],
            &["LanguageParser::file_extensions", "PhpParser::file_extensions"],
        );
        assert!(m.is_override("PhpParser", "file_extensions"),
                "реализация интерфейсного метода — это override");
        assert_eq!(m.overrides_of("PhpParser", "file_extensions"),
                   vec!["LanguageParser::file_extensions".to_string()]);
        assert!(!m.is_override("PhpParser", "private_helper_not_in_iface"));
    }

    #[test]
    fn transitive_parent_override_and_reverse() {
        // A <- B <- C, метод handle объявлен в A и C.
        let m = model(
            &[
                ("A", ""),
                ("B", "extends A"),
                ("C", "extends B"),
            ],
            &["A::handle", "C::handle"],
        );
        assert!(m.is_override("C", "handle"), "C::handle переопределяет A::handle транзитивно");
        assert_eq!(m.overrides_of("C", "handle"), vec!["A::handle".to_string()]);
        // обратное: A::handle переопределяется в C
        assert_eq!(m.overridden_by("A", "handle"), vec!["C::handle".to_string()]);
        assert!(!m.is_override("A", "handle"), "у A нет предков с handle");
        // MRO-резолв: C::handle резолвится в сам C (own); B::handle — в предка A (inherited).
        assert_eq!(m.resolve_method("C", "handle").as_deref(), Some("C"));
        assert_eq!(m.resolve_method("B", "handle").as_deref(), Some("A"));
        assert_eq!(m.resolve_method("C", "nonexistent"), None);
        // parent::handle из C минует сам C (хоть C и объявляет handle) → предок A.
        assert_eq!(m.resolve_in_ancestors("C", "handle").as_deref(), Some("A"));
        assert_eq!(m.resolve_in_ancestors("A", "handle"), None);
    }

    #[test]
    fn rust_kind_markers_are_not_inheritance() {
        // Rust пишет в bases вид декларации — это не отношение наследования.
        let m = model(&[("LanguageParser", "trait"), ("StorageMode", "enum")],
                      &["LanguageParser::parse"]);
        assert!(!m.is_override("LanguageParser", "parse"));
        assert!(m.overrides_of("LanguageParser", "parse").is_empty());
    }
}
