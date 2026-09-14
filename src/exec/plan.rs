use crate::ast::{BinOp, DataType, Expr, ExplainStmt, SelectItem, SelectStmt, Stmt};
use crate::index::{encode_key, BTree, Bound};
use crate::result::ResultSet;
use crate::storage::Rid;
use crate::{Database, Error, Result};

use super::coerce;
use super::eval::{eval_const, expr_has_column, expr_has_subquery};

pub(crate) fn execute_explain(db: &Database, e: &ExplainStmt) -> Result<ResultSet> {
    match &*e.stmt {
        Stmt::Select(s) => Ok(ResultSet::Message(plan_select(db, s)?)),
        _ => Err(Error::Runtime("explain supports select only".into())),
    }
}

/// The access path this planner chose, and the ones it turned down.
///
/// This is the record the EXPLAIN text is rendered from, so the text and the
/// structure cannot disagree with each other. **Neither is the truth about
/// what runs**: `build_select` is, and `introspect::plan` renders that tree.
/// `tests/plan_api.rs` asserts the two agree (see the drift test there).
pub(crate) struct PathChoice {
    pub chosen: AccessPath,
    /// Losing candidates, most relevant first, each with the reason it lost.
    /// The reasons are shown to a reader, so they are written as sentences.
    pub rejected: Vec<RejectedPath>,
}

pub(crate) struct RejectedPath {
    /// The path not taken, named the way `plan_select` names it.
    pub path: &'static str,
    pub reason: String,
}

pub(crate) enum AccessPath {
    /// No FROM clause: a single projected tuple.
    Constant,
    /// Sequential scan of one table (or of a view's own plan).
    FullScan { table: String, view: bool },
    /// Index lookup or range scan driven by a sargable predicate.
    IndexScan { table: String, index: String, column: String, condition: String },
    /// In-order walk of an index's leaf chain, for a matching ORDER BY.
    OrderedIndexScan { table: String, index: String, column: String },
    /// Several FROM entries, joined by hash or by nested loops.
    Join { hash: bool, tables: usize },
}

/// The EXPLAIN text for a SELECT, rendered from [`path_choice`].
pub(crate) fn plan_select(db: &Database, s: &SelectStmt) -> Result<String> {
    Ok(render_path(&path_choice(db, s)?.chosen))
}

pub(crate) fn render_path(path: &AccessPath) -> String {
    match path {
        AccessPath::Constant => "ConstantSelect -> Project".into(),
        // A view is scanned by running its own plan, not by reading pages of a
        // file, and the text says so rather than calling it a full scan.
        AccessPath::FullScan { table, view } => {
            let kind = if *view { "ViewScan" } else { "FullScan" };
            format!("{kind}(table={table}) -> Filter -> Project")
        }
        AccessPath::IndexScan { table, index, condition, .. } => {
            format!("IndexScan(index={index}, table={table}, where {condition}) -> Filter -> Project")
        }
        AccessPath::OrderedIndexScan { table, index, .. } => {
            format!("OrderedIndexScan(index={index}, table={table}) -> Filter -> Project")
        }
        AccessPath::Join { hash, tables } => {
            let strategy = if *hash { "HashJoin" } else { "NestedLoopJoin" };
            format!("{strategy}(tables={tables}) -> Filter -> Project")
        }
    }
}

