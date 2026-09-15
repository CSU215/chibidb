use std::collections::{HashMap, VecDeque};

use crate::catalog::Schema;
use crate::sql::ast::{Expr, JoinKind};
use crate::index::encode_key_into;
use crate::value::Value;
use crate::{Database, Error, Result};

use super::{ExecContext, PhysicalOperator};
use crate::exec::chunk::{CHUNK_ROWS, Chunk};
use crate::exec::eval::EvalCtx;
use crate::exec::subquery::{eval_bound, eval_predicate_bound};

/// Equi-join key pairs and any residual predicate a lowered hash join carries.
/// Plain data handed in by lowering, so the operator has no planner dependency.
pub(crate) struct HashKeys {
    pub(crate) left_keys: Vec<Expr>,
    pub(crate) right_keys: Vec<Expr>,
    pub(crate) residual: Option<Expr>,
}
/// Build-row indices matching one key. The common unique-key case stays inline
/// so building the table does not allocate a `Vec` per key.
enum MatchList {
    One(usize),
    Many(Vec<usize>),
}

impl MatchList {
    fn push(&mut self, row: usize) {
        match self {
            MatchList::One(first) => *self = MatchList::Many(vec![*first, row]),
            MatchList::Many(rows) => rows.push(row),
        }
    }

    fn copy_into(&self, out: &mut Vec<usize>) {
        match self {
            MatchList::One(row) => out.push(*row),
            MatchList::Many(rows) => out.extend_from_slice(rows),
        }
    }
}
/// Left-deep nested-loop join. Materializes both children, then produces the
/// combined rows; LEFT/RIGHT keep unmatched rows with NULLs on the other side.
pub struct NestedLoopJoin {
    left: Box<dyn PhysicalOperator>,
    right: Box<dyn PhysicalOperator>,
    kind: JoinKind,
    condition: Option<Expr>,
    schema: Schema,
    rows: Vec<Vec<Value>>,
    pos: usize,
}

impl NestedLoopJoin {
    pub fn new(
        left: Box<dyn PhysicalOperator>,
        right: Box<dyn PhysicalOperator>,
        kind: JoinKind,
        condition: Option<Expr>,
    ) -> Result<Self> {
        if matches!(kind, JoinKind::Left | JoinKind::Right) && condition.is_none() {
            return Err(Error::Runtime("outer join requires an on clause".into()));
        }
        let mut schema = left.schema().clone();
        schema.columns.extend(right.schema().columns.iter().cloned());
        Ok(Self { left, right, kind, condition, schema, rows: Vec::new(), pos: 0 })
    }

    fn matches(&self, ctx: &mut ExecContext<'_>, row: &[Value]) -> Result<bool> {
        match &self.condition {
            None => Ok(true),
            Some(condition) => {
                eval_predicate_bound(ctx.db, ctx.trx, condition, &self.schema, row, ctx.outer)
            }
        }
    }
}

impl PhysicalOperator for NestedLoopJoin {
    fn schema(&self) -> &Schema {
        &self.schema
    }

    fn label(&self) -> String {
        format!("NestedLoopJoin kind={:?}", self.kind)
    }

    fn children(&self) -> Vec<&dyn PhysicalOperator> {
        vec![self.left.as_ref(), self.right.as_ref()]
    }

    fn open(&mut self, ctx: &mut ExecContext<'_>) -> Result<()> {
        let left_rows = drain(&mut self.left, ctx)?;
        let right_rows = drain(&mut self.right, ctx)?;
        let left_cols = self.left.schema().columns.len();
        let right_cols = self.right.schema().columns.len();

        let mut rows = Vec::new();
        match self.kind {
            JoinKind::Left => {
                for left in &left_rows {
                    let mut matched = false;
                    for right in &right_rows {
                        let mut row = left.clone();
                        row.extend(right.iter().cloned());
                        if self.matches(ctx, &row)? {
                            rows.push(row);
                            matched = true;
                        }
                    }
                    if !matched {
                        let mut row = left.clone();
                        row.extend(vec![Value::Null; right_cols]);
                        rows.push(row);
                    }
                }
            }
            JoinKind::Right => {
                for right in &right_rows {
                    let mut matched = false;
                    for left in &left_rows {
                        let mut row = left.clone();
                        row.extend(right.iter().cloned());
                        if self.matches(ctx, &row)? {
                            rows.push(row);
                            matched = true;
                        }
                    }
                    if !matched {
                        let mut row = vec![Value::Null; left_cols];
                        row.extend(right.iter().cloned());
                        rows.push(row);
                    }
                }
            }
            JoinKind::Cross | JoinKind::Inner => {
                for left in &left_rows {
                    for right in &right_rows {
                        let mut row = left.clone();
                        row.extend(right.iter().cloned());
                        if self.matches(ctx, &row)? {
                            rows.push(row);
                        }
                    }
                }
            }
        }
        self.rows = rows;
        self.pos = 0;
        Ok(())
    }

