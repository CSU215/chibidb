//! The plan panel's data: what will run, why it was chosen, and what the names
//! in the statement resolved to.
//!
//! Three facts from three sources, deliberately kept apart on the wire:
//!
//! * `plan` (the tree) comes from [`PhysicalOperator::children`] -- the
//!   operators `build_select` just built. **This is what runs.**
//! * `explain` / `chosen` / `rejected` come from `exec/plan.rs`, the rule-based
//!   access-path choice. That file is a parallel implementation of the same
//!   decision (`docs/chibidb设计与实现.md` §11.1), so its output is labelled as
//!   the planner's *record*; rendering it as the plan would show a plan nobody
//!   runs whenever the two drift. `tests/plan_api.rs` asserts they agree.
//! * `bind` is resolved here, against the catalog.
//!
//! The reasons in `rejected` are read by people, so they are written as
//! sentences; every structural name stays the identifier the engine uses.

use crate::ast::{Expr, SelectItem, SelectStmt, Stmt, TableRef};
use crate::config::{EngineKind, PageLayout};
use crate::exec::operator::{self, PhysicalOperator};
use crate::exec::plan::AccessPath;
use crate::result::json_string;
use crate::Database;

/// One operator of the tree that will run.
pub struct PlanNode {
    pub node: String,
    pub detail: Vec<(&'static str, String)>,
    pub children: Vec<PlanNode>,
}

/// The access path the planner recorded, in a shape a panel can print.
pub struct ChosenPath {
    pub path: &'static str,
    pub table: Option<String>,
    pub index: Option<String>,
    /// The indexed column, for the index paths.
    pub column: Option<String>,
    pub condition: Option<String>,
}

/// A path the planner turned down, and why. The reason is a sentence because a
/// person reads it; the path name stays the engine's identifier.
pub struct RejectedPath {
    pub path: &'static str,
    pub reason: String,
}

/// One table reference in the statement, resolved against the catalog.
pub struct BindTable {
    pub name: String,
    pub alias: Option<String>,
    pub database: String,
    pub engine: Option<&'static str>,
    pub layout: Option<&'static str>,
    pub file_no: Option<u32>,
}

/// One column reference, resolved to the base column it names.
///
/// `source` says how it resolved, and is the field to read first: a reference
/// that did not resolve is reported as such rather than guessed at, because a
/// panel that silently shows the wrong column is worse than one showing none.
pub struct BindColumn {
    /// The reference as written (`x.name`, `name`).
    pub reference: String,
    pub qualifier: Option<String>,
    pub table: Option<String>,
    pub column: Option<String>,
    pub dtype: Option<String>,
    /// `base` (a column of a FROM table), `alias` (a SELECT alias), `ambiguous`
    /// (several FROM tables have that column), or `unknown`.
    pub source: &'static str,
}

pub struct BindInfo {
    pub tables: Vec<BindTable>,
    pub columns: Vec<BindColumn>,
}

/// Everything the panel shows about one statement.
pub struct PlanReport {
    pub explain: Option<String>,
    pub chosen: Option<ChosenPath>,
    pub rejected: Vec<RejectedPath>,
    /// The tree that runs, `None` when the operator layer does not cover the
    /// statement (the materialized executor runs it instead).
    pub tree: Option<PlanNode>,
    pub bind: Option<BindInfo>,
    /// Why there is no plan, when there is none.
    pub error: Option<String>,
}

impl PlanReport {
    /// The report as a JSON object. Written by hand: the crate has no JSON
    /// dependency, and this shape is small and fixed.
    pub fn to_json(&self) -> String {
        let mut out = String::from("{\"explain\":");
        match &self.explain {
            Some(text) => out.push_str(&json_string(text)),
            None => out.push_str("null"),
        }
        out.push_str(",\"chosen\":");
        match &self.chosen {
            Some(chosen) => {
                out.push_str(&format!(
                    "{{\"path\":{},\"table\":{},\"index\":{},\"column\":{},\"condition\":{}}}",
                    json_string(chosen.path),
                    optional(&chosen.table),
                    optional(&chosen.index),
                    optional(&chosen.column),
                    optional(&chosen.condition),
                ));
            }
            None => out.push_str("null"),
        }
        let rejected: Vec<String> = self
            .rejected
            .iter()
            .map(|r| {
                format!(
                    "{{\"path\":{},\"reason\":{}}}",
                    json_string(r.path),
                    json_string(&r.reason)
                )
            })
            .collect();
        out.push_str(&format!(",\"rejected\":[{}]", rejected.join(",")));
        // `physical` is spelled out rather than implied by `plan` being null:
        // the reader needs to know whether what is drawn above is what runs.
        out.push_str(&format!(",\"physical\":{}", self.tree.is_some()));
        out.push_str(",\"plan\":");
        match &self.tree {
            Some(node) => out.push_str(&node.to_json()),
            None => out.push_str("null"),
        }
        out.push_str(",\"bind\":");
        match &self.bind {
            Some(bind) => out.push_str(&bind.to_json()),
            None => out.push_str("null"),
        }
        out.push_str(",\"error\":");
        match &self.error {
            Some(message) => out.push_str(&format!(
                "{{\"stage\":\"plan\",\"message\":{}}}",
                json_string(message)
            )),
            None => out.push_str("null"),
        }
        out.push('}');
        out
    }
}

impl PlanNode {
    pub fn to_json(&self) -> String {
        let detail: Vec<String> = self
            .detail
            .iter()
            .map(|(key, value)| format!("{{\"key\":{},\"value\":{}}}", json_string(key), json_string(value)))
            .collect();
        let children: Vec<String> = self.children.iter().map(PlanNode::to_json).collect();
        format!(
            "{{\"node\":{},\"detail\":[{}],\"children\":[{}]}}",
            json_string(&self.node),
            detail.join(","),
            children.join(",")
        )
    }
}

impl BindInfo {
    fn to_json(&self) -> String {
        let tables: Vec<String> = self
            .tables
            .iter()
            .map(|t| {
                format!(
                    "{{\"name\":{},\"alias\":{},\"database\":{},\"engine\":{},\"layout\":{},\"file_no\":{}}}",
                    json_string(&t.name),
                    optional(&t.alias),
                    json_string(&t.database),
                    optional_owned(t.engine),
                    optional_owned(t.layout),
                    t.file_no.map_or_else(|| "null".to_string(), |n| n.to_string()),
                )
            })
            .collect();
        let columns: Vec<String> = self
            .columns
            .iter()
            .map(|c| {
                format!(
                    "{{\"ref\":{},\"qualifier\":{},\"table\":{},\"column\":{},\"type\":{},\"source\":{}}}",
                    json_string(&c.reference),
                    optional(&c.qualifier),
                    optional(&c.table),
                    optional(&c.column),
                    optional(&c.dtype),
                    json_string(c.source),
                )
            })
            .collect();
        format!("{{\"tables\":[{}],\"columns\":[{}]}}", tables.join(","), columns.join(","))
    }
}

/// Describes one statement for the plan panel. Read-only: no transaction is
/// opened, and nothing here runs the statement.
pub fn report(database: &str, db: &Database, stmt: &Stmt) -> PlanReport {
    let mut report = PlanReport {
        explain: None,
        chosen: None,
        rejected: Vec::new(),
        tree: None,
        bind: None,
        error: None,
    };

    if let Stmt::Select(select) = stmt {
        if let Ok(choice) = crate::exec::plan::path_choice(db, select) {
            report.explain = Some(crate::exec::plan::render_path(&choice.chosen));
            report.chosen = Some(chosen_path(&choice.chosen));
            report.rejected = choice
                .rejected
                .into_iter()
                .map(|r| RejectedPath { path: r.path, reason: r.reason })
                .collect();
        }
        report.bind = Some(bind_info(database, db, select));
    }

    // The tree is built the same way the executor builds it, and then dropped
    // without being opened: building reads the catalog and, for an index scan,
    // the index -- it does not touch rows.
    match operator::build_statement(db, stmt) {
        Ok(Some(op)) => report.tree = Some(tree(op.as_ref())),
        Ok(None) => {
            report.error = Some(
                "only SELECT and DML statements run through the operator layer".to_string(),
            );
        }
        Err(e) => report.error = Some(e.to_string()),
    }
    report
}

/// The tree that will run, as data. Recursion follows
/// [`PhysicalOperator::children`], so this cannot describe a tree the executor
/// would not build.
pub fn tree(op: &dyn PhysicalOperator) -> PlanNode {
    PlanNode {
        node: op.name().to_string(),
        detail: op.details(),
        children: op.children().into_iter().map(tree).collect(),
    }
}

fn chosen_path(path: &AccessPath) -> ChosenPath {
    match path {
        AccessPath::Constant => ChosenPath {
            path: "ConstantSelect",
            table: None,
            index: None,
            column: None,
            condition: None,
        },
        AccessPath::FullScan { table, view } => ChosenPath {
            path: if *view { "ViewScan" } else { "FullScan" },
            table: Some(table.clone()),
            index: None,
            column: None,
            condition: None,
        },
        AccessPath::IndexScan { table, index, column, condition } => ChosenPath {
            path: "IndexScan",
            table: Some(table.clone()),
            index: Some(index.clone()),
            column: Some(column.clone()),
            condition: Some(condition.clone()),
        },
        AccessPath::OrderedIndexScan { table, index, column } => ChosenPath {
            path: "OrderedIndexScan",
            table: Some(table.clone()),
            index: Some(index.clone()),
            column: Some(column.clone()),
            condition: None,
        },
        AccessPath::Join { hash, .. } => ChosenPath {
            path: if *hash { "HashJoin" } else { "NestedLoopJoin" },
            table: None,
            index: None,
            column: None,
            condition: None,
        },
    }
}

/// Resolves every table and column reference in a SELECT against the catalog.
pub fn bind_info(database: &str, db: &Database, select: &SelectStmt) -> BindInfo {
    let catalog = db.catalog();
    let mut tables = Vec::new();
    for tref in &select.from {
        tables.push(bind_table(database, db, tref));
    }

    let mut refs: Vec<(Option<String>, String)> = Vec::new();
    for item in &select.items {
        match item {
            SelectItem::Star => {}
            SelectItem::Expr(expr) | SelectItem::Aliased(expr, _) => collect_columns(expr, &mut refs),
        }
    }
    if let Some(selection) = &select.selection {
        collect_columns(selection, &mut refs);
    }
    for expr in &select.group_by {
        collect_columns(expr, &mut refs);
    }
    for (expr, _) in &select.order_by {
        collect_columns(expr, &mut refs);
    }

    let mut columns = Vec::new();
    let mut seen: Vec<(Option<String>, String)> = Vec::new();
    for (qualifier, name) in refs {
        // The same column appears in the projection and the predicate; list it
        // once, in the order it was first written.
        if seen.contains(&(qualifier.clone(), name.clone())) {
            continue;
        }
        seen.push((qualifier.clone(), name.clone()));
        columns.push(resolve_column(&catalog, &tables, qualifier, name, select));
    }

    BindInfo { tables, columns }
}

fn bind_table(database: &str, db: &Database, tref: &TableRef) -> BindTable {
    let catalog = db.catalog();
    // A view has no file of its own: it is a stored SELECT, and saying "heap"
    // or a page count for it would be an invention.
    if catalog.view(&tref.name).is_some() {
        return BindTable {
            name: tref.name.clone(),
            alias: tref.alias.clone(),
            database: database.to_string(),
            engine: Some("view"),
            layout: None,
            file_no: None,
        };
    }
    match catalog.table(&tref.name) {
        Ok(table) => BindTable {
            name: tref.name.clone(),
            alias: tref.alias.clone(),
            database: database.to_string(),
            engine: Some(engine_name(table.engine_kind)),
            layout: Some(layout_name(table.layout)),
            file_no: Some(table.heap.file_no),
        },
        Err(_) => BindTable {
            name: tref.name.clone(),
            alias: tref.alias.clone(),
            database: database.to_string(),
            engine: None,
            layout: None,
            file_no: None,
        },
    }
}

fn resolve_column(
    catalog: &crate::catalog::Catalog,
    tables: &[BindTable],
    qualifier: Option<String>,
    name: String,
    select: &SelectStmt,
) -> BindColumn {
    let reference = match &qualifier {
        Some(q) => format!("{q}.{name}"),
        None => name.clone(),
    };
    let mut found: Vec<(String, String)> = Vec::new();
    for bound in tables {
        // Only the tables this reference could belong to: an explicit
        // qualifier restricts it, an unqualified name searches all of them.
        if let Some(q) = &qualifier
            && bound.alias.as_deref() != Some(q.as_str())
            && &bound.name != q
        {
            continue;
        }
        let Ok(table) = catalog.table(&bound.name) else {
            continue;
        };
        if let Some(column) = table.schema.columns.iter().find(|c| c.name == name) {
            found.push((bound.name.clone(), column.dtype.to_string()));
        }
    }

    match found.len() {
        1 => {
            let (table, dtype) = found.into_iter().next().expect("one match");
            BindColumn {
                reference,
                qualifier,
                table: Some(table),
                column: Some(name),
                dtype: Some(dtype),
                source: "base",
            }
        }
        0 if is_select_alias(select, &name) => BindColumn {
            reference,
            qualifier,
            table: None,
            column: None,
            dtype: None,
            source: "alias",
        },
        0 => BindColumn {
            reference,
            qualifier,
            table: None,
            column: None,
            dtype: None,
            source: "unknown",
        },
        _ => BindColumn {
            reference,
            qualifier,
            table: None,
            column: None,
            dtype: None,
            source: "ambiguous",
        },
    }
}

/// Whether `name` is a `SELECT ... AS name` alias. An alias is not a base
/// column, but it is not a mistake either, and the panel should say which.
fn is_select_alias(select: &SelectStmt, name: &str) -> bool {
    select.items.iter().any(|item| match item {
        SelectItem::Aliased(_, alias) => alias.eq_ignore_ascii_case(name),
        _ => false,
    })
}

fn collect_columns(expr: &Expr, out: &mut Vec<(Option<String>, String)>) {
    match expr {
        Expr::Column(name) => out.push((None, name.clone())),
        Expr::QualifiedColumn(table, name) => out.push((Some(table.clone()), name.clone())),
        Expr::Unary(_, e) | Expr::IsNull(e, _) => collect_columns(e, out),
        Expr::Binary(_, l, r) => {
            collect_columns(l, out);
            collect_columns(r, out);
        }
        Expr::Like { expr, pattern, .. } => {
            collect_columns(expr, out);
            collect_columns(pattern, out);
        }
        Expr::Function(_, args) => {
            for arg in args {
                collect_columns(arg, out);
            }
        }
        Expr::Aggregate(_, Some(e), _) => collect_columns(e, out),
        Expr::Aggregate(_, None, _) => {}
        // Subqueries bind against their own FROM clause; their columns are not
        // columns of this statement, and listing them here would misattribute
        // them to the enclosing query.
        Expr::InSubquery { expr, .. } => collect_columns(expr, out),
        Expr::Exists { .. } | Expr::ScalarSubquery(_) => {}
        Expr::Int(_) | Expr::Float(_) | Expr::Str(_) | Expr::Null | Expr::Value(_) => {}
    }
}

fn engine_name(kind: EngineKind) -> &'static str {
    match kind {
        EngineKind::Heap => "heap",
        EngineKind::Lsm => "lsm",
    }
}

fn layout_name(layout: PageLayout) -> &'static str {
    match layout {
        PageLayout::Row => "row",
        PageLayout::Pax => "pax",
    }
}

/// A nullable string field.
fn optional(value: &Option<String>) -> String {
    match value {
        Some(value) => json_string(value),
        None => "null".to_string(),
    }
}

fn optional_owned(value: Option<&'static str>) -> String {
    match value {
        Some(value) => json_string(value),
        None => "null".to_string(),
    }
}
