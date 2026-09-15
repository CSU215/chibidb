//! The logical plan: a relational IR plus its translation and rewrites.
//!
//! [`translate`] is a pure structural step that exposes a SELECT as explicit
//! `Scan`/`Filter`/`Join`/`Project`/`Aggregate`/`Having`/`Sort`/`Distinct`/
//! `Limit` nodes instead of the quirks of [`SelectStmt`]. [`optimize`] runs the
//! logical rewrites (currently predicate pushdown) on that algebra, and the
//! lowering step turns the result into physical operators. Access-path choice
//! (index vs scan, hash vs nested loop) is deliberately *not* here.

use crate::catalog::Schema;
use crate::exec::command::{DeleteCommand, InsertCommand, UpdateCommand};
use crate::sql::ast::{BinOp, Expr, JoinKind, Limit, SelectItem, SelectStmt, Stmt, TableRef};
use crate::{Database, Error, Result};

use crate::exec::aggregate::{expr_has_aggregate, extract_aggregates};
use crate::exec::eval::{expr_has_column, expr_has_subquery};

use crate::value::DataType;

use super::util::{
    combine_and, equi_join_keys, items_have_aggregate, join_clauses, split_conjuncts,
};

/// One node of the logical plan for a statement.
pub(crate) enum LogicalOperator {
    /// A base table or view, with its alias resolved by the caller.
    Scan(TableRef),
    /// One empty tuple, for a SELECT without a FROM clause.
    Constant,
    Filter { input: Box<LogicalOperator>, predicate: Expr },
    Join {
        left: Box<LogicalOperator>,
        right: Box<LogicalOperator>,
        kind: JoinKind,
        on: Option<Expr>,
    },
    /// Grouping/aggregation: one output row per group, with the aggregate
    /// results appended (see the physical `Aggregate`).
    Aggregate {
        input: Box<LogicalOperator>,
        group_by: Vec<Expr>,
        aggregates: Vec<Expr>,
    },
    /// A `HAVING` filter above an [`LogicalOperator::Aggregate`].
    Having { input: Box<LogicalOperator>, predicate: Expr },
    Project { input: Box<LogicalOperator>, items: Vec<SelectItem> },
    Sort { input: Box<LogicalOperator>, order_by: Vec<(Expr, bool)> },
    Distinct { input: Box<LogicalOperator> },
    Limit { input: Box<LogicalOperator>, limit: Limit },
    /// `UNION [ALL]`: each input is a complete logical plan; the trailing
    /// ORDER BY / LIMIT apply to the whole set. The bool marks `UNION ALL`.
    Union {
        inputs: Vec<(bool, Box<LogicalOperator>)>,
        order_by: Vec<(Expr, bool)>,
        limit: Option<Limit>,
    },
    /// DML commands, carrying schema-bound operands lowering turns into the
    /// command operator.
    Insert(InsertCommand),
    Update(UpdateCommand),
    Delete(DeleteCommand),
}

/// Translates a top-level statement into a logical plan. Returns `None` for
/// statements the operator layer does not cover (DDL, SHOW, transaction
/// control, ...). DML is bound to schema positions here, so the plan and the
/// operators carry resolved operands rather than the AST statement.
pub(crate) fn translate_stmt(db: &Database, stmt: &Stmt) -> Result<Option<LogicalOperator>> {
    Ok(match stmt {
        Stmt::Select(s) => {
            validate_select(s)?;
            let node = translate(s);
            if let Some(node) = &node {
                validate_union_arities(db, node)?;
            }
            node
        }
        Stmt::Insert(i) => Some(LogicalOperator::Insert(InsertCommand::resolve(db, i)?)),
        Stmt::Update(u) => Some(LogicalOperator::Update(UpdateCommand::resolve(db, u)?)),
        Stmt::Delete(d) => Some(LogicalOperator::Delete(DeleteCommand::resolve(db, d)?)),
        _ => None,
    })
}

