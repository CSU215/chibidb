use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum BinOp {
    Add,
    Sub,
    Mul,
    Div,
    Mod,
    Eq,
    NotEq,
    Lt,
    Le,
    Gt,
    Ge,
    And,
    Or,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum UnOp {
    Neg,
    Not,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum AggFunc {
    Count,
    Sum,
    Avg,
    Min,
    Max,
}

impl fmt::Display for AggFunc {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            AggFunc::Count => "count",
            AggFunc::Sum => "sum",
            AggFunc::Avg => "avg",
            AggFunc::Min => "min",
            AggFunc::Max => "max",
        };
        f.write_str(s)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Expr {
    Int(i64),
    Float(f64),
    Str(String),
    Null,
    Column(String),
    Unary(UnOp, Box<Expr>),
    Binary(BinOp, Box<Expr>, Box<Expr>),
    IsNull(Box<Expr>, bool),
    /// `expr [NOT] LIKE pattern [ESCAPE c]`; `%`/`_` wildcards.
    Like { expr: Box<Expr>, pattern: Box<Expr>, negated: bool, escape: Option<char> },
    /// Scalar function call, e.g. `concat(a, b)`. Name is lowercased.
    Function(String, Vec<Expr>),
    /// `func([DISTINCT] expr)`; the flag marks `DISTINCT`.
    Aggregate(AggFunc, Option<Box<Expr>>, bool),
    QualifiedColumn(String, String),
    /// A materialized literal produced by lifting subqueries.
    Value(crate::value::Value),
    /// `[NOT] IN (SELECT ...)`; materialized before row evaluation.
    InSubquery { expr: Box<Expr>, sub: Box<SelectStmt>, negated: bool },
    /// `EXISTS (SELECT ...)` / `NOT EXISTS`; materialized before evaluation.
    Exists { sub: Box<SelectStmt> },
    /// A scalar `(SELECT ...)`; materialized to a single value.
    ScalarSubquery(Box<SelectStmt>),
}

impl fmt::Display for Expr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Expr::Int(n) => write!(f, "{n}"),
            Expr::Float(x) => write!(f, "{x}"),
            Expr::Str(s) => write!(f, "'{s}'"),
            Expr::Null => write!(f, "NULL"),
            Expr::Column(c) => write!(f, "{c}"),
            Expr::Unary(op, e) => write!(f, "({op} {e})"),
            Expr::Binary(op, l, r) => write!(f, "({op} {l} {r})"),
            Expr::IsNull(e, false) => write!(f, "(is-null {e})"),
            Expr::IsNull(e, true) => write!(f, "(is-not-null {e})"),
            Expr::Like { expr, pattern, negated, escape } => {
                let head = if *negated { "not-like" } else { "like" };
                match escape {
                    Some(c) => write!(f, "({head} {expr} {pattern} escape {c})"),
                    None => write!(f, "({head} {expr} {pattern})"),
                }
            }
            Expr::Function(name, args) => {
                write!(f, "({name}")?;
                for a in args {
                    write!(f, " {a}")?;
                }
                f.write_str(")")
            }
            Expr::Aggregate(func, None, _) => write!(f, "({func} *)"),
            Expr::Aggregate(func, Some(e), false) => write!(f, "({func} {e})"),
            Expr::Aggregate(func, Some(e), true) => write!(f, "({func} distinct {e})"),
            Expr::QualifiedColumn(t, c) => write!(f, "{t}.{c}"),
            Expr::Value(v) => write!(f, "{v}"),
            Expr::InSubquery { .. } => write!(f, "(in-subquery)"),
            Expr::Exists { .. } => write!(f, "(exists)"),
            Expr::ScalarSubquery(_) => write!(f, "(subquery)"),
        }
    }
}