/// Rule-based access-path choice for a SELECT, with the rejected candidates
/// kept rather than dropped.
pub(crate) fn path_choice(db: &Database, s: &SelectStmt) -> Result<PathChoice> {
    if s.from.is_empty() {
        return Ok(PathChoice { chosen: AccessPath::Constant, rejected: Vec::new() });
    }
    if s.from.len() > 1 {
        let hash = super::operator::select_uses_hash_join(db, s)?;
        let reason = if hash {
            "两侧的等值连接条件可以建哈希表，嵌套循环会对右表反复扫描"
        } else {
            "连接条件不是等值比较，建不起哈希表"
        };
        return Ok(PathChoice {
            chosen: AccessPath::Join { hash, tables: s.from.len() },
            rejected: vec![RejectedPath {
                path: if hash { "NestedLoopJoin" } else { "HashJoin" },
                reason: reason.into(),
            }],
        });
    }

    let table = s.from[0].name.clone();
    let is_view = db.catalog().view(&table).is_some();
    // An in-order scan needs the output order to come from the scan itself:
    // grouping and aggregation both reorder the result.
    let ordering_usable =
        s.group_by.is_empty() && !super::operator::items_have_aggregate(&s.items);
    let order_column = resolved_order_column(&s.items, &s.order_by);
    let no_predicate = match s.selection {
        None => "没有 WHERE 子句，索引无从下推".to_string(),
        Some(_) => "WHERE 的谓词列上没有索引，或谓词不是可下推的等值/范围比较".to_string(),
    };

    if let Some(sarg) = find_sargable(db, &table, s.selection.as_ref())? {
        let ordered = ordering_usable && order_column.as_deref() == Some(sarg.column.as_str());
        let mut rejected = vec![RejectedPath {
            path: "TableScan",
            reason: format!(
                "WHERE 命中索引 {}（列 {}）的{}",
                sarg.index,
                sarg.column,
                match &sarg.kind {
                    SargKind::Eq(_) => "等值条件",
                    SargKind::Range { .. } => "范围条件",
                }
            ),
        }];
        let chosen = if ordered {
            rejected.push(RejectedPath {
                path: "IndexScan",
                reason: format!(
                    "ORDER BY 的列与索引列相同，{} 的叶链本身有序，不必再排序",
                    sarg.index
                ),
            });
            AccessPath::OrderedIndexScan {
                table,
                index: sarg.index,
                column: sarg.column,
            }
        } else {
            rejected.push(RejectedPath {
                path: "OrderedIndexScan",
                reason: unusable_ordering(s, order_column.as_deref(), &sarg.column),
            });
            let condition = describe_sarg(&sarg);
            AccessPath::IndexScan { table, index: sarg.index, column: sarg.column, condition }
        };
        return Ok(PathChoice { chosen, rejected });
    }

    // An index on the ORDER BY column can supply the order without a WHERE
    // clause, so the sort never happens. Views have no indexes to ride.
    if ordering_usable
        && !is_view
        && let Some(column) = order_column
        && let Some(ix) = db
            .catalog()
            .indexes_for(&table)
            .into_iter()
            .find(|ix| ix.column == column)
    {
        return Ok(PathChoice {
            chosen: AccessPath::OrderedIndexScan { table, index: ix.name.clone(), column },
            rejected: vec![
                RejectedPath {
                    path: "FullScan",
                    reason: format!("ORDER BY 的列上有索引 {}，叶链的顺序就是所需顺序", ix.name),
                },
                RejectedPath { path: "IndexScan", reason: no_predicate },
            ],
        });
    }
    Ok(PathChoice {
        chosen: AccessPath::FullScan { table, view: is_view },
        rejected: vec![RejectedPath { path: "IndexScan", reason: no_predicate }],
    })
}

/// Why an in-order index scan could not satisfy this ORDER BY.
fn unusable_ordering(s: &SelectStmt, order_column: Option<&str>, sarg_column: &str) -> String {
    if !s.group_by.is_empty() || super::operator::items_have_aggregate(&s.items) {
        return "有 GROUP BY 或聚合，输出顺序不由叶链决定".into();
    }
    match order_column {
        Some(column) => format!("ORDER BY 的列是 {column}，与索引列 {sarg_column} 不同"),
        None if s.order_by.is_empty() => "没有 ORDER BY，叶链顺序用不上".into(),
        None => "ORDER BY 不是单个升序列（降序或表达式排序都要再排一次）".into(),
    }
}

/// The plain column of a single ascending ORDER BY key, after resolving SELECT
/// aliases. `SELECT b AS id ... ORDER BY id` therefore resolves to `b`, so an
/// index on the base column `id` is not mistaken for satisfying the order.
pub(crate) fn resolved_order_column(
    items: &[SelectItem],
    order_by: &[(Expr, bool)],
) -> Option<String> {
    let [(e, desc)] = order_by else {
        return None;
    };
    if *desc {
        return None;
    }
    match resolve_alias(items, e) {
        Expr::Column(n) => Some(n.clone()),
        Expr::QualifiedColumn(_, n) => Some(n.clone()),
        _ => None,
    }
}