/// Semantic checks that belong to analysis, not to lowering: GROUP BY rules.
/// Subqueries in the FROM/WHERE expressions are validated when their own plans
/// are built.
fn validate_select(select: &SelectStmt) -> Result<()> {
    for group in &select.group_by {
        if expr_has_aggregate(group) {
            return Err(Error::Runtime(
                "aggregate functions are not allowed in group by".into(),
            ));
        }
    }
    // Without GROUP BY, an aggregate query may only project aggregates: any bare
    // column reference in the SELECT list is a grouping error.
    if select.group_by.is_empty()
        && (select.having.is_some() || items_have_aggregate(&select.items))
    {
        for item in &select.items {
            if let SelectItem::Expr(e) | SelectItem::Aliased(e, _) = item
                && expr_has_column(e)
            {
                return Err(Error::Runtime(
                    "column must appear in group by or aggregate".into(),
                ));
            }
        }
    }
    for (_, operand) in &select.set_ops {
        validate_select(operand)?;
    }
    Ok(())
}

/// Every UNION arm must produce the same number of columns. Checked here from
/// the catalog-resolved output arity, so lowering never has to.
fn validate_union_arities(db: &Database, node: &LogicalOperator) -> Result<()> {
    match node {
        LogicalOperator::Union { inputs, .. } => {
            let mut arity: Option<usize> = None;
            for (_, input) in inputs {
                validate_union_arities(db, input)?;
                let Some(n) = output_names(db, input)?.map(|names| names.len()) else {
                    continue;
                };
                match arity {
                    None => arity = Some(n),
                    Some(a) if a != n => {
                        return Err(Error::Runtime(format!(
                            "union column count mismatch: {a} vs {n}"
                        )));
                    }
                    _ => {}
                }
            }
            Ok(())
        }
        LogicalOperator::Filter { input, .. }
        | LogicalOperator::Having { input, .. }
        | LogicalOperator::Aggregate { input, .. }
        | LogicalOperator::Project { input, .. }
        | LogicalOperator::Sort { input, .. }
        | LogicalOperator::Distinct { input }
        | LogicalOperator::Limit { input, .. } => validate_union_arities(db, input),
        LogicalOperator::Join { left, right, .. } => {
            validate_union_arities(db, left)?;
            validate_union_arities(db, right)
        }
        _ => Ok(()),
    }
}

/// Translates a SELECT into a logical plan. Returns `None` for the shapes the
/// operator layer cannot run (a `*`/aggregate projection with no FROM).
pub(crate) fn translate(select: &SelectStmt) -> Option<LogicalOperator> {
    if !select.set_ops.is_empty() {
        return translate_union(select);
    }
    if select.from.is_empty() {
        // A constant SELECT projects one empty tuple; `*` and aggregates have
        // no input to work on and stay unsupported.
        if select.items.iter().any(|it| matches!(it, SelectItem::Star))
            || items_have_aggregate(&select.items)
        {
            return None;
        }
        return Some(LogicalOperator::Project {
            input: Box::new(LogicalOperator::Constant),
            items: select.items.clone(),
        });
    }
    let mut node = logical_from(select)?;
    // Grouped/aggregate queries: Aggregate, then HAVING / ORDER BY / project /
    // DISTINCT / LIMIT as standard nodes.
    if !select.group_by.is_empty()
        || select.having.is_some()
        || items_have_aggregate(&select.items)
    {
        let mut tail = LogicalOperator::Aggregate {
            input: Box::new(node),
            group_by: select.group_by.clone(),
            aggregates: aggregate_list(select),
        };
        if let Some(having) = &select.having {
            tail = LogicalOperator::Having {
                input: Box::new(tail),
                predicate: having.clone(),
            };
        }
        if !select.order_by.is_empty() {
            tail = LogicalOperator::Sort {
                input: Box::new(tail),
                order_by: select.order_by.clone(),
            };
        }
        tail = LogicalOperator::Project {
            input: Box::new(tail),
            items: select.items.clone(),
        };
        if select.distinct {
            tail = LogicalOperator::Distinct { input: Box::new(tail) };
        }
        if let Some(limit) = &select.limit {
            tail = LogicalOperator::Limit { input: Box::new(tail), limit: limit.clone() };
        }
        return Some(tail);
    }
    if !select.order_by.is_empty() {
        node = LogicalOperator::Sort {
            input: Box::new(node),
            order_by: select.order_by.clone(),
        };
    }
    node = LogicalOperator::Project {
        input: Box::new(node),
        items: select.items.clone(),
    };
    if select.distinct {
        node = LogicalOperator::Distinct { input: Box::new(node) };
    }
    if let Some(limit) = &select.limit {
        node = LogicalOperator::Limit { input: Box::new(node), limit: limit.clone() };
    }
    Some(node)
}