impl fmt::Display for BinOp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            BinOp::Add => "+",
            BinOp::Sub => "-",
            BinOp::Mul => "*",
            BinOp::Div => "/",
            BinOp::Mod => "%",
            BinOp::Eq => "=",
            BinOp::NotEq => "<>",
            BinOp::Lt => "<",
            BinOp::Le => "<=",
            BinOp::Gt => ">",
            BinOp::Ge => ">=",
            BinOp::And => "and",
            BinOp::Or => "or",
        };
        f.write_str(s)
    }
}

impl fmt::Display for UnOp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            UnOp::Neg => "-",
            UnOp::Not => "not",
        };
        f.write_str(s)
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum DataType {
    Int,
    Float,
    Char(u32),
    Date,
    Text,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ColumnDef {
    pub name: String,
    pub dtype: DataType,
    pub not_null: bool,
    pub primary_key: bool,
    pub unique: bool,
    pub default: Option<Expr>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct CreateTableStmt {
    pub name: String,
    pub columns: Vec<ColumnDef>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct InsertStmt {
    pub table: String,
    /// Optional explicit column list: `INSERT INTO t (a, b) VALUES ...`.
    pub columns: Option<Vec<String>>,
    pub rows: Vec<Vec<Expr>>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct DeleteStmt {
    pub table: String,
    pub selection: Option<Expr>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct UpdateStmt {
    pub table: String,
    pub assignments: Vec<(String, Expr)>,
    pub selection: Option<Expr>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct CreateIndexStmt {
    pub name: String,
    pub table: String,
    pub column: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct DropIndexStmt {
    pub name: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct DropTableStmt {
    pub name: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct CreateViewStmt {
    pub name: String,
    /// The original select text after AS, stored verbatim in the catalog.
    pub sql: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct DropViewStmt {
    pub name: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct CreateDatabaseStmt {
    pub name: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct DropDatabaseStmt {
    pub name: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct UseStmt {
    pub name: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ExplainStmt {
    pub stmt: Box<Stmt>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum TrxCtl {
    Begin,
    Commit,
    Rollback,
}

#[derive(Debug, Clone, PartialEq)]
pub struct TableRef {
    pub name: String,
    pub alias: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum SelectItem {
    Star,
    Expr(Expr),
    Aliased(Expr, String),
}

#[derive(Debug, Clone, PartialEq)]
pub struct Limit {
    pub count: Expr,
    pub offset: Option<Expr>,
}

/// How each FROM entry (after the first) is attached to the accumulation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JoinKind {
    /// comma list: cartesian product
    Cross,
    /// JOIN ... ON / INNER JOIN ... ON
    Inner,
    /// LEFT [OUTER] JOIN ... ON: unmatched left rows survive with NULLs
    Left,
    /// RIGHT [OUTER] JOIN ... ON: unmatched right rows survive with NULLs
    Right,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SelectStmt {
    pub distinct: bool,
    pub items: Vec<SelectItem>,
    pub from: Vec<TableRef>,
    /// one per `from` entry; the first is a placeholder
    pub joins: Vec<JoinKind>,
    pub on: Vec<Expr>,
    pub selection: Option<Expr>,
    pub group_by: Vec<Expr>,
    pub having: Option<Expr>,
    pub order_by: Vec<(Expr, bool)>,
    pub limit: Option<Limit>,
    /// UNION [ALL] operands, applied left-to-right after this select's body.
    /// `bool` is `all` (no dedup).
    pub set_ops: Vec<(bool, Box<SelectStmt>)>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Stmt {
    CreateDatabase(CreateDatabaseStmt),
    CreateIndex(CreateIndexStmt),
    CreateTable(CreateTableStmt),
    CreateView(CreateViewStmt),
    Delete(DeleteStmt),
    DropDatabase(DropDatabaseStmt),
    DropIndex(DropIndexStmt),
    DropTable(DropTableStmt),
    DropView(DropViewStmt),
    Explain(ExplainStmt),
    Insert(InsertStmt),
    Select(Box<SelectStmt>),
    Update(UpdateStmt),
    Use(UseStmt),
    Trx(TrxCtl),
    Checkpoint,
    Vacuum,
}