/// Replaces an unqualified column reference that names a SELECT alias with the
/// aliased expression.
fn resolve_alias<'a>(items: &'a [SelectItem], expr: &'a Expr) -> &'a Expr {
    if let Expr::Column(name) = expr {
        for item in items {
            if let SelectItem::Aliased(inner, alias) = item
                && alias == name
            {
                return inner;
            }
        }
    }
    expr
}

/// The predicate an index scan will apply, as SQL-ish text (`id = 5`).
fn describe_sarg(s: &Sargable) -> String {
    match &s.kind {
        SargKind::Eq(lit) => format!("{} = {lit}", s.column),
        SargKind::Range { lower, upper } => {
            let mut parts = Vec::new();
            if let Some((inclusive, lit)) = lower {
                parts.push(format!("{} {} {lit}", s.column, if *inclusive { ">=" } else { ">" }));
            }
            if let Some((inclusive, lit)) = upper {
                parts.push(format!("{} {} {lit}", s.column, if *inclusive { "<=" } else { "<" }));
            }
            parts.join(" and ")
        }
    }
}

enum SargKind {
    Eq(Expr),
    /// Bounds are `(inclusive, literal)`; either side may be absent.
    Range { lower: Option<(bool, Expr)>, upper: Option<(bool, Expr)> },
}

struct Sargable {
    index: String,
    column: String,
    dtype: DataType,
    kind: SargKind,
}

fn split_conjuncts(expr: &Expr) -> Vec<&Expr> {
    match expr {
        Expr::Binary(BinOp::And, l, r) => {
            let mut out = split_conjuncts(l);
            out.extend(split_conjuncts(r));
            out
        }
        other => vec![other],
    }
}

fn flip_cmp(op: BinOp) -> Option<BinOp> {
    match op {
        BinOp::Eq => Some(BinOp::Eq),
        BinOp::Lt => Some(BinOp::Gt),
        BinOp::Le => Some(BinOp::Ge),
        BinOp::Gt => Some(BinOp::Lt),
        BinOp::Ge => Some(BinOp::Le),
        _ => None,
    }
}

/// Rule-based access path choice: equality or range predicates over an
/// indexed column (even buried in an AND chain) use the index. An equality
/// wins outright; otherwise all `>`/`>=`/`<`/`<=` conjuncts on one indexed
/// column are combined into a single bounded range scan.
fn find_sargable(
    db: &Database,
    table: &str,
    selection: Option<&Expr>,
) -> Result<Option<Sargable>> {
    let Some(sel) = selection else {
        return Ok(None);
    };
    // views have no indexes, so no access path choice applies
    if db.catalog().view(table).is_some() {
        return Ok(None);
    }
    struct Cand {
        column: String,
        index: String,
        dtype: DataType,
        op: BinOp,
        lit: Expr,
    }
    let catalog = db.catalog();
    let schema = &catalog.table(table)?.schema;
    let mut cands: Vec<Cand> = Vec::new();
    for conj in split_conjuncts(sel) {
        let (col_expr, op, lit) = match conj {
            Expr::Binary(op @ (BinOp::Eq | BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge), l, r) => {
                if matches!(**l, Expr::Column(_)) && !expr_has_column(r) && !expr_has_subquery(r) {
                    ((**l).clone(), *op, (**r).clone())
                } else if matches!(**r, Expr::Column(_))
                    && !expr_has_column(l)
                    && !expr_has_subquery(l)
                {
                    match flip_cmp(*op) {
                        Some(flip) => ((**r).clone(), flip, (**l).clone()),
                        None => continue,
                    }
                } else {
                    continue;
                }
            }
            _ => continue,
        };
        let Expr::Column(cname) = col_expr else {
            continue;
        };
        let Some(col_idx) = schema.index_of(&cname) else {
            continue;
        };
        let Some(ix) = catalog.indexes_for(table).into_iter().find(|ix| ix.column == cname)
        else {
            continue;
        };
        let dtype = schema.columns[col_idx].dtype;
        // A literal that cannot be losslessly coerced to the column type must
        // not select an index: the row path compares it with `cmp_values`
        // (e.g. `int_col = 1.0`), so reject the candidate and let the full
        // scan handle it instead of erroring inside `literal_key`.
        if literal_key(&lit, dtype, &cname).is_err() {
            continue;
        }
        if op == BinOp::Eq {
            return Ok(Some(Sargable {
                index: ix.name.clone(),
                column: cname,
                dtype,
                kind: SargKind::Eq(lit),
            }));
        }
        cands.push(Cand { column: cname, index: ix.name.clone(), dtype, op, lit });
    }
    let Some(first) = cands.first() else {
        return Ok(None);
    };
    let column = first.column.clone();
    let index = first.index.clone();
    let dtype = first.dtype;
    let mut lower = None;
    let mut upper = None;
    for c in &cands {
        if c.column != column {
            continue;
        }
        match c.op {
            BinOp::Gt => lower = Some((false, c.lit.clone())),
            BinOp::Ge => lower = Some((true, c.lit.clone())),
            BinOp::Lt => upper = Some((false, c.lit.clone())),
            BinOp::Le => upper = Some((true, c.lit.clone())),
            _ => {}
        }
    }
    Ok(Some(Sargable { index, column, dtype, kind: SargKind::Range { lower, upper } }))
}