/// Translates a UNION chain: the head select (minus its trailing ORDER BY /
/// LIMIT, which the union node owns) and every operand become inputs.
fn translate_union(select: &SelectStmt) -> Option<LogicalOperator> {
    let mut base = select.clone();
    base.set_ops = Vec::new();
    let order_by = std::mem::take(&mut base.order_by);
    let limit = base.limit.take();
    let mut inputs = vec![(true, Box::new(translate(&base)?))];
    for (all, operand) in &select.set_ops {
        inputs.push((*all, Box::new(translate(operand)?)));
    }
    Some(LogicalOperator::Union { inputs, order_by, limit })
}

/// Translates a SELECT's FROM / WHERE / JOIN region into a logical plan. The
/// WHERE clause becomes one `Filter` above the join tree; it is not yet pushed.
fn logical_from(select: &SelectStmt) -> Option<LogicalOperator> {
    let first = select.from.first()?;
    let mut node = LogicalOperator::Scan(first.clone());
    for (i, (kind, on)) in join_clauses(select).into_iter().enumerate() {
        let right = LogicalOperator::Scan(select.from[i + 1].clone());
        node = LogicalOperator::Join {
            left: Box::new(node),
            right: Box::new(right),
            kind,
            on,
        };
    }
    if let Some(selection) = &select.selection {
        node = LogicalOperator::Filter {
            input: Box::new(node),
            predicate: selection.clone(),
        };
    }
    Some(node)
}

/// The distinct aggregate expressions in a SELECT's items, HAVING and ORDER BY,
/// in first-seen order. Matches what lowering extracts, so `#aggN` indices line
/// up.
fn aggregate_list(select: &SelectStmt) -> Vec<Expr> {
    let mut refs: Vec<&Expr> = Vec::new();
    for item in &select.items {
        match item {
            SelectItem::Expr(e) | SelectItem::Aliased(e, _) => refs.push(e),
            SelectItem::Star => {}
        }
    }
    if let Some(having) = &select.having {
        refs.push(having);
    }
    for (e, _) in &select.order_by {
        refs.push(e);
    }
    extract_aggregates(&refs)
}

/// The schema of a logical node's source. Only single-source scans are resolved
/// here (predicate pushdown asks for exactly those); projection shapes keep
/// their input schema, because pushing a predicate only needs to know which
/// source owns a column. Resolving from the catalog keeps the logical layer
/// independent of physical operator construction.
fn schema(db: &Database, node: &LogicalOperator) -> Result<Option<Schema>> {
    match node {
        LogicalOperator::Scan(tref) => source_schema(db, tref),
        LogicalOperator::Filter { input, .. }
        | LogicalOperator::Having { input, .. }
        | LogicalOperator::Aggregate { input, .. }
        | LogicalOperator::Project { input, .. }
        | LogicalOperator::Sort { input, .. }
        | LogicalOperator::Distinct { input }
        | LogicalOperator::Limit { input, .. } => schema(db, input),
        LogicalOperator::Join { left, right, .. } => {
            let (Some(mut left), Some(right)) = (schema(db, left)?, schema(db, right)?) else {
                return Ok(None);
            };
            left.columns.extend(right.columns);
            Ok(Some(left))
        }
        LogicalOperator::Constant => Ok(Some(Schema::default())),
        LogicalOperator::Union { inputs, .. } => match inputs.first() {
            Some((_, first)) => schema(db, first),
            None => Ok(Some(Schema::default())),
        },
        LogicalOperator::Insert(_) | LogicalOperator::Update(_) | LogicalOperator::Delete(_) => {
            Ok(Some(Schema::default()))
        }
    }
}

