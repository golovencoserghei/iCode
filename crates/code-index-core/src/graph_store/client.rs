// Клиент Neo4j/Memgraph на базе neo4rs (Bolt-протокол).
// Совместим с обоими движками: схема создаётся «мягко» (ошибки игнорируются),
// потому что синтаксис CREATE CONSTRAINT/INDEX отличается между версиями.

use anyhow::Result;
use neo4rs::{query, Graph};

use super::GraphCommand;

pub struct GraphClient {
    graph: Graph,
}

impl GraphClient {
    pub async fn connect(bolt_url: &str, username: &str, password: &str) -> Result<Self> {
        let graph = Graph::new(bolt_url, username, password).await?;
        Ok(Self { graph })
    }

    /// Создать индексы/ограничения. Ошибки игнорируются — синтаксис
    /// различается между Neo4j и Memgraph, при отсутствии индексов
    /// запросы работают медленнее, но корректно.
    pub async fn ensure_schema(&self) {
        let stmts = [
            // Neo4j 5.x синтаксис
            "CREATE INDEX file_path_idx IF NOT EXISTS FOR (f:File) ON (f.repo, f.path)",
            "CREATE INDEX func_name_idx IF NOT EXISTS FOR (fn:Function) ON (fn.repo, fn.name)",
            "CREATE INDEX class_name_idx IF NOT EXISTS FOR (c:Class) ON (c.repo, c.name)",
            "CREATE INDEX module_name_idx IF NOT EXISTS FOR (m:Module) ON (m.repo, m.name)",
            "CREATE INDEX route_path_idx IF NOT EXISTS FOR (rt:Route) ON (rt.repo, rt.path)",
            // Memgraph синтаксис (старый Neo4j 3.x стиль)
            "CREATE INDEX ON :File(path)",
            "CREATE INDEX ON :Function(name)",
            "CREATE INDEX ON :Class(name)",
            "CREATE INDEX ON :Module(name)",
            "CREATE INDEX ON :Route(path)",
        ];
        for stmt in stmts {
            let _ = self.graph.run(query(stmt)).await;
        }
    }

