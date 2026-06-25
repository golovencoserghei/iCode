//! M3 PHP-parser integration test: index a temp dir with one `.php` file
//! covering a namespace + `use` imports, an interface, a class that `extends` a
//! base and `implements` the interface (with a method calling `$this->helper()`
//! and `Model::find()`), a free function, and a block of Laravel route
//! definitions. Verifies the code graph is populated and the qualified-name /
//! receiver / import-kind / route shapes hold, exercising parse → store → read
//! across the contract seam.

use std::fs;

use icode_core::model::Language;
use icode_core::traits::CodeReadStore;
use icode_engine::parse::parse_php;
use icode_engine::{index_path, SqliteCodeStore};

const SAMPLE: &str = r#"<?php

namespace App\Http\Controllers;

use App\Models\User;
use App\Support\{Helper, Logger as Log};
use Illuminate\Support\Facades\Route;

interface Renderable
{
    public function render(): string;
}

class UserController extends BaseController implements Renderable
{
    public function show(int $id): User
    {
        $this->helper();
        return Model::find($id);
    }

    public function render(): string
    {
        return 'ok';
    }

    private function helper(): void
    {
    }
}

function format_user(User $user): string
{
    return $user->name;
}

Route::get('/users/{id}', [UserController::class, 'show'])->name('users.show');
Route::post('/users', 'UserController@store');
Route::any('/ping', function () {
    return 'pong';
});
"#;

#[test]
fn index_php_builds_code_graph_and_routes() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    fs::write(root.join("sample.php"), SAMPLE).expect("write sample");

    let store = SqliteCodeStore::open(root).expect("open store");
    let stats = index_path(root, &store).expect("index");

    assert_eq!(stats.files_indexed, 1, "one .php file indexed");
    assert_eq!(stats.errors, 0, "no parse errors");

    let db = store.stats().expect("stats");
    assert_eq!(db.files, 1);
    // interface Renderable + class UserController (>= 2).
    assert!(db.classes >= 2, "expected classes >= 2 (interface + class), got {}", db.classes);
    // show + render + helper + format_user (>= 4).
    assert!(db.functions >= 4, "expected functions >= 4, got {}", db.functions);
    assert!(db.imports >= 1, "expected imports >= 1, got {}", db.imports);
    assert!(db.calls > 0, "expected call edges, got {}", db.calls);
    assert!(db.routes >= 2, "expected routes >= 2, got {}", db.routes);

    // The method resolves with a `Class::method` qualified name.
    let show = store
        .get_function("show", Some(Language::Php), true)
        .expect("get_function show")
        .expect("show present");
    assert_eq!(show.qualified_name, "UserController::show");

    // The class is present with both extends + implements as bases.
    let ctrl = store
        .get_class("UserController", Some(Language::Php), true)
        .expect("get_class UserController")
        .expect("UserController present");
    assert!(ctrl.bases.iter().any(|b| b == "BaseController"), "extends base: {:?}", ctrl.bases);
    assert!(ctrl.bases.iter().any(|b| b == "Renderable"), "implements interface: {:?}", ctrl.bases);

    // The interface is present as a class-like declaration.
    let iface = store
        .get_class("Renderable", Some(Language::Php), true)
        .expect("get_class Renderable")
        .expect("Renderable present");
    assert_eq!(iface.name, "Renderable");

    // Inspect the parser output directly for import kinds, the call receivers,
    // and the route shapes (store-level inspection of those is M-later).
    let parsed = parse_php(SAMPLE, "sample.php");

    // Imports: kind "use", grouped use flattened, alias captured.
    assert!(
        parsed.imports.iter().any(|i| i.kind == "use"),
        "expected a `use` import, got: {:?}",
        parsed.imports
    );
    let user_import = parsed
        .imports
        .iter()
        .find(|i| i.name.as_deref() == Some("User"))
        .expect("User import");
    assert_eq!(user_import.module, "App\\Models\\User");
    let log_import = parsed
        .imports
        .iter()
        .find(|i| i.alias.as_deref() == Some("Log"))
        .expect("aliased Logger import from a group");
    assert_eq!(log_import.module, "App\\Support\\Logger");

    // member-call `$this->helper()` with receiver `$this`.
    let helper_call = parsed
        .calls
        .iter()
        .find(|c| c.callee == "helper")
        .expect("$this->helper() call");
    assert_eq!(helper_call.caller, "UserController::show");
    assert_eq!(helper_call.receiver.as_deref(), Some("$this"));

    // scoped-call `Model::find()` with receiver class `Model`.
    let find_call = parsed
        .calls
        .iter()
        .find(|c| c.callee == "find")
        .expect("Model::find() call");
    assert_eq!(find_call.caller, "UserController::show");
    assert_eq!(find_call.receiver.as_deref(), Some("Model"));

    // Routes: array handler + string `Ctrl@method` handler.
    let get_route = parsed
        .routes
        .iter()
        .find(|r| r.method == "GET")
        .expect("a GET route");
    assert_eq!(get_route.route, "/users/{id}");
    assert_eq!(get_route.handler_class.as_deref(), Some("UserController"));
    assert_eq!(get_route.handler_method.as_deref(), Some("show"));
    assert_eq!(get_route.name.as_deref(), Some("users.show"), "->name() captured");

    let post_route = parsed
        .routes
        .iter()
        .find(|r| r.method == "POST")
        .expect("a POST route");
    assert_eq!(post_route.route, "/users");
    assert_eq!(post_route.handler_class.as_deref(), Some("UserController"));
    assert_eq!(post_route.handler_method.as_deref(), Some("store"));

    // `Route::any(...)` → method "ANY", closure handler ⇒ no class/method.
    let any_route = parsed
        .routes
        .iter()
        .find(|r| r.method == "ANY")
        .expect("an ANY route");
    assert_eq!(any_route.route, "/ping");
    assert!(any_route.handler_class.is_none(), "closure handler has no class");
}