/// Resolves a FROM source (table or view) to the schema a scan exposes. A view
/// is expanded into its projected output columns, mirroring `ViewScan`.
fn source_schema(db: &Database, tref: &TableRef) -> Result<Option<Schema>> {
    let owner = tref.alias.clone().unwrap_or_else(|| tref.name.clone());
    if let Some(sql) = db.catalog().view(&tref.name).cloned() {
        let stmts = crate::sql::parser::parse(&sql)?;
        let Some(Stmt::Select(select)) = stmts.into_iter().next() else {
            return Ok(None);
        };
        let Some(node) = translate(&select) else {
            return Ok(None);
        };
        let Some(names) = output_names(db, &node)? else {
            return Ok(None);
        };
        return Ok(Some(Schema {
            columns: names
                .into_iter()
                .map(|name| {
                    crate::catalog::ColumnDesc::plain(Some(owner.clone()), name, DataType::Text)
                })
                .collect(),
        }));
    }
    let columns = db.catalog().table(&tref.name)?.schema.columns.clone();
    Ok(Some(Schema {
        columns: columns
            .into_iter()
            .map(|c| crate::catalog::ColumnDesc::plain(Some(owner.clone()), c.name, c.dtype))
            .collect(),
    }))
}

/// The projected output column names of a logical plan, used to describe a
/// view's columns without building physical operators.
fn output_names(db: &Database, node: &LogicalOperator) -> Result<Option<Vec<String>>> {
    Ok(match node {
        LogicalOperator::Scan(tref) => {
            source_schema(db, tref)?.map(|s| s.columns.into_iter().map(|c| c.name).collect())
        }
        LogicalOperator::Constant => Some(Vec::new()),
        LogicalOperator::Filter { input, .. }
        | LogicalOperator::Having { input, .. }
        | LogicalOperator::Sort { input, .. }
        | LogicalOperator::Distinct { input }
        | LogicalOperator::Limit { input, .. } => output_names(db, input)?,
        LogicalOperator::Aggregate { input, aggregates, .. } => {
            let Some(mut names) = output_names(db, input)? else {
                return Ok(None);
            };
            names.extend((0..aggregates.len()).map(crate::exec::aggregate::agg_column));
            Some(names)
        }
        LogicalOperator::Project { input, items } => {
            let Some(base) = output_names(db, input)? else {
                return Ok(None);
            };
            let mut out = Vec::new();
            for item in items {
                match item {
                    SelectItem::Star => out.extend(base.iter().cloned()),
                    SelectItem::Expr(e) => out.push(e.to_string()),
                    SelectItem::Aliased(_, alias) => out.push(alias.clone()),
                }
            }
            Some(out)
        }
        LogicalOperator::Join { left, right, .. } => {
            let (Some(mut l), Some(r)) = (output_names(db, left)?, output_names(db, right)?) else {
                return Ok(None);
            };
            l.extend(r);
            Some(l)
        }
        LogicalOperator::Union { inputs, .. } => match inputs.first() {
            Some((_, first)) => output_names(db, first)?,
            None => Some(Vec::new()),
        },
        LogicalOperator::Insert(_) | LogicalOperator::Update(_) | LogicalOperator::Delete(_) => {
            Some(Vec::new())
        }
    })
}