    fn next(&mut self, _ctx: &mut ExecContext<'_>) -> Result<Option<Vec<Value>>> {
        if self.pos >= self.rows.len() {
            return Ok(None);
        }
        let row = self.rows[self.pos].clone();
        self.pos += 1;
        Ok(Some(row))
    }

    fn close(&mut self) -> Result<()> {
        self.rows.clear();
        self.pos = 0;
        Ok(())
    }
}

/// Opens `op`, collects all its rows, and closes it.
pub(crate) fn drain(op: &mut Box<dyn PhysicalOperator>, ctx: &mut ExecContext<'_>) -> Result<Vec<Vec<Value>>> {
    op.open(ctx)?;
    let mut rows = Vec::new();
    while let Some(row) = op.next(ctx)? {
        rows.push(row);
    }
    op.close()?;
    Ok(rows)
}

/// Opens `op`, appends all its rows to `out` flat (row-major), and closes it.
/// The fused row path reuses one row buffer, so no `Vec` is allocated per row.
fn drain_into(
    op: &mut Box<dyn PhysicalOperator>,
    ctx: &mut ExecContext<'_>,
    out: &mut Vec<Value>,
) -> Result<()> {
    op.open(ctx)?;
    let fused = op.for_each_row(ctx, &mut |row| {
        out.extend_from_slice(row);
        Ok(())
    })?;
    if !fused {
        while let Some(row) = op.next(ctx)? {
            out.extend_from_slice(&row);
        }
    }
    op.close()
}

/// Appends one key component to `out`, length-prefixed so components cannot
/// run together. Returns `false` for a NULL value (the key never matches).
fn push_key_component(out: &mut Vec<u8>, value: &Value) -> Result<bool> {
    if matches!(value, Value::Null) {
        return Ok(false);
    }
    let start = out.len();
    out.extend_from_slice(&[0u8; 4]); // length prefix, patched below
    encode_key_into(out, value)?;
    let len = (out.len() - start - 4) as u32;
    out[start..start + 4].copy_from_slice(&len.to_le_bytes());
    Ok(true)
}

/// Encodes the columns at `indices` into `out` (cleared); `false` on a NULL.
fn indices_key_into(indices: &[usize], row: &[Value], out: &mut Vec<u8>) -> Result<bool> {
    out.clear();
    for &index in indices {
        if !push_key_component(out, &row[index])? {
            return Ok(false);
        }
    }
    Ok(true)
}

/// Encodes expression keys into `out` (cleared); `false` on a NULL value.
fn expr_key_into(
    keys: &[Expr],
    schema: &Schema,
    row: &[Value],
    db: &Database,
    trx: &mut crate::txn::trx::TrxState,
    out: &mut Vec<u8>,
) -> Result<bool> {
    out.clear();
    let ctx = EvalCtx::row(schema, row);
    for key in keys {
        let value = eval_bound(db, trx, key, Some(&ctx))?;
        if !push_key_component(out, &value)? {
            return Ok(false);
        }
    }
    Ok(true)
}

/// Resolves hash-join keys that are plain columns to their schema positions.
fn key_indices(keys: &[Expr], schema: &Schema) -> Option<Vec<usize>> {
    let mut indices = Vec::with_capacity(keys.len());
    for key in keys {
        let index = match key {
            Expr::Column(name) => schema.columns.iter().position(|c| &c.name == name)?,
            Expr::QualifiedColumn(owner, name) => schema
                .columns
                .iter()
                .position(|c| c.owner.as_deref() == Some(owner) && &c.name == name)?,
            _ => return None,
        };
        indices.push(index);
    }
    Some(indices)
}