    pub async fn handle_command(&self, cmd: GraphCommand) -> Result<()> {
        match cmd {
            GraphCommand::UpsertFile { repo, path, language, hash } => {
                self.graph
                    .run(
                        query(
                            "MERGE (f:File {repo: $repo, path: $path}) \
                             SET f.language = $lang, f.hash = $hash",
                        )
                        .param("repo", repo)
                        .param("path", path)
                        .param("lang", language)
                        .param("hash", hash),
                    )
                    .await?;
            }

            GraphCommand::UpsertFunction {
                repo,
                file_path,
                name,
                qualified_name,
                line_start,
                line_end,
            } => {
                let qname = qualified_name
                    .filter(|s| !s.is_empty())
                    .unwrap_or_else(|| format!("{}::{}", file_path, name));
                self.graph
                    .run(
                        query(
                            "MERGE (fn:Function {repo: $repo, qualified_name: $qn}) \
                             SET fn.name = $name, fn.file_path = $fp, \
                                 fn.line_start = $ls, fn.line_end = $le \
                             WITH fn \
                             MATCH (f:File {repo: $repo, path: $fp}) \
                             MERGE (fn)-[:DEFINED_IN]->(f)",
                        )
                        .param("repo", repo)
                        .param("qn", qname)
                        .param("name", name)
                        .param("fp", file_path)
                        .param("ls", line_start as i64)
                        .param("le", line_end as i64),
                    )
                    .await?;
            }

            GraphCommand::UpsertClass {
                repo,
                file_path,
                name,
                line_start,
                line_end,
                bases,
            } => {
                let qname = format!("{}::{}", file_path, name);
                self.graph
                    .run(
                        query(
                            "MERGE (c:Class {repo: $repo, qualified_name: $qn}) \
                             SET c.name = $name, c.file_path = $fp, \
                                 c.line_start = $ls, c.line_end = $le \
                             WITH c \
                             MATCH (f:File {repo: $repo, path: $fp}) \
                             MERGE (c)-[:DEFINED_IN]->(f)",
                        )
                        .param("repo", repo.clone())
                        .param("qn", qname)
                        .param("name", name.clone())
                        .param("fp", file_path.clone())
                        .param("ls", line_start as i64)
                        .param("le", line_end as i64),
                    )
                    .await?;

                if let Some(bases_str) = bases {
                    let child_qn = format!("{}::{}", file_path, name);
                    for base in bases_str
                        .split(',')
                        .map(|s| s.trim())
                        .filter(|s| !s.is_empty())
                    {
                        let _ = self
                            .graph
                            .run(
                                query(
                                    "MATCH (child:Class {repo: $repo, qualified_name: $child_qn}) \
                                     MERGE (parent:Class {repo: $repo, name: $pname}) \
                                     MERGE (child)-[:INHERITS]->(parent)",
                                )
                                .param("repo", repo.clone())
                                .param("child_qn", child_qn.clone())
                                .param("pname", base.to_string()),
                            )
                            .await;
                    }
                }
            }

            GraphCommand::AddCall { repo, file_path, caller, callee, line } => {
                let q_caller = format!("{}::{}", file_path, caller);
                self.graph
                    .run(
                        query(
                            "MERGE (caller:Function {repo: $repo, qualified_name: $q_caller}) \
                             SET caller.name = $caller_name, caller.file_path = $file_path \
                             WITH caller \
                             MERGE (callee:Function {repo: $repo, name: $callee}) \
                             MERGE (caller)-[:CALLS {line: $line}]->(callee)",
                        )
                        .param("repo", repo)
                        .param("q_caller", q_caller)
                        .param("caller_name", caller)
                        .param("file_path", file_path)
                        .param("callee", callee)
                        .param("line", line as i64),
                    )
                    .await?;
            }

            GraphCommand::AddImport { repo, file_path, module, kind } => {
                self.graph
                    .run(
                        query(
                            "MATCH (f:File {repo: $repo, path: $path}) \
                             MERGE (m:Module {repo: $repo, name: $module}) \
                             MERGE (f)-[:IMPORTS {kind: $kind}]->(m)",
                        )
                        .param("repo", repo)
                        .param("path", file_path)
                        .param("module", module)
                        .param("kind", kind),
                    )
                    .await?;
            }

            GraphCommand::AddRoute {
                repo,
                file_path,
                method,
                path,
                handler_class,
                handler_method,
                line,
            } => {
                self.graph
                    .run(
                        query(
                            "MERGE (rt:Route {repo: $repo, method: $method, path: $path}) \
                             SET rt.file_path = $fp, rt.line = $line, rt.handler_class = $hclass \
                             WITH rt \
                             MERGE (fn:Function {repo: $repo, name: $hmethod}) \
                             MERGE (rt)-[:HANDLED_BY]->(fn)",
                        )
                        .param("repo", repo)
                        .param("method", method)
                        .param("path", path)
                        .param("fp", file_path)
                        .param("line", line as i64)
                        .param("hclass", handler_class.unwrap_or_default())
                        .param("hmethod", handler_method),
                    )
                    .await?;
            }

            GraphCommand::DeleteFileData { repo, path } => {
                // Сначала чистим Route-узлы, определённые в этом файле (HANDLED_BY
                // снимется через DETACH). Отдельным запросом — основной delete ниже
                // оперирует Function/Class/Module.
                let _ = self
                    .graph
                    .run(
                        query(
                            "MATCH (rt:Route {repo: $repo, file_path: $path}) DETACH DELETE rt",
                        )
                        .param("repo", repo.clone())
                        .param("path", path.clone()),
                    )
                    .await;
                // Delete function nodes defined in this file:
                //   - Remove DEFINED_IN edge (outgoing from fn → file)
                //   - Remove outgoing CALLS edges  (fn → other)
                //   - Remove incoming CALLS edges  (other → fn) to avoid dangling refs
                //   - Delete the node itself
                // Same treatment for Class nodes (DEFINED_IN + outgoing/incoming INHERITS).
                // Finally prune Module nodes in this repo that lost all IMPORTS edges.
                self.graph
                    .run(
                        query(
                            "MATCH (f:File {repo: $repo, path: $path}) \
                             OPTIONAL MATCH (fn:Function)-[rd:DEFINED_IN]->(f) DELETE rd \
                             WITH f, collect(fn) AS fns \
                             UNWIND fns AS fn \
                             OPTIONAL MATCH (fn)-[rc_out:CALLS]->() DELETE rc_out \
                             WITH f, fn \
                             OPTIONAL MATCH ()-[rc_in:CALLS]->(fn) DELETE rc_in \
                             WITH f, fn DELETE fn \
                             WITH f \
                             OPTIONAL MATCH (c:Class)-[rcd:DEFINED_IN]->(f) DELETE rcd \
                             WITH f, collect(c) AS cs \
                             UNWIND cs AS c \
                             OPTIONAL MATCH (c)-[rci_out:INHERITS]->() DELETE rci_out \
                             WITH f, c \
                             OPTIONAL MATCH ()-[rci_in:INHERITS]->(c) DELETE rci_in \
                             WITH f, c DELETE c \
                             WITH f \
                             OPTIONAL MATCH (f)-[ri:IMPORTS]->() DELETE ri \
                             WITH f DELETE f \
                             WITH 1 AS dummy \
                             MATCH (m:Module {repo: $repo}) WHERE NOT ()-[:IMPORTS]->(m) DELETE m",
                        )
                        .param("repo", repo)
                        .param("path", path),
                    )
                    .await?;
            }
        }
        Ok(())
    }

    // ── Query-методы для MCP-инструментов ─────────────────────────────────