/// Applies the logical rewrites to the plan (currently predicate pushdown).
pub(crate) fn optimize(
    db: &Database,
    node: LogicalOperator,
) -> Result<Option<LogicalOperator>> {
    Ok(match node {
        node @ (LogicalOperator::Scan(_)
        | LogicalOperator::Filter { .. }
        | LogicalOperator::Join { .. }) => pushdown_region(db, node)?,
        LogicalOperator::Aggregate { input, group_by, aggregates } => {
            optimize(db, *input)?.map(|n| LogicalOperator::Aggregate {
                input: Box::new(n),
                group_by,
                aggregates,
            })
        }
        LogicalOperator::Having { input, predicate } => {
            optimize(db, *input)?.map(|n| LogicalOperator::Having {
                input: Box::new(n),
                predicate,
            })
        }
        LogicalOperator::Project { input, items } => {
            optimize(db, *input)?.map(|n| LogicalOperator::Project {
                input: Box::new(n),
                items,
            })
        }
        LogicalOperator::Sort { input, order_by } => {
            optimize(db, *input)?.map(|n| LogicalOperator::Sort {
                input: Box::new(n),
                order_by,
            })
        }
        LogicalOperator::Distinct { input } => {
            optimize(db, *input)?.map(|n| LogicalOperator::Distinct { input: Box::new(n) })
        }
        LogicalOperator::Limit { input, limit } => optimize(db, *input)?.map(|n| {
            LogicalOperator::Limit { input: Box::new(n), limit }
        }),
        LogicalOperator::Constant => Some(LogicalOperator::Constant),
        LogicalOperator::Union { inputs, order_by, limit } => {
            let mut out = Vec::with_capacity(inputs.len());
            for (all, input) in inputs {
                let Some(n) = optimize(db, *input)? else {
                    return Ok(None);
                };
                out.push((all, Box::new(n)));
            }
            Some(LogicalOperator::Union { inputs: out, order_by, limit })
        }
        LogicalOperator::Insert(i) => Some(LogicalOperator::Insert(i)),
        LogicalOperator::Update(u) => Some(LogicalOperator::Update(u)),
        LogicalOperator::Delete(d) => Some(LogicalOperator::Delete(d)),
    })
}

/// Pushes WHERE conjuncts that reference a single source onto that source.
/// Only inner/comma join chains are eligible: below an outer join the
/// null-extension would change which rows are produced.
fn pushdown_region(
    db: &Database,
    node: LogicalOperator,
) -> Result<Option<LogicalOperator>> {
    if !inner_only(&node) {
        return Ok(Some(node));
    }
    let (input, selection) = match node {
        LogicalOperator::Filter { input, predicate } => (*input, Some(predicate)),
        other => (other, None),
    };
    let Some(selection) = selection else {
        return Ok(Some(input));
    };
    let conjuncts: Vec<Expr> = split_conjuncts(&selection).into_iter().cloned().collect();
    let mut scans = Vec::new();
    collect_scans(&input, &mut scans);
    let mut schemas = Vec::with_capacity(scans.len());
    for scan in &scans {
        let Some(schema) = schema(db, scan)? else {
            return Ok(None);
        };
        schemas.push(schema);
    }
    let mut pushed: Vec<Vec<Expr>> = vec![Vec::new(); schemas.len()];
    let mut kept = Vec::new();
    for conjunct in conjuncts {
        match sole_source(&conjunct, &schemas) {
            Some(i) => pushed[i].push(conjunct),
            None => kept.push(conjunct),
        }
    }
    let mut index = 0;
    let rebuilt = rebuild(input, &pushed, &mut index);
    let rebuilt = form_joins(db, rebuilt, &mut kept)?;
    Ok(Some(match combine_and(kept) {
        Some(predicate) => LogicalOperator::Filter { input: Box::new(rebuilt), predicate },
        None => rebuilt,
    }))
}