/// Hash equi-join: builds a hash table on one side, then streams the other
/// side in chunks, preserving row order. Supports INNER/LEFT (build right)
/// and RIGHT (build left).
pub struct HashJoin {
    left: Box<dyn PhysicalOperator>,
    right: Box<dyn PhysicalOperator>,
    kind: JoinKind,
    left_keys: Vec<Expr>,
    right_keys: Vec<Expr>,
    residual: Option<Expr>,
    schema: Schema,
    table: HashMap<Vec<u8>, MatchList>,
    /// Build rows stored flat (row-major, `build_cols` per row) so building the
    /// table does not allocate a `Vec` per row.
    build_values: Vec<Value>,
    build_cols: usize,
    probe_is_right: bool,
    probe_keys: Vec<Expr>,
    probe_schema: Schema,
    probe_key_indices: Option<Vec<usize>>,
    probe_open: bool,
    probe_done: bool,
    pending: VecDeque<Vec<Value>>,
    left_cols: usize,
    right_cols: usize,
}

impl HashJoin {
    pub(crate) fn new(
        left: Box<dyn PhysicalOperator>,
        right: Box<dyn PhysicalOperator>,
        kind: JoinKind,
        keys: HashKeys,
    ) -> Self {
        let mut schema = left.schema().clone();
        schema.columns.extend(right.schema().columns.iter().cloned());
        Self {
            left,
            right,
            kind,
            left_keys: keys.left_keys,
            right_keys: keys.right_keys,
            residual: keys.residual,
            schema,
            table: HashMap::new(),
            build_values: Vec::new(),
            build_cols: 0,
            probe_is_right: false,
            probe_keys: Vec::new(),
            probe_schema: Schema::default(),
            probe_key_indices: None,
            probe_open: false,
            probe_done: false,
            pending: VecDeque::new(),
            left_cols: 0,
            right_cols: 0,
        }
    }

    /// The build row at `index`; build rows are stored flat with a fixed
    /// `build_cols` stride into `build_values`.
    fn build_row(&self, index: usize) -> &[Value] {
        let start = index * self.build_cols;
        &self.build_values[start..start + self.build_cols]
    }

    /// Encodes one probe row's join key into `out` (cleared); `false` means a
    /// NULL key (never matches).
    fn probe_key_into(
        &self,
        chunk: &Chunk,
        i: usize,
        ctx: &mut ExecContext<'_>,
        out: &mut Vec<u8>,
    ) -> Result<bool> {
        match &self.probe_key_indices {
            Some(indices) => {
                out.clear();
                for &index in indices {
                    let value = chunk.column(index).value(i);
                    if !push_key_component(out, &value)? {
                        return Ok(false);
                    }
                }
                Ok(true)
            }
            None => {
                let row = chunk.row(i);
                expr_key_into(&self.probe_keys, &self.probe_schema, &row, ctx.db, ctx.trx, out)
            }
        }
    }