fn literal_key(lit: &Expr, dtype: DataType, column: &str) -> Result<Vec<u8>> {
    let v = eval_const(lit)?;
    let coerced = coerce(v, dtype, column)?;
    encode_key(&coerced)
}

fn bound<'a>(key: Option<&'a Vec<u8>>, inclusive: bool) -> Bound<'a> {
    match key {
        Some(k) if inclusive => Bound::Included(k),
        Some(k) => Bound::Excluded(k),
        None => Bound::Unbounded,
    }
}

/// Index-derived row ids for a sargable selection, if one applies. The
/// `IndexScan` operator consumes this.
pub(crate) struct IndexScanRids {
    pub column: String,
    pub rids: Vec<Rid>,
}

pub(crate) fn plan_index_scan(
    db: &Database,
    table: &str,
    selection: Option<&Expr>,
) -> Result<Option<IndexScanRids>> {
    let Some(sarg) = find_sargable(db, table, selection)? else {
        return Ok(None);
    };
    let ix_file = db
        .catalog()
        .indexes_for(table)
        .into_iter()
        .find(|ix| ix.name == sarg.index)
        .map(|ix| ix.store.file)
        .expect("sargable index exists");
    let btree = BTree::at(ix_file);
    let rids: Vec<Rid> = match &sarg.kind {
        SargKind::Eq(lit) => {
            let key = literal_key(lit, sarg.dtype, &sarg.column)?;
            btree.search(&db.pool, &key)?
        }
        SargKind::Range { lower, upper } => {
            let lower_key = lower
                .as_ref()
                .map(|(_, l)| literal_key(l, sarg.dtype, &sarg.column))
                .transpose()?;
            let upper_key = upper
                .as_ref()
                .map(|(_, l)| literal_key(l, sarg.dtype, &sarg.column))
                .transpose()?;
            let start = bound(lower_key.as_ref(), lower.as_ref().is_some_and(|(i, _)| *i));
            let end = bound(upper_key.as_ref(), upper.as_ref().is_some_and(|(i, _)| *i));
            scan_rids(&btree, &db.pool, start, end)?
        }
    };
    Ok(Some(IndexScanRids { column: sarg.column, rids }))
}

fn scan_rids(
    btree: &BTree,
    pool: &crate::storage::BufferPool,
    start: Bound,
    end: Bound,
) -> Result<Vec<Rid>> {
    Ok(btree
        .scan_range(pool, start, end)?
        .into_iter()
        .map(|(_, rid)| rid)
        .collect())
}

/// The index file backing an in-order scan of `column`, used to satisfy an
/// `ORDER BY column` even when there is no WHERE clause to make it sargable.
pub(crate) fn ordered_index_file(
    db: &Database,
    table: &str,
    column: &str,
) -> Result<Option<crate::storage::page::FileId>> {
    let catalog = db.catalog();
    if catalog.view(table).is_some() {
        return Ok(None);
    }
    Ok(catalog
        .indexes_for(table)
        .into_iter()
        .find(|ix| ix.column == column)
        .map(|ix| ix.store.file))
}