/// The logical WHERE -> JOIN rewrite: turns comma (`Cross`) joins into inner
/// joins by moving their equi-join WHERE conjuncts onto the join nodes (deepest
/// first). Conjuncts no join can consume stay in `residual` for the filter above
/// the tree. Access-path choice (hash vs nested loop) stays in lowering.
fn form_joins(
    db: &Database,
    node: LogicalOperator,
    residual: &mut Vec<Expr>,
) -> Result<LogicalOperator> {
    match node {
        LogicalOperator::Join { left, right, kind, on } => {
            let left = Box::new(form_joins(db, *left, residual)?);
            let right = Box::new(form_joins(db, *right, residual)?);
            if matches!(kind, JoinKind::Cross | JoinKind::Inner)
                && let (Some(ls), Some(rs)) = (schema(db, &left)?, schema(db, &right)?)
            {
                let mut join_preds = Vec::new();
                residual.retain(|conjunct| {
                    if equi_join_keys(conjunct, &ls, &rs).is_some() {
                        join_preds.push(conjunct.clone());
                        false
                    } else {
                        true
                    }
                });
                if let Some(extra) = combine_and(join_preds) {
                    // Fold the pushed-down equi-join conjuncts into the existing
                    // ON, so they become hash keys instead of a runtime filter.
                    let on = match on {
                        Some(existing) => {
                            Some(Expr::Binary(BinOp::And, Box::new(existing), Box::new(extra)))
                        }
                        None => Some(extra),
                    };
                    return Ok(LogicalOperator::Join {
                        left,
                        right,
                        kind: JoinKind::Inner,
                        on,
                    });
                }
            }
            Ok(LogicalOperator::Join { left, right, kind, on })
        }
        other => Ok(other),
    }
}

fn inner_only(node: &LogicalOperator) -> bool {
    match node {
        LogicalOperator::Scan(_) => true,
        LogicalOperator::Filter { input, .. } => inner_only(input),
        LogicalOperator::Join { left, right, kind, .. } => {
            matches!(kind, JoinKind::Inner | JoinKind::Cross)
                && inner_only(left)
                && inner_only(right)
        }
        _ => false,
    }
}

fn collect_scans<'a>(node: &'a LogicalOperator, out: &mut Vec<&'a LogicalOperator>) {
    match node {
        LogicalOperator::Scan(_) => out.push(node),
        LogicalOperator::Filter { input, .. } => collect_scans(input, out),
        LogicalOperator::Join { left, right, .. } => {
            collect_scans(left, out);
            collect_scans(right, out);
        }
        _ => {}
    }
}

/// Rebuilds the tree in the same left-to-right order [`collect_scans`] used,
/// wrapping each scan in the predicates assigned to it.
fn rebuild(node: LogicalOperator, pushed: &[Vec<Expr>], index: &mut usize) -> LogicalOperator {
    match node {
        LogicalOperator::Scan(tref) => {
            let i = *index;
            *index += 1;
            let scan = LogicalOperator::Scan(tref);
            match combine_and(pushed[i].clone()) {
                Some(predicate) => {
                    LogicalOperator::Filter { input: Box::new(scan), predicate }
                }
                None => scan,
            }
        }
        LogicalOperator::Filter { input, predicate } => LogicalOperator::Filter {
            input: Box::new(rebuild(*input, pushed, index)),
            predicate,
        },
        LogicalOperator::Join { left, right, kind, on } => LogicalOperator::Join {
            left: Box::new(rebuild(*left, pushed, index)),
            right: Box::new(rebuild(*right, pushed, index)),
            kind,
            on,
        },
        other => other,
    }
}