    /// Pulls one probe chunk and appends its joined rows to `pending`.
    /// Returns `false` once the probe side is exhausted.
    fn fill_output(&mut self, ctx: &mut ExecContext<'_>) -> Result<bool> {
        if self.probe_done {
            return Ok(false);
        }
        if !self.probe_open {
            if self.probe_is_right {
                self.right.open(ctx)?;
            } else {
                self.left.open(ctx)?;
            }
            self.probe_open = true;
        }
        let pulled = {
            let probe: &mut Box<dyn PhysicalOperator> =
                if self.probe_is_right { &mut self.right } else { &mut self.left };
            probe.next_chunk(ctx)?
        };
        let Some(chunk) = pulled else {
            let probe: &mut Box<dyn PhysicalOperator> =
                if self.probe_is_right { &mut self.right } else { &mut self.left };
            probe.close()?;
            self.probe_open = false;
            self.probe_done = true;
            return Ok(false);
        };

        let outer = if self.probe_is_right {
            self.kind == JoinKind::Right
        } else {
            self.kind == JoinKind::Left
        };
        let mut key_buf = Vec::new();
        let mut match_buf: Vec<usize> = Vec::new();
        for i in 0..chunk.len() {
            let has_key = self.probe_key_into(&chunk, i, ctx, &mut key_buf)?;
            match_buf.clear();
            if has_key && let Some(list) = self.table.get(&key_buf) {
                list.copy_into(&mut match_buf);
            }
            if match_buf.is_empty() && !outer {
                continue;
            }
            let row = chunk.row(i);
            let mut matched = false;
            if !match_buf.is_empty() {
                for &build_index in &match_buf {
                    let mut combined = if self.probe_is_right {
                        self.build_row(build_index).to_vec()
                    } else {
                        row.clone()
                    };
                    if self.probe_is_right {
                        combined.extend(row.iter().cloned());
                    } else {
                        combined.extend_from_slice(self.build_row(build_index));
                    }
                    if residual_ok(&self.residual, &self.schema, ctx, &combined)? {
                        self.pending.push_back(combined);
                        matched = true;
                    }
                }
            }
            if !matched && outer {
                let combined = if self.probe_is_right {
                    let mut nulls = vec![Value::Null; self.left_cols];
                    nulls.extend(row.iter().cloned());
                    nulls
                } else {
                    let mut left = row.clone();
                    left.extend(vec![Value::Null; self.right_cols]);
                    left
                };
                self.pending.push_back(combined);
            }
        }
        Ok(true)
    }
}

fn residual_ok(
    residual: &Option<Expr>,
    schema: &Schema,
    ctx: &mut ExecContext<'_>,
    row: &[Value],
) -> Result<bool> {
    match residual {
        None => Ok(true),
        Some(predicate) => {
            eval_predicate_bound(ctx.db, ctx.trx, predicate, schema, row, ctx.outer)
        }
    }
}

impl PhysicalOperator for HashJoin {
    fn schema(&self) -> &Schema {
        &self.schema
    }

    fn label(&self) -> String {
        format!("HashJoin kind={:?}", self.kind)
    }

    fn children(&self) -> Vec<&dyn PhysicalOperator> {
        vec![self.left.as_ref(), self.right.as_ref()]
    }

    fn open(&mut self, ctx: &mut ExecContext<'_>) -> Result<()> {
        self.pending.clear();
        self.table.clear();
        self.build_values.clear();
        self.probe_open = false;
        self.probe_done = false;
        self.left_cols = self.left.schema().columns.len();
        self.right_cols = self.right.schema().columns.len();

        let build_is_right = self.kind != JoinKind::Right;
        self.probe_is_right = !build_is_right;

        let (build_schema, build_keys) = if build_is_right {
            (self.right.schema().clone(), self.right_keys.clone())
        } else {
            (self.left.schema().clone(), self.left_keys.clone())
        };
        self.build_cols = build_schema.columns.len();
        {
            let build_op = if build_is_right { &mut self.right } else { &mut self.left };
            drain_into(build_op, ctx, &mut self.build_values)?;
        }
        let build_key_indices = key_indices(&build_keys, &build_schema);
        let rows = self.build_values.len().checked_div(self.build_cols).unwrap_or(0);
        self.table.reserve(rows);
        let mut key_buf = Vec::new();
        for i in 0..rows {
            let row = &self.build_values[i * self.build_cols..(i + 1) * self.build_cols];
            let has_key = match &build_key_indices {
                Some(indices) => indices_key_into(indices, row, &mut key_buf)?,
                None => {
                    expr_key_into(&build_keys, &build_schema, row, ctx.db, ctx.trx, &mut key_buf)?
                }
            };
            if has_key {
                self.table
                    .entry(key_buf.clone())
                    .and_modify(|list| list.push(i))
                    .or_insert(MatchList::One(i));
            }
        }

        if self.probe_is_right {
            self.probe_keys = self.right_keys.clone();
            self.probe_schema = self.right.schema().clone();
        } else {
            self.probe_keys = self.left_keys.clone();
            self.probe_schema = self.left.schema().clone();
        }
        self.probe_key_indices = key_indices(&self.probe_keys, &self.probe_schema);
        Ok(())
    }

