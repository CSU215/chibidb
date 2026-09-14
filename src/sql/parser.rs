use crate::config::{EngineKind, PageLayout};
use crate::error::{Error, Result};
use crate::sql::ast::{
    AggFunc, BinOp, ColumnDef, CreateDatabaseStmt, CreateIndexStmt, CreateTableStmt,
    CreateUserStmt, CreateViewStmt, DeleteStmt, DropDatabaseStmt, DropIndexStmt, DropTableStmt,
    DropUserStmt, DropViewStmt, ExplainStmt, Expr, GrantStmt, InsertStmt, JoinKind, Limit,
    LoginStmt, Privilege, RevokeStmt, SelectItem, SelectStmt, ShowColumnsStmt, Stmt, TableRef,
    TrxCtl, UnOp, UpdateStmt, UseStmt,
};
use crate::sql::lexer::{Punct, Token, TokenKind, lex};
use crate::value::DataType;

/// Parses one or more `;`-separated statements. Positions in errors are byte
/// offsets into `sql`.
pub fn parse(sql: &str) -> Result<Vec<Stmt>> {
    let tokens = lex(sql)?;
    Parser { tokens, src: sql, pos: 0 }.parse_statements()
}

struct Parser<'a> {
    tokens: Vec<Token>,
    src: &'a str,
    pos: usize,
}