/// Renders an indented tree of `node`, two spaces per depth, newline-terminated.
pub(crate) fn logical_tree(node: &LogicalOperator) -> String {
    fn walk(node: &LogicalOperator, depth: usize, out: &mut String) {
        for _ in 0..depth {
            out.push_str("  ");
        }
        out.push_str(&label(node));
        out.push('\n');
        children(node, &mut |child| walk(child, depth + 1, out));
    }
    fn label(node: &LogicalOperator) -> String {
        match node {
            LogicalOperator::Scan(tref) => match &tref.alias {
                Some(alias) => format!("Scan {} as {alias}", tref.name),
                None => format!("Scan {}", tref.name),
            },
            LogicalOperator::Constant => "Constant".to_string(),
            LogicalOperator::Filter { predicate, .. } => format!("Filter {predicate}"),
            LogicalOperator::Having { predicate, .. } => format!("Having {predicate}"),
            LogicalOperator::Join { kind, .. } => format!("Join {kind:?}"),
            LogicalOperator::Aggregate { .. } => "Aggregate".to_string(),
            LogicalOperator::Project { items, .. } => format!("Project cols={}", items.len()),
            LogicalOperator::Sort { order_by, .. } => format!("Sort keys={}", order_by.len()),
            LogicalOperator::Distinct { .. } => "Distinct".to_string(),
            LogicalOperator::Limit { limit, .. } => {
                format!("Limit count={}", limit.count)
            }
            LogicalOperator::Union { inputs, .. } => format!("Union arms={}", inputs.len()),
            LogicalOperator::Insert(i) => format!("Insert {}", i.table),
            LogicalOperator::Update(u) => format!("Update {}", u.table),
            LogicalOperator::Delete(d) => format!("Delete {}", d.table),
        }
    }
    fn children(node: &LogicalOperator, f: &mut impl FnMut(&LogicalOperator)) {
        match node {
            LogicalOperator::Scan(_)
            | LogicalOperator::Constant
            | LogicalOperator::Insert(_)
            | LogicalOperator::Update(_)
            | LogicalOperator::Delete(_) => {}
            LogicalOperator::Filter { input, .. }
            | LogicalOperator::Having { input, .. }
            | LogicalOperator::Aggregate { input, .. }
            | LogicalOperator::Project { input, .. }
            | LogicalOperator::Sort { input, .. }
            | LogicalOperator::Distinct { input }
            | LogicalOperator::Limit { input, .. } => f(input),
            LogicalOperator::Join { left, right, .. } => {
                f(left);
                f(right);
            }
            LogicalOperator::Union { inputs, .. } => {
                for (_, input) in inputs {
                    f(input);
                }
            }
        }
    }
    let mut out = String::new();
    walk(node, 0, &mut out);
    out
}

/// The single source that owns every column of `expr`, or `None` when the
/// expression has no column, carries a subquery, or resolves in more than one
/// source (ambiguous, so pushing it would hide the ambiguity).
fn sole_source(expr: &Expr, sources: &[Schema]) -> Option<usize> {
    if !expr_has_column(expr) || expr_has_subquery(expr) {
        return None;
    }
    let mut found = None;
    for (i, schema) in sources.iter().enumerate() {
        if expr_resolves(expr, schema) {
            if found.is_some() {
                return None;
            }
            found = Some(i);
        }
    }
    found
}

/// Whether every column reference in `expr` resolves against `schema`.
fn expr_resolves(expr: &Expr, schema: &Schema) -> bool {
    match expr {
        Expr::Column(name) => schema.index_of(name).is_some(),
        Expr::QualifiedColumn(owner, name) => schema
            .columns
            .iter()
            .any(|c| c.owner.as_deref() == Some(owner) && &c.name == name),
        Expr::Unary(_, e) => expr_resolves(e, schema),
        Expr::Binary(_, l, r) => expr_resolves(l, schema) && expr_resolves(r, schema),
        Expr::IsNull(e, _) => expr_resolves(e, schema),
        Expr::Like { expr, pattern, .. } => {
            expr_resolves(expr, schema) && expr_resolves(pattern, schema)
        }
        Expr::Function(_, args) => args.iter().all(|a| expr_resolves(a, schema)),
        // Literals, values and anything else without a column resolve trivially.
        _ => true,
    }
}