    fn next(&mut self, ctx: &mut ExecContext<'_>) -> Result<Option<Vec<Value>>> {
        loop {
            if let Some(row) = self.pending.pop_front() {
                return Ok(Some(row));
            }
            if !self.fill_output(ctx)? {
                return Ok(None);
            }
        }
    }

    fn next_chunk(&mut self, ctx: &mut ExecContext<'_>) -> Result<Option<Chunk>> {
        loop {
            if !self.pending.is_empty() {
                let take = self.pending.len().min(CHUNK_ROWS);
                let rows: Vec<Vec<Value>> = self.pending.drain(..take).collect();
                return Ok(Some(Chunk::from_rows(&rows)?));
            }
            if !self.fill_output(ctx)? {
                return Ok(None);
            }
        }
    }

    fn for_each_row(
        &mut self,
        ctx: &mut ExecContext<'_>,
        sink: &mut dyn FnMut(&[Value]) -> Result<()>,
    ) -> Result<bool> {
        if self.probe_done {
            return Ok(true);
        }
        if !self.probe_open {
            let probe: &mut Box<dyn PhysicalOperator> =
                if self.probe_is_right { &mut self.right } else { &mut self.left };
            probe.open(ctx)?;
            self.probe_open = true;
        }
        let outer = if self.probe_is_right {
            self.kind == JoinKind::Right
        } else {
            self.kind == JoinKind::Left
        };
        let probe_cols = self.probe_schema.columns.len();
        let mut key_buf = Vec::new();
        let mut match_buf: Vec<usize> = Vec::new();
        let mut combined: Vec<Value> = Vec::with_capacity(self.left_cols + self.right_cols);
        loop {
            let pulled = {
                let probe: &mut Box<dyn PhysicalOperator> =
                    if self.probe_is_right { &mut self.right } else { &mut self.left };
                probe.next_chunk(ctx)?
            };
            let Some(chunk) = pulled else { break };
            for i in 0..chunk.len() {
                let has_key = self.probe_key_into(&chunk, i, ctx, &mut key_buf)?;
                match_buf.clear();
                if has_key && let Some(list) = self.table.get(&key_buf) {
                    list.copy_into(&mut match_buf);
                }
                if match_buf.is_empty() && !outer {
                    continue;
                }
                let mut matched = false;
                if !match_buf.is_empty() {
                    for &build_index in &match_buf {
                        combined.clear();
                        if self.probe_is_right {
                            combined.extend_from_slice(self.build_row(build_index));
                            for c in 0..probe_cols {
                                combined.push(chunk.column(c).value(i));
                            }
                        } else {
                            for c in 0..probe_cols {
                                combined.push(chunk.column(c).value(i));
                            }
                            combined.extend_from_slice(self.build_row(build_index));
                        }
                        if residual_ok(&self.residual, &self.schema, ctx, &combined)? {
                            sink(&combined)?;
                            matched = true;
                        }
                    }
                }
                if !matched && outer {
                    combined.clear();
                    if self.probe_is_right {
                        combined.extend(std::iter::repeat_n(Value::Null, self.left_cols));
                        for c in 0..probe_cols {
                            combined.push(chunk.column(c).value(i));
                        }
                    } else {
                        for c in 0..probe_cols {
                            combined.push(chunk.column(c).value(i));
                        }
                        combined.extend(std::iter::repeat_n(Value::Null, self.right_cols));
                    }
                    sink(&combined)?;
                }
            }
        }
        if self.probe_open {
            let probe: &mut Box<dyn PhysicalOperator> =
                if self.probe_is_right { &mut self.right } else { &mut self.left };
            probe.close()?;
            self.probe_open = false;
        }
        self.probe_done = true;
        Ok(true)
    }

    fn close(&mut self) -> Result<()> {
        if self.probe_open {
            let probe: &mut Box<dyn PhysicalOperator> =
                if self.probe_is_right { &mut self.right } else { &mut self.left };
            probe.close()?;
            self.probe_open = false;
        }
        self.table.clear();
        self.build_values.clear();
        self.pending.clear();
        Ok(())
    }
}