impl Parser<'_> {
    fn peek(&self) -> Option<&Token> {
        self.tokens.get(self.pos)
    }

    fn kind_at(&self, offset: usize) -> Option<&TokenKind> {
        self.tokens.get(self.pos + offset).map(|t| &t.kind)
    }

    fn bump(&mut self) -> Option<&Token> {
        let t = self.tokens.get(self.pos);
        if t.is_some() {
            self.pos += 1;
        }
        t
    }

    fn at_punct(&self, p: Punct) -> bool {
        matches!(self.kind_at(0), Some(TokenKind::Punct(q)) if *q == p)
    }

    fn eat_punct(&mut self, p: Punct) -> bool {
        if self.at_punct(p) {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    fn expect_punct(&mut self, p: Punct) -> Result<()> {
        if self.eat_punct(p) { Ok(()) } else { Err(self.unexpected("punctuation")) }
    }

    fn at_keyword(&self, kw: &str) -> bool {
        matches!(self.kind_at(0), Some(TokenKind::Ident(s)) if s.eq_ignore_ascii_case(kw))
    }

    fn eat_keyword(&mut self, kw: &str) -> bool {
        if self.at_keyword(kw) {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    /// The byte offset to point at for the token under the cursor. Past the end
    /// of the input that is the end of the text, which is where the cursor
    /// actually is -- the place that is missing something.
    fn here(&self) -> usize {
        self.peek().map_or(self.src.len(), |t| t.pos)
    }

    fn syntax(&self, message: impl Into<String>) -> Error {
        Error::syntax(message, self.here())
    }

    fn unexpected(&self, want: &str) -> Error {
        let got = match self.peek() {
            None => "end of input".to_string(),
            Some(t) => format!("{:?}", t.kind),
        };
        self.syntax(format!("expected {want}, got {got}"))
    }

    fn parse_statements(&mut self) -> Result<Vec<Stmt>> {
        let mut out = Vec::new();
        while self.eat_punct(Punct::Semicolon) {}
        while self.peek().is_some() {
            out.push(self.parse_statement()?);
            if self.peek().is_some() {
                self.expect_punct(Punct::Semicolon)?;
            }
            while self.eat_punct(Punct::Semicolon) {}
        }
        Ok(out)
    }

    fn parse_statement(&mut self) -> Result<Stmt> {
        if self.eat_keyword("begin") {
            return Ok(Stmt::Trx(TrxCtl::Begin));
        }
        if self.eat_keyword("start") {
            if !self.eat_keyword("transaction") {
                return Err(self.unexpected("transaction"));
            }
            // accepted and ignored characteristics
            if self.eat_keyword("read") {
                let _ = self.eat_keyword("only") || self.eat_keyword("write");
            } else if self.eat_keyword("with") {
                let _ = self.eat_keyword("consistent");
                let _ = self.eat_keyword("snapshot");
            }
            return Ok(Stmt::Trx(TrxCtl::Begin));
        }
        if self.eat_keyword("commit") {
            return Ok(Stmt::Trx(TrxCtl::Commit));
        }
        if self.eat_keyword("rollback") {
            return Ok(Stmt::Trx(TrxCtl::Rollback));
        }
        if self.eat_keyword("checkpoint") {
            return Ok(Stmt::Checkpoint);
        }
        if self.eat_keyword("vacuum") {
            return Ok(Stmt::Vacuum);
        }
        if self.eat_keyword("show") {
            if self.eat_keyword("tables") {
                return Ok(Stmt::ShowTables);
            }
            if self.eat_keyword("database") || self.eat_keyword("databases") {
                return Ok(Stmt::ShowDatabases);
            }
            if self.eat_keyword("columns") {
                if !self.eat_keyword("from") && !self.eat_keyword("in") {
                    return Err(self.unexpected("from"));
                }
                return Ok(Stmt::ShowColumns(ShowColumnsStmt { table: self.parse_ident("table name")? }));
            }
            return Err(self.unexpected("tables, databases or columns"));
        }
        if self.eat_keyword("describe") || self.eat_keyword("desc") {
            return Ok(Stmt::ShowColumns(ShowColumnsStmt { table: self.parse_ident("table name")? }));
        }
        if self.eat_keyword("explain") {
            if !self.at_keyword("select") {
                return Err(self.unexpected("select after explain"));
            }
            let inner = self.parse_statement()?;
            return Ok(Stmt::Explain(ExplainStmt { stmt: Box::new(inner) }));
        }
        if self.eat_keyword("login") {
            let name = self.parse_name_literal("user name")?;
            self.expect_keyword("identified")?;
            self.expect_keyword("by")?;
            let password = self.parse_string("password")?;
            return Ok(Stmt::Login(LoginStmt { name, password }));
        }
        if self.eat_keyword("select") {
            return self.parse_select();
        }
        if self.eat_keyword("create") {
            return self.parse_create();
        }
        if self.eat_keyword("drop") {
            if self.eat_keyword("database") {
                return Ok(Stmt::DropDatabase(DropDatabaseStmt { name: self.parse_ident("database name")? }));
            }
            if self.eat_keyword("user") {
                return Ok(Stmt::DropUser(DropUserStmt {
                    name: self.parse_name_literal("user name")?,
                }));
            }
            if self.eat_keyword("index") {
                return Ok(Stmt::DropIndex(DropIndexStmt { name: self.parse_ident("index name")? }));
            }
            if self.eat_keyword("view") {
                return Ok(Stmt::DropView(DropViewStmt { name: self.parse_ident("view name")? }));
            }
            if !self.eat_keyword("table") {
                return Err(self.unexpected("index, view or table"));
            }
            return Ok(Stmt::DropTable(DropTableStmt { name: self.parse_ident("table name")? }));
        }
        if self.eat_keyword("use") {
            return Ok(Stmt::Use(UseStmt { name: self.parse_ident("database name")? }));
        }
        if self.eat_keyword("grant") {
            let privileges = self.parse_privileges()?;
            self.expect_keyword("on")?;
            let database = self.parse_scope()?;
            self.expect_keyword("to")?;
            let user = self.parse_name_literal("user name")?;
            return Ok(Stmt::Grant(GrantStmt { privileges, database, user }));
        }
        if self.eat_keyword("revoke") {
            let privileges = self.parse_privileges()?;
            self.expect_keyword("on")?;
            let database = self.parse_scope()?;
            self.expect_keyword("from")?;
            let user = self.parse_name_literal("user name")?;
            return Ok(Stmt::Revoke(RevokeStmt { privileges, database, user }));
        }
        if self.eat_keyword("insert") {
            if !self.eat_keyword("into") {
                return Err(self.unexpected("into"));
            }
            return self.parse_insert();
        }
        if self.eat_keyword("delete") {
            if !self.eat_keyword("from") {
                return Err(self.unexpected("from"));
            }
            let table = self.parse_ident("table name")?;
            let selection = self.parse_where()?;
            return Ok(Stmt::Delete(DeleteStmt { table, selection }));
        }
        if self.eat_keyword("update") {
            let table = self.parse_ident("table name")?;
            self.expect_keyword("set")?;
            let mut assignments = Vec::new();
            loop {
                let col = self.parse_ident("column name")?;
                self.expect_punct(Punct::Eq)?;
                assignments.push((col, self.parse_expr()?));
                if !self.eat_punct(Punct::Comma) {
                    break;
                }
            }
            let selection = self.parse_where()?;
            return Ok(Stmt::Update(UpdateStmt { table, assignments, selection }));
        }
        Err(self.unexpected("statement"))
    }

    fn expect_keyword(&mut self, kw: &str) -> Result<()> {
        if self.eat_keyword(kw) { Ok(()) } else { Err(self.unexpected(kw)) }
    }

    fn parse_create(&mut self) -> Result<Stmt> {
        if self.eat_keyword("database") {
            return Ok(Stmt::CreateDatabase(CreateDatabaseStmt {
                name: self.parse_ident("database name")?,
            }));
        }
        if self.eat_keyword("user") {
            let name = self.parse_name_literal("user name")?;
            self.expect_keyword("identified")?;
            self.expect_keyword("by")?;
            return Ok(Stmt::CreateUser(CreateUserStmt {
                name,
                password: self.parse_string("password")?,
            }));
        }
        if self.eat_keyword("view") {
            let name = self.parse_ident("view name")?;
            self.expect_keyword("as")?;
            // Keep the original select text for the catalog.
            let start = self
                .tokens
                .get(self.pos)
                .map(|t| t.pos)
                .ok_or_else(|| self.unexpected("select after as"))?;
            let stmt = self.parse_statement()?;
            let end = self.tokens.get(self.pos).map(|t| t.pos).unwrap_or(self.src.len());
            let sql = self.src[start..end].trim_end().to_string();
            return match stmt {
                Stmt::Select(_) => Ok(Stmt::CreateView(CreateViewStmt { name, sql })),
                _ => Err(self.unexpected("select after as")),
            };
        }
        if self.eat_keyword("index") {
            let name = self.parse_ident("index name")?;
            self.expect_keyword("on")?;
            let table = self.parse_ident("table name")?;
            self.expect_punct(Punct::LParen)?;
            let column = self.parse_ident("column name")?;
            self.expect_punct(Punct::RParen)?;
            return Ok(Stmt::CreateIndex(CreateIndexStmt { name, table, column }));
        }
        if !self.eat_keyword("table") {
            return Err(self.unexpected("table"));
        }
        self.parse_create_table()
    }

    fn parse_where(&mut self) -> Result<Option<Expr>> {
        if self.eat_keyword("where") { Ok(Some(self.parse_expr()?)) } else { Ok(None) }
    }

    const RESERVED: &'static [&'static str] = &[
        "where", "group", "having", "order", "limit", "on", "join", "inner", "left", "right",
        "outer", "in", "exists", "distinct", "vacuum", "like", "union",
    ];

    fn agg_func(name: &str) -> Option<AggFunc> {
        for (kw, func) in [
            ("count", AggFunc::Count),
            ("sum", AggFunc::Sum),
            ("avg", AggFunc::Avg),
            ("min", AggFunc::Min),
            ("max", AggFunc::Max),
        ] {
            if name.eq_ignore_ascii_case(kw) {
                return Some(func);
            }
        }
        None
    }

    fn at_reserved(&self) -> bool {
        matches!(self.kind_at(0), Some(TokenKind::Ident(s))
            if Self::RESERVED.contains(&s.to_ascii_lowercase().as_str()))
    }

    fn parse_ident(&mut self, want: &str) -> Result<String> {
        match self.bump() {
            Some(Token { kind: TokenKind::Ident(s), .. }) => Ok(s.clone()),
            _ => Err(self.unexpected(want)),
        }
    }

    /// Accepts a bare identifier or a quoted string, for names that may be
    /// written either way (`create user alice` / `create user 'alice'`).
    fn parse_name_literal(&mut self, want: &str) -> Result<String> {
        match self.bump() {
            Some(Token { kind: TokenKind::Ident(s) | TokenKind::Str(s), .. }) => Ok(s.clone()),
            _ => Err(self.unexpected(want)),
        }
    }

    fn parse_string(&mut self, want: &str) -> Result<String> {
        match self.bump() {
            Some(Token { kind: TokenKind::Str(s), .. }) => Ok(s.clone()),
            _ => Err(self.unexpected(want)),
        }
    }

    fn parse_privileges(&mut self) -> Result<Vec<Privilege>> {
        let mut out = Vec::new();
        loop {
            if self.eat_keyword("all") {
                out.push(Privilege::Read);
                out.push(Privilege::Write);
            } else if self.eat_keyword("read") {
                out.push(Privilege::Read);
            } else if self.eat_keyword("write") {
                out.push(Privilege::Write);
            } else {
                return Err(self.unexpected("privilege (read/write/all)"));
            }
            if !self.eat_punct(Punct::Comma) {
                break;
            }
        }
        Ok(out)
    }

    fn parse_scope(&mut self) -> Result<String> {
        if self.eat_punct(Punct::Star) {
            return Ok("*".to_string());
        }
        self.parse_ident("database name")
    }

    fn parse_select(&mut self) -> Result<Stmt> {
        let mut first = self.parse_select_core()?;
        while self.eat_keyword("union") {
            let all = self.eat_keyword("all");
            if !self.eat_keyword("select") {
                return Err(self.unexpected("select after union"));
            }
            first.set_ops.push((all, Box::new(self.parse_select_core()?)));
        }
        // trailing ORDER BY / LIMIT belong to the whole set operation
        if self.eat_keyword("order") {
            self.expect_keyword("by")?;
            loop {
                let expr = self.parse_expr()?;
                let desc = if self.eat_keyword("desc") {
                    true
                } else {
                    self.eat_keyword("asc");
                    false
                };
                first.order_by.push((expr, desc));
                if !self.eat_punct(Punct::Comma) {
                    break;
                }
            }
        }
        if self.eat_keyword("limit") {
            // Captured before the expression is consumed: the complaint is about
            // what was written here, not about whatever follows it.
            let count_at = self.here();
            let count = self.parse_expr()?;
            if !matches!(count, Expr::Int(_)) {
                return Err(Error::syntax("limit count must be a non-negative integer", count_at));
            }
            let offset = if self.eat_keyword("offset") {
                let offset_at = self.here();
                let off = self.parse_expr()?;
                if !matches!(off, Expr::Int(_)) {
                    return Err(Error::syntax(
                        "limit offset must be a non-negative integer",
                        offset_at,
                    ));
                }
                Some(off)
            } else {
                None
            };
            first.limit = Some(Limit { count, offset });
        }
        Ok(Stmt::Select(Box::new(first)))
    }

    fn parse_select_core(&mut self) -> Result<SelectStmt> {
        let distinct = self.eat_keyword("distinct");
        let mut items = Vec::new();
        loop {
            if self.eat_punct(Punct::Star) {
                items.push(SelectItem::Star);
            } else {
                let expr = self.parse_expr()?;
                if self.eat_keyword("as") {
                    items.push(SelectItem::Aliased(expr, self.parse_ident("alias")?));
                } else {
                    items.push(SelectItem::Expr(expr));
                }
            }
            if !self.eat_punct(Punct::Comma) {
                break;
            }
        }
        let mut from = Vec::new();
        let mut on = Vec::new();
        let mut joins = Vec::new();
        if self.eat_keyword("from") {
            from.push(self.parse_table_ref()?);
            joins.push(JoinKind::Cross);
            loop {
                if self.eat_punct(Punct::Comma) {
                    from.push(self.parse_table_ref()?);
                    joins.push(JoinKind::Cross);
                    continue;
                }
                if self.eat_keyword("inner") && !self.at_keyword("join") {
                    return Err(self.unexpected("join"));
                }
                let kind = if self.eat_keyword("left") {
                    self.eat_keyword("outer");
                    self.expect_keyword("join")?;
                    JoinKind::Left
                } else if self.eat_keyword("right") {
                    self.eat_keyword("outer");
                    self.expect_keyword("join")?;
                    JoinKind::Right
                } else if self.eat_keyword("join") {
                    JoinKind::Inner
                } else {
                    break;
                };
                from.push(self.parse_table_ref()?);
                joins.push(kind);
                self.expect_keyword("on")?;
                on.push(self.parse_expr()?);
            }
        }
        let selection = if self.eat_keyword("where") { Some(self.parse_expr()?) } else { None };
        let group_by = if self.eat_keyword("group") {
            self.expect_keyword("by")?;
            let mut group_by = vec![self.parse_expr()?];
            while self.eat_punct(Punct::Comma) {
                group_by.push(self.parse_expr()?);
            }
            group_by
        } else {
            vec![]
        };
        let having = if self.eat_keyword("having") { Some(self.parse_expr()?) } else { None };
        Ok(SelectStmt {
            distinct,
            items,
            from,
            joins,
            on,
            selection,
            group_by,
            having,
            order_by: Vec::new(),
            limit: None,
            set_ops: Vec::new(),
        })
    }

    fn parse_table_ref(&mut self) -> Result<TableRef> {
        let name = self.parse_ident("table name")?;
        let alias = if self.eat_keyword("as") {
            Some(self.parse_ident("alias")?)
        } else if self.at_reserved() || !matches!(self.kind_at(0), Some(TokenKind::Ident(_))) {
            None
        } else {
            Some(self.parse_ident("alias")?)
        };
        Ok(TableRef { name, alias })
    }

    fn parse_insert(&mut self) -> Result<Stmt> {
        let table = self.parse_ident("table name")?;
        let mut columns = None;
        if self.eat_punct(Punct::LParen) {
            let mut cols = vec![self.parse_ident("column name")?];
            while self.eat_punct(Punct::Comma) {
                cols.push(self.parse_ident("column name")?);
            }
            self.expect_punct(Punct::RParen)?;
            columns = Some(cols);
        }
        self.expect_keyword("values")?;
        let mut rows = Vec::new();
        loop {
            self.expect_punct(Punct::LParen)?;
            let mut row = Vec::new();
            loop {
                row.push(self.parse_value()?);
                if !self.eat_punct(Punct::Comma) {
                    break;
                }
            }
            self.expect_punct(Punct::RParen)?;
            rows.push(row);
            if !self.eat_punct(Punct::Comma) {
                break;
            }
        }
        Ok(Stmt::Insert(InsertStmt { table, columns, rows }))
    }

    /// A literal value in an INSERT row: optional `-`, then int/float/string or
    /// the `null` keyword.
    fn parse_value(&mut self) -> Result<Expr> {
        let neg = self.eat_punct(Punct::Minus);
        if !neg && self.eat_keyword("null") {
            return Ok(Expr::Null);
        }
        match self.bump() {
            Some(Token { kind: TokenKind::Int(n), .. }) => Ok(Expr::Int(if neg { -*n } else { *n })),
            Some(Token { kind: TokenKind::Float(x), .. }) => {
                Ok(Expr::Float(if neg { -*x } else { *x }))
            }
            Some(Token { kind: TokenKind::Str(s), .. }) if !neg => Ok(Expr::Str(s.clone())),
            _ => Err(self.unexpected("value")),
        }
    }

    fn parse_create_table(&mut self) -> Result<Stmt> {
        let name = self.parse_ident("table name")?;
        self.expect_punct(Punct::LParen)?;
        let mut columns = Vec::new();
        loop {
            columns.push(self.parse_column_def()?);
            if !self.eat_punct(Punct::Comma) {
                break;
            }
        }
        self.expect_punct(Punct::RParen)?;
        let mut engine = None;
        let mut layout = None;
        loop {
            if self.eat_keyword("engine") {
                self.expect_punct(Punct::Eq)?;
                engine = Some(match self.bump() {
                    Some(Token { kind: TokenKind::Ident(s), .. }) if s.eq_ignore_ascii_case("heap") => {
                        EngineKind::Heap
                    }
                    Some(Token { kind: TokenKind::Ident(s), .. }) if s.eq_ignore_ascii_case("lsm") => {
                        EngineKind::Lsm
                    }
                    _ => return Err(self.unexpected("engine name (heap or lsm)")),
                });
            } else if self.eat_keyword("page_layout") {
                self.expect_punct(Punct::Eq)?;
                layout = Some(match self.bump() {
                    Some(Token { kind: TokenKind::Ident(s), .. }) if s.eq_ignore_ascii_case("row") => {
                        PageLayout::Row
                    }
                    Some(Token { kind: TokenKind::Ident(s), .. }) if s.eq_ignore_ascii_case("pax") => {
                        PageLayout::Pax
                    }
                    _ => return Err(self.unexpected("page layout (row or pax)")),
                });
            } else {
                break;
            }
        }
        Ok(Stmt::CreateTable(CreateTableStmt { name, columns, engine, layout }))
    }

    fn parse_column_def(&mut self) -> Result<ColumnDef> {
        let name = self.parse_ident("column name")?;
        let dtype = self.parse_data_type()?;
        let mut not_null = false;
        let mut primary_key = false;
        let mut unique = false;
        let mut default = None;
        loop {
            if self.eat_keyword("primary") {
                self.expect_keyword("key")?;
                primary_key = true;
                not_null = true;
                unique = true;
            } else if self.eat_keyword("unique") {
                unique = true;
            } else if self.eat_keyword("not") {
                self.expect_keyword("null")?;
                not_null = true;
            } else if self.eat_keyword("default") {
                default = Some(self.parse_value()?);
            } else if self.eat_keyword("null") {
                // explicit "null" column attribute is the default behavior
            } else {
                break;
            }
        }
        Ok(ColumnDef { name, dtype, not_null, primary_key, unique, default })
    }

    fn parse_data_type(&mut self) -> Result<DataType> {
        if self.eat_keyword("int") {
            return Ok(DataType::Int);
        }
        if self.eat_keyword("float") {
            return Ok(DataType::Float);
        }
        if self.eat_keyword("char") {
            self.expect_punct(Punct::LParen)?;
            let length_at = self.here();
            let n = match self.bump() {
                Some(Token { kind: TokenKind::Int(n), .. }) => *n,
                _ => return Err(self.unexpected("char length")),
            };
            if n <= 0 {
                return Err(Error::syntax("char length must be positive", length_at));
            }
            self.expect_punct(Punct::RParen)?;
            return Ok(DataType::Char(n as u32));
        }
        if self.eat_keyword("date") {
            return Ok(DataType::Date);
        }
        if self.eat_keyword("text") {
            return Ok(DataType::Text);
        }
        Err(self.unexpected("data type"))
    }

    fn parse_expr(&mut self) -> Result<Expr> {
        self.parse_or()
    }

    fn parse_or(&mut self) -> Result<Expr> {
        let mut lhs = self.parse_and()?;
        while self.eat_keyword("or") {
            let rhs = self.parse_and()?;
            lhs = Expr::Binary(BinOp::Or, Box::new(lhs), Box::new(rhs));
        }
        Ok(lhs)
    }

    fn parse_and(&mut self) -> Result<Expr> {
        let mut lhs = self.parse_not()?;
        while self.eat_keyword("and") {
            let rhs = self.parse_not()?;
            lhs = Expr::Binary(BinOp::And, Box::new(lhs), Box::new(rhs));
        }
        Ok(lhs)
    }

    fn parse_not(&mut self) -> Result<Expr> {
        if self.eat_keyword("not") {
            return Ok(Expr::Unary(UnOp::Not, Box::new(self.parse_not()?)));
        }
        self.parse_comparison()
    }

    fn parse_comparison(&mut self) -> Result<Expr> {
        let lhs = self.parse_additive()?;
        if self.eat_keyword("is") {
            let negated = self.eat_keyword("not");
            self.expect_keyword("null")?;
            return Ok(Expr::IsNull(Box::new(lhs), negated));
        }
        // [NOT] IN (value list | subquery)
        if self.at_keyword("in") || (self.at_keyword("not") && self.next_is_keyword("in")) {
            let negated = self.eat_keyword("not");
            self.expect_keyword("in")?;
            return self.finish_in(lhs, negated);
        }
        // [NOT] LIKE pattern [ESCAPE c]
        if self.at_keyword("like") || (self.at_keyword("not") && self.next_is_keyword("like")) {
            let negated = self.eat_keyword("not");
            self.expect_keyword("like")?;
            let pattern = self.parse_additive()?;
            let escape = if self.eat_keyword("escape") {
                let escape_at = self.here();
                match self.bump() {
                    Some(Token { kind: TokenKind::Str(s), .. }) if s.chars().count() == 1 => {
                        s.chars().next()
                    }
                    _ => {
                        return Err(Error::syntax(
                            "ESCAPE requires a single-character string",
                            escape_at,
                        ))
                    }
                }
            } else {
                None
            };
            return Ok(Expr::Like {
                expr: Box::new(lhs),
                pattern: Box::new(pattern),
                negated,
                escape,
            });
        }
        let op = match self.peek().map(|t| &t.kind) {
            Some(TokenKind::Punct(Punct::Eq)) => BinOp::Eq,
            Some(TokenKind::Punct(Punct::NotEq)) => BinOp::NotEq,
            Some(TokenKind::Punct(Punct::Lt)) => BinOp::Lt,
            Some(TokenKind::Punct(Punct::Le)) => BinOp::Le,
            Some(TokenKind::Punct(Punct::Gt)) => BinOp::Gt,
            Some(TokenKind::Punct(Punct::Ge)) => BinOp::Ge,
            _ => return Ok(lhs),
        };
        self.pos += 1;
        Ok(Expr::Binary(op, Box::new(lhs), Box::new(self.parse_additive()?)))
    }

    fn next_is_keyword(&self, kw: &str) -> bool {
        matches!(self.kind_at(1), Some(TokenKind::Ident(s)) if s.eq_ignore_ascii_case(kw))
    }

    /// Parses the parenthesized part of `[NOT] IN (...)`: either a value list
    /// (desugared to an OR of / AND of comparisons, which keeps NULL
    /// three-valued semantics correct) or a subquery.
    fn finish_in(&mut self, lhs: Expr, negated: bool) -> Result<Expr> {
        self.expect_punct(Punct::LParen)?;
        if self.at_keyword("select") {
            self.eat_keyword("select");
            let sub = match self.parse_select()? {
                Stmt::Select(s) => *s,
                _ => unreachable!("parse_select only returns select"),
            };
            self.expect_punct(Punct::RParen)?;
            return Ok(Expr::InSubquery { expr: Box::new(lhs), sub: Box::new(sub), negated });
        }
        let mut items = vec![self.parse_additive()?];
        while self.eat_punct(Punct::Comma) {
            items.push(self.parse_additive()?);
        }
        self.expect_punct(Punct::RParen)?;
        let (fold, cmp) = if negated { (BinOp::And, BinOp::NotEq) } else { (BinOp::Or, BinOp::Eq) };
        let mut acc = Expr::Binary(cmp, Box::new(lhs.clone()), Box::new(items.remove(0)));
        for item in items {
            let eq = Expr::Binary(cmp, Box::new(lhs.clone()), Box::new(item));
            acc = Expr::Binary(fold, Box::new(acc), Box::new(eq));
        }
        Ok(acc)
    }

    fn parse_additive(&mut self) -> Result<Expr> {
        let mut lhs = self.parse_multiplicative()?;
        loop {
            let op = if self.eat_punct(Punct::Plus) {
                BinOp::Add
            } else if self.eat_punct(Punct::Minus) {
                BinOp::Sub
            } else {
                break;
            };
            lhs = Expr::Binary(op, Box::new(lhs), Box::new(self.parse_multiplicative()?));
        }
        Ok(lhs)
    }

    fn parse_multiplicative(&mut self) -> Result<Expr> {
        let mut lhs = self.parse_unary()?;
        loop {
            let op = if self.eat_punct(Punct::Star) {
                BinOp::Mul
            } else if self.eat_punct(Punct::Slash) {
                BinOp::Div
            } else if self.eat_punct(Punct::Percent) {
                BinOp::Mod
            } else {
                break;
            };
            lhs = Expr::Binary(op, Box::new(lhs), Box::new(self.parse_unary()?));
        }
        Ok(lhs)
    }

    fn parse_unary(&mut self) -> Result<Expr> {
        if self.eat_punct(Punct::Minus) {
            return Ok(Expr::Unary(UnOp::Neg, Box::new(self.parse_unary()?)));
        }
        if self.eat_punct(Punct::Plus) {
            return self.parse_unary();
        }
        self.parse_primary()
    }

    fn peek_agg_fn(&self) -> Option<AggFunc> {
        match self.peek().map(|t| &t.kind) {
            Some(TokenKind::Ident(s)) => Self::agg_func(s),
            _ => None,
        }
    }

    fn parse_primary(&mut self) -> Result<Expr> {
        // EXISTS (SELECT ...)
        if self.at_keyword("exists")
            && matches!(self.kind_at(1), Some(TokenKind::Punct(Punct::LParen)))
            && matches!(self.kind_at(2), Some(TokenKind::Ident(s)) if s.eq_ignore_ascii_case("select"))
        {
            self.pos += 2; // 'exists' and '('
            self.expect_keyword("select")?;
            let sub = match self.parse_select()? {
                Stmt::Select(s) => *s,
                _ => unreachable!("parse_select only returns select"),
            };
            self.expect_punct(Punct::RParen)?;
            return Ok(Expr::Exists { sub: Box::new(sub) });
        }
        // aggregate call: agg '(' [DISTINCT] (expr | '*') ')'
        if let Some(func) = self.peek_agg_fn()
            && matches!(self.kind_at(1), Some(TokenKind::Punct(Punct::LParen)))
        {
            self.pos += 2; // name and '('
            let distinct = self.eat_keyword("distinct");
            let star_at = self.here();
            let arg = if self.eat_punct(Punct::Star) {
                if func != AggFunc::Count {
                    return Err(Error::syntax("* is only valid in count(*)", star_at));
                }
                if distinct {
                    return Err(Error::syntax("count(distinct *) is not valid", star_at));
                }
                None
            } else {
                Some(Box::new(self.parse_expr()?))
            };
            self.expect_punct(Punct::RParen)?;
            return Ok(Expr::Aggregate(func, arg, distinct));
        }
        // scalar function call: name '(' args ')'
        if let Some(TokenKind::Ident(name)) = self.kind_at(0).cloned()
            && matches!(self.kind_at(1), Some(TokenKind::Punct(Punct::LParen)))
        {
            self.pos += 2; // name and '('
            let mut args = Vec::new();
            if !self.at_punct(Punct::RParen) {
                loop {
                    args.push(self.parse_expr()?);
                    if !self.eat_punct(Punct::Comma) {
                        break;
                    }
                }
            }
            self.expect_punct(Punct::RParen)?;
            return Ok(Expr::Function(name.to_ascii_lowercase(), args));
        }
        // qualified column: table '.' column
        if matches!(self.kind_at(0), Some(TokenKind::Ident(_)))
            && matches!(self.kind_at(1), Some(TokenKind::Punct(Punct::Dot)))
            && matches!(self.kind_at(2), Some(TokenKind::Ident(_)))
        {
            let table = self.parse_ident("table name")?;
            self.pos += 1; // '.'
            let name = self.parse_ident("column name")?;
            return Ok(Expr::QualifiedColumn(table, name));
        }
        match self.bump() {
            Some(Token { kind: TokenKind::Int(n), .. }) => Ok(Expr::Int(*n)),
            Some(Token { kind: TokenKind::Float(x), .. }) => Ok(Expr::Float(*x)),
            Some(Token { kind: TokenKind::Str(s), .. }) => Ok(Expr::Str(s.clone())),
            Some(Token { kind: TokenKind::Ident(s), .. }) if s.eq_ignore_ascii_case("null") => {
                Ok(Expr::Null)
            }
            Some(Token { kind: TokenKind::Ident(s), .. }) => Ok(Expr::Column(s.clone())),
            Some(Token { kind: TokenKind::Punct(Punct::LParen), .. }) => {
                // scalar subquery: (SELECT ...)
                if self.at_keyword("select") {
                    self.eat_keyword("select");
                    let sub = match self.parse_select()? {
                        Stmt::Select(s) => *s,
                        _ => unreachable!("parse_select only returns select"),
                    };
                    self.expect_punct(Punct::RParen)?;
                    return Ok(Expr::ScalarSubquery(Box::new(sub)));
                }
                let e = self.parse_expr()?;
                self.expect_punct(Punct::RParen)?;
                Ok(e)
            }
            _ => Err(self.unexpected("expression")),
        }
    }
}