    pub async fn find_dependencies(
        &self,
        repo: &str,
        path: &str,
        depth: i64,
    ) -> Result<Vec<serde_json::Value>> {
        let depth = depth.clamp(1, 20);
        let mut stream = self
            .graph
            .execute(
                query(
                    "MATCH (f:File {repo: $repo, path: $path})-[:IMPORTS*1..$depth]->(dep) \
                     RETURN dep.name AS name, labels(dep)[0] AS kind",
                )
                .param("repo", repo.to_string())
                .param("path", path.to_string())
                .param("depth", depth),
            )
            .await?;

        let mut out = Vec::new();
        loop {
            match stream.next().await {
                Ok(Some(row)) => {
                    let name: String = row.get("name").unwrap_or_default();
                    let kind: String = row.get("kind").unwrap_or_else(|_| "Module".to_string());
                    out.push(serde_json::json!({ "name": name, "kind": kind }));
                }
                Ok(None) => break,
                Err(e) => return Err(anyhow::anyhow!("graph stream error: {}", e)),
            }
        }
        Ok(out)
    }

    pub async fn impact_analysis(
        &self,
        repo: &str,
        path: &str,
        depth: i64,
    ) -> Result<Vec<String>> {
        let depth = depth.clamp(1, 20);
        let mut stream = self
            .graph
            .execute(
                query(
                    "MATCH (dep:File)-[:IMPORTS*1..$depth]->(f:File {repo: $repo, path: $path}) \
                     RETURN DISTINCT dep.path AS path",
                )
                .param("repo", repo.to_string())
                .param("path", path.to_string())
                .param("depth", depth),
            )
            .await?;

        let mut out = Vec::new();
        loop {
            match stream.next().await {
                Ok(Some(row)) => {
                    let p: String = row.get("path").unwrap_or_default();
                    out.push(p);
                }
                Ok(None) => break,
                Err(e) => return Err(anyhow::anyhow!("graph stream error: {}", e)),
            }
        }
        Ok(out)
    }

    pub async fn get_call_chain(
        &self,
        repo: &str,
        from_fn: &str,
        to_fn: &str,
    ) -> Result<Option<Vec<String>>> {
        let mut stream = self
            .graph
            .execute(
                query(
                    "MATCH p = shortestPath( \
                       (a:Function {repo: $repo, name: $from})-[:CALLS*1..15]-> \
                       (b:Function {repo: $repo, name: $to}) \
                     ) \
                     RETURN [n IN nodes(p) | n.name] AS chain",
                )
                .param("repo", repo.to_string())
                .param("from", from_fn.to_string())
                .param("to", to_fn.to_string()),
            )
            .await?;

        match stream.next().await {
            Ok(Some(row)) => {
                let chain: Vec<String> = row.get("chain").unwrap_or_default();
                Ok(Some(chain))
            }
            Ok(None) => Ok(None),
            Err(e) => Err(anyhow::anyhow!("graph stream error: {}", e)),
        }
    }

    pub async fn get_callers_graph(
        &self,
        repo: &str,
        fn_name: &str,
        limit: i64,
    ) -> Result<Vec<serde_json::Value>> {
        let mut stream = self
            .graph
            .execute(
                query(
                    "MATCH (caller:Function {repo: $repo})-[:CALLS]-> \
                           (fn:Function {repo: $repo, name: $name}) \
                     RETURN caller.name AS name, \
                            coalesce(caller.file_path, '') AS file_path, \
                            coalesce(caller.qualified_name, caller.name) AS qualified_name \
                     LIMIT $limit",
                )
                .param("repo", repo.to_string())
                .param("name", fn_name.to_string())
                .param("limit", limit),
            )
            .await?;

        let mut out = Vec::new();
        loop {
            match stream.next().await {
                Ok(Some(row)) => {
                    let name: String = row.get("name").unwrap_or_default();
                    let file_path: String = row.get("file_path").unwrap_or_default();
                    let qualified_name: String = row.get("qualified_name").unwrap_or_default();
                    out.push(serde_json::json!({ "caller": name, "file": file_path, "qualified_name": qualified_name }));
                }
                Ok(None) => break,
                Err(e) => return Err(anyhow::anyhow!("graph stream error: {}", e)),
            }
        }
        Ok(out)
    }

    pub async fn get_callees_graph(
        &self,
        repo: &str,
        fn_name: &str,
        limit: i64,
    ) -> Result<Vec<serde_json::Value>> {
        let mut stream = self
            .graph
            .execute(
                query(
                    "MATCH (fn:Function {repo: $repo, name: $name})-[:CALLS]-> \
                           (callee:Function {repo: $repo}) \
                     RETURN callee.name AS name, \
                            coalesce(callee.file_path, '') AS file_path, \
                            coalesce(callee.qualified_name, callee.name) AS qualified_name \
                     LIMIT $limit",
                )
                .param("repo", repo.to_string())
                .param("name", fn_name.to_string())
                .param("limit", limit),
            )
            .await?;

        let mut out = Vec::new();
        loop {
            match stream.next().await {
                Ok(Some(row)) => {
                    let name: String = row.get("name").unwrap_or_default();
                    let file_path: String = row.get("file_path").unwrap_or_default();
                    let qualified_name: String = row.get("qualified_name").unwrap_or_default();
                    out.push(serde_json::json!({ "callee": name, "file": file_path, "qualified_name": qualified_name }));
                }
                Ok(None) => break,
                Err(e) => return Err(anyhow::anyhow!("graph stream error: {}", e)),
            }
        }
        Ok(out)
    }
}
