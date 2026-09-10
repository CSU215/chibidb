use crate::ast::{
    AggFunc, BinOp, ColumnDef, CreateDatabaseStmt, CreateIndexStmt, CreateTableStmt, CreateViewStmt,
    DataType, DeleteStmt, DropDatabaseStmt, DropIndexStmt, DropTableStmt, DropViewStmt, ExplainStmt,
    Expr, InsertStmt, JoinKind, Limit, SelectItem, SelectStmt, Stmt, TableRef, TrxCtl, UnOp,
    UpdateStmt, UseStmt,
};
use crate::lexer::{Punct, Token, TokenKind, lex};
use crate::{Error, Result};

pub fn parse(sql: &str) -> Result<Vec<Stmt>> {
    let tokens = lex(sql)?;
    let mut p = Parser { tokens, src: sql, pos: 0 };
    p.parse_statements()
}

struct Parser<'a> {
    tokens: Vec<Token>,
    src: &'a str,
    pos: usize,
}

impl<'a> Parser<'a> {
    fn peek(&self) -> Option<&Token> {
        self.tokens.get(self.pos)
    }

    fn bump(&mut self) -> Option<&Token> {
        let t = self.tokens.get(self.pos);
        if t.is_some() {
            self.pos += 1;
        }
        t
    }

    fn at_punct(&self, p: Punct) -> bool {
        matches!(self.peek().map(|t| &t.kind), Some(TokenKind::Punct(q)) if *q == p)
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
        if self.eat_punct(p) {
            Ok(())
        } else {
            Err(self.unexpected("punctuation"))
        }
    }

    fn at_keyword(&self, kw: &str) -> bool {
        matches!(self.peek().map(|t| &t.kind), Some(TokenKind::Ident(s)) if s.eq_ignore_ascii_case(kw))
    }

    fn eat_keyword(&mut self, kw: &str) -> bool {
        if self.at_keyword(kw) {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    fn unexpected(&self, want: &str) -> Error {
        let got = match self.peek() {
            None => "end of input".to_string(),
            Some(t) => format!("{:?}", t.kind),
        };
        Error::Syntax(format!("expected {want}, got {got}"))
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
        if self.eat_keyword("explain") {
            if !self.at_keyword("select") {
                return Err(self.unexpected("select after explain"));
            }
            let inner = self.parse_statement()?;
            return Ok(Stmt::Explain(ExplainStmt { stmt: Box::new(inner) }));
        }
        if self.eat_keyword("select") {
            return self.parse_select();
        }
        if self.eat_keyword("create") {
            if self.eat_keyword("database") {
                let name = self.parse_ident("database name")?;
                return Ok(Stmt::CreateDatabase(CreateDatabaseStmt { name }));
            }
            if self.eat_keyword("view") {
                let name = self.parse_ident("view name")?;
                if !self.eat_keyword("as") {
                    return Err(self.unexpected("as"));
                }
                // keep the original select text for the catalog
                let start = self
                    .tokens
                    .get(self.pos)
                    .map(|t| t.pos)
                    .ok_or_else(|| self.unexpected("select after as"))?;
                let stmt = self.parse_statement()?;
                let end = self
                    .tokens
                    .get(self.pos)
                    .map(|t| t.pos)
                    .unwrap_or(self.src.len());
                let sql = self.src[start..end].trim_end().to_string();
                match stmt {
                    Stmt::Select(_) => {
                        return Ok(Stmt::CreateView(CreateViewStmt { name, sql }))
                    }
                    _ => return Err(self.unexpected("select after as")),
                }
            }
            if self.eat_keyword("index") {
                let name = self.parse_ident("index name")?;
                if !self.eat_keyword("on") {
                    return Err(self.unexpected("on"));
                }
                let table = self.parse_ident("table name")?;
                self.expect_punct(Punct::LParen)?;
                let column = self.parse_ident("column name")?;
                self.expect_punct(Punct::RParen)?;
                return Ok(Stmt::CreateIndex(CreateIndexStmt { name, table, column }));
            }
            if !self.eat_keyword("table") {
                return Err(self.unexpected("table"));
            }
            return self.parse_create_table();
        }
        if self.eat_keyword("drop") {
            if self.eat_keyword("database") {
                let name = self.parse_ident("database name")?;
                return Ok(Stmt::DropDatabase(DropDatabaseStmt { name }));
            }
            if self.eat_keyword("index") {
                let name = self.parse_ident("index name")?;
                return Ok(Stmt::DropIndex(DropIndexStmt { name }));
            }
            if self.eat_keyword("view") {
                let name = self.parse_ident("view name")?;
                return Ok(Stmt::DropView(DropViewStmt { name }));
            }
            if !self.eat_keyword("table") {
                return Err(self.unexpected("index, view or table"));
            }
            let name = self.parse_ident("table name")?;
            return Ok(Stmt::DropTable(DropTableStmt { name }));
        }
        if self.eat_keyword("use") {
            let name = self.parse_ident("database name")?;
            return Ok(Stmt::Use(UseStmt { name }));
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
            if !self.eat_keyword("set") {
                return Err(self.unexpected("set"));
            }
            let mut assignments = Vec::new();
            loop {
                let col = self.parse_ident("column name")?;
                self.expect_punct(Punct::Eq)?;
                let expr = self.parse_expr()?;
                assignments.push((col, expr));
                if !self.eat_punct(Punct::Comma) {
                    break;
                }
            }
            let selection = self.parse_where()?;
            return Ok(Stmt::Update(UpdateStmt { table, assignments, selection }));
        }
        Err(self.unexpected("statement"))
    }

    fn parse_where(&mut self) -> Result<Option<Expr>> {
        if self.eat_keyword("where") {
            Ok(Some(self.parse_expr()?))
        } else {
            Ok(None)
        }
    }

    const RESERVED: &'static [&'static str] = &[
        "where", "group", "having", "order", "limit", "on", "join", "inner", "left", "right",
        "outer", "in", "exists", "distinct", "vacuum", "like", "union",
    ];

    fn agg_func(name: &str) -> Option<AggFunc> {
        if name.eq_ignore_ascii_case("count") {
            Some(AggFunc::Count)
        } else if name.eq_ignore_ascii_case("sum") {
            Some(AggFunc::Sum)
        } else if name.eq_ignore_ascii_case("avg") {
            Some(AggFunc::Avg)
        } else if name.eq_ignore_ascii_case("min") {
            Some(AggFunc::Min)
        } else if name.eq_ignore_ascii_case("max") {
            Some(AggFunc::Max)
        } else {
            None
        }
    }

    fn at_reserved(&self) -> bool {
        matches!(self.peek().map(|t| &t.kind), Some(TokenKind::Ident(s)) if Self::RESERVED.contains(&s.to_ascii_lowercase().as_str()))
    }

    fn parse_ident(&mut self, want: &str) -> Result<String> {
        match self.bump() {
            Some(Token { kind: TokenKind::Ident(s), .. }) => Ok(s.clone()),
            _ => Err(self.unexpected(want)),
        }
    }

    fn parse_select(&mut self) -> Result<Stmt> {
        let mut first = self.parse_select_core()?;
        while self.eat_keyword("union") {
            let all = self.eat_keyword("all");
            if !self.eat_keyword("select") {
                return Err(self.unexpected("select after union"));
            }
            let next = self.parse_select_core()?;
            first.set_ops.push((all, Box::new(next)));
        }
        // trailing ORDER BY / LIMIT belong to the whole set operation
        if self.eat_keyword("order") {
            if !self.eat_keyword("by") {
                return Err(self.unexpected("by"));
            }
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
            let count = self.parse_expr()?;
            if !matches!(count, Expr::Int(_)) {
                return Err(Error::Syntax("limit count must be a non-negative integer".into()));
            }
            let offset = if self.eat_keyword("offset") {
                let off = self.parse_expr()?;
                if !matches!(off, Expr::Int(_)) {
                    return Err(Error::Syntax(
                        "limit offset must be a non-negative integer".into(),
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
                    let alias = self.parse_ident("alias")?;
                    items.push(SelectItem::Aliased(expr, alias));
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
                    if !self.eat_keyword("join") {
                        return Err(self.unexpected("join"));
                    }
                    JoinKind::Left
                } else if self.eat_keyword("right") {
                    self.eat_keyword("outer");
                    if !self.eat_keyword("join") {
                        return Err(self.unexpected("join"));
                    }
                    JoinKind::Right
                } else if self.eat_keyword("join") {
                    JoinKind::Inner
                } else {
                    break;
                };
                from.push(self.parse_table_ref()?);
                joins.push(kind);
                if !self.eat_keyword("on") {
                    return Err(self.unexpected("on"));
                }
                on.push(self.parse_expr()?);
                continue;
            }
        }
        let selection = if self.eat_keyword("where") {
            Some(self.parse_expr()?)
        } else {
            None
        };
        let group_by = if self.eat_keyword("group") {
            if !self.eat_keyword("by") {
                return Err(self.unexpected("by"));
            }
            let mut group_by = vec![self.parse_expr()?];
            while self.eat_punct(Punct::Comma) {
                group_by.push(self.parse_expr()?);
            }
            group_by
        } else {
            vec![]
        };
        let having = if self.eat_keyword("having") {
            Some(self.parse_expr()?)
        } else {
            None
        };
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
        } else if self.at_reserved() {
            None
        } else if matches!(self.peek().map(|t| &t.kind), Some(TokenKind::Ident(_))) {
            Some(self.parse_ident("alias")?)
        } else {
            None
        };
        Ok(TableRef { name, alias })
    }

    fn parse_insert(&mut self) -> Result<Stmt> {
        let table = match self.bump() {
            Some(Token { kind: TokenKind::Ident(s), .. }) => s.clone(),
            _ => return Err(self.unexpected("table name")),
        };
        let mut columns = None;
        if self.eat_punct(Punct::LParen) {
            let mut cols = vec![self.parse_ident("column name")?];
            while self.eat_punct(Punct::Comma) {
                cols.push(self.parse_ident("column name")?);
            }
            self.expect_punct(Punct::RParen)?;
            columns = Some(cols);
        }
        if !self.eat_keyword("values") {
            return Err(self.unexpected("values"));
        }
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

    fn parse_value(&mut self) -> Result<Expr> {
        let neg = self.eat_punct(Punct::Minus);
        if !neg && self.at_keyword("null") {
            self.pos += 1;
            return Ok(Expr::Null);
        }
        let v = match self.bump() {
            Some(Token { kind: TokenKind::Int(n), .. }) => Expr::Int(if neg { -*n } else { *n }),
            Some(Token { kind: TokenKind::Float(x), .. }) => {
                Expr::Float(if neg { -*x } else { *x })
            }
            Some(Token { kind: TokenKind::Str(s), .. }) if !neg => Expr::Str(s.clone()),
            _ => return Err(self.unexpected("value")),
        };
        Ok(v)
    }

    fn parse_create_table(&mut self) -> Result<Stmt> {
        let name = match self.bump() {
            Some(Token { kind: TokenKind::Ident(s), .. }) => s.clone(),
            _ => return Err(self.unexpected("table name")),
        };
        self.expect_punct(Punct::LParen)?;
        let mut columns = Vec::new();
        loop {
            columns.push(self.parse_column_def()?);
            if !self.eat_punct(Punct::Comma) {
                break;
            }
        }
        self.expect_punct(Punct::RParen)?;
        Ok(Stmt::CreateTable(CreateTableStmt { name, columns }))
    }

    fn parse_column_def(&mut self) -> Result<ColumnDef> {
        let name = match self.bump() {
            Some(Token { kind: TokenKind::Ident(s), .. }) => s.clone(),
            _ => return Err(self.unexpected("column name")),
        };
        let dtype = self.parse_data_type()?;
        let mut not_null = false;
        let mut primary_key = false;
        let mut unique = false;
        let mut default = None;
        loop {
            if self.eat_keyword("primary") {
                if !self.eat_keyword("key") {
                    return Err(self.unexpected("key"));
                }
                primary_key = true;
                not_null = true;
                unique = true;
            } else if self.eat_keyword("unique") {
                unique = true;
            } else if self.eat_keyword("not") {
                if !self.eat_keyword("null") {
                    return Err(self.unexpected("null"));
                }
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
            let n = match self.bump() {
                Some(Token { kind: TokenKind::Int(n), .. }) => *n,
                _ => return Err(self.unexpected("char length")),
            };
            if n <= 0 {
                return Err(Error::Syntax("char length must be positive".into()));
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
            let e = self.parse_not()?;
            return Ok(Expr::Unary(UnOp::Not, Box::new(e)));
        }
        self.parse_comparison()
    }

    fn parse_comparison(&mut self) -> Result<Expr> {
        let lhs = self.parse_additive()?;
        if self.eat_keyword("is") {
            let negated = self.eat_keyword("not");
            if !self.eat_keyword("null") {
                return Err(self.unexpected("null"));
            }
            return Ok(Expr::IsNull(Box::new(lhs), negated));
        }
        // [NOT] IN (value list | subquery)
        let next_is_in = matches!(self.tokens.get(self.pos + 1).map(|t| &t.kind),
            Some(TokenKind::Ident(s)) if s.eq_ignore_ascii_case("in"));
        if self.at_keyword("in") || (self.at_keyword("not") && next_is_in) {
            let negated = self.eat_keyword("not");
            if !self.eat_keyword("in") {
                return Err(self.unexpected("in"));
            }
            return self.finish_in(lhs, negated);
        }
        // [NOT] LIKE 'pattern'
        let next_is_like = matches!(self.tokens.get(self.pos + 1).map(|t| &t.kind),
            Some(TokenKind::Ident(s)) if s.eq_ignore_ascii_case("like"));
        if self.at_keyword("like") || (self.at_keyword("not") && next_is_like) {
            let negated = self.eat_keyword("not");
            if !self.eat_keyword("like") {
                return Err(self.unexpected("like"));
            }
            let pattern = self.parse_additive()?;
            let escape = if self.eat_keyword("escape") {
                match self.bump() {
                    Some(Token { kind: TokenKind::Str(s), .. }) if s.chars().count() == 1 => {
                        s.chars().next()
                    }
                    _ => {
                        return Err(Error::Syntax(
                            "ESCAPE requires a single-character string".into(),
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
        let op = if self.at_punct(Punct::Eq) {
            BinOp::Eq
        } else if self.at_punct(Punct::NotEq) {
            BinOp::NotEq
        } else if self.at_punct(Punct::Lt) {
            BinOp::Lt
        } else if self.at_punct(Punct::Le) {
            BinOp::Le
        } else if self.at_punct(Punct::Gt) {
            BinOp::Gt
        } else if self.at_punct(Punct::Ge) {
            BinOp::Ge
        } else {
            return Ok(lhs);
        };
        self.pos += 1;
        let rhs = self.parse_additive()?;
        Ok(Expr::Binary(op, Box::new(lhs), Box::new(rhs)))
    }

    /// Parses the parenthesized part of `[NOT] IN (...)`: either a value
    /// list (desugared to an OR of / AND of comparisons, which keeps NULL
    /// three-valued semantics correct) or a subquery.
    fn finish_in(&mut self, lhs: Expr, negated: bool) -> Result<Expr> {
        self.expect_punct(Punct::LParen)?;
        if self.at_keyword("select") {
            if !self.eat_keyword("select") {
                return Err(self.unexpected("select"));
            }
            let sub = match self.parse_select()? {
                Stmt::Select(s) => *s,
                _ => unreachable!("parse_select only returns select"),
            };
            self.expect_punct(Punct::RParen)?;
            return Ok(Expr::InSubquery {
                expr: Box::new(lhs),
                sub: Box::new(sub),
                negated,
            });
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
            let op = if self.at_punct(Punct::Plus) {
                BinOp::Add
            } else if self.at_punct(Punct::Minus) {
                BinOp::Sub
            } else {
                break;
            };
            self.pos += 1;
            let rhs = self.parse_multiplicative()?;
            lhs = Expr::Binary(op, Box::new(lhs), Box::new(rhs));
        }
        Ok(lhs)
    }

    fn parse_multiplicative(&mut self) -> Result<Expr> {
        let mut lhs = self.parse_unary()?;
        loop {
            let op = if self.at_punct(Punct::Star) {
                BinOp::Mul
            } else if self.at_punct(Punct::Slash) {
                BinOp::Div
            } else if self.at_punct(Punct::Percent) {
                BinOp::Mod
            } else {
                break;
            };
            self.pos += 1;
            let rhs = self.parse_unary()?;
            lhs = Expr::Binary(op, Box::new(lhs), Box::new(rhs));
        }
        Ok(lhs)
    }

    fn parse_unary(&mut self) -> Result<Expr> {
        if self.eat_punct(Punct::Minus) {
            let e = self.parse_unary()?;
            return Ok(Expr::Unary(UnOp::Neg, Box::new(e)));
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
            && matches!(self.tokens.get(self.pos + 1).map(|t| &t.kind),
                        Some(TokenKind::Punct(Punct::LParen)))
            && matches!(self.tokens.get(self.pos + 2).map(|t| &t.kind),
                        Some(TokenKind::Ident(s)) if s.eq_ignore_ascii_case("select"))
        {
            self.pos += 2; // 'exists' and '('
            if !self.eat_keyword("select") {
                return Err(self.unexpected("select"));
            }
            let sub = match self.parse_select()? {
                Stmt::Select(s) => *s,
                _ => unreachable!("parse_select only returns select"),
            };
            self.expect_punct(Punct::RParen)?;
            return Ok(Expr::Exists { sub: Box::new(sub) });
        }
        if let Some(func) = self.peek_agg_fn()
            && matches!(self.tokens.get(self.pos + 1).map(|t| &t.kind),
                        Some(TokenKind::Punct(Punct::LParen)))
            {
                self.pos += 2; // consume name and '('
                let distinct = self.eat_keyword("distinct");
                let arg = if self.eat_punct(Punct::Star) {
                    if func != AggFunc::Count {
                        return Err(Error::Syntax("* is only valid in count(*)".into()));
                    }
                    if distinct {
                        return Err(Error::Syntax("count(distinct *) is not valid".into()));
                    }
                    None
                } else {
                    Some(Box::new(self.parse_expr()?))
                };
                self.expect_punct(Punct::RParen)?;
                return Ok(Expr::Aggregate(func, arg, distinct));
            }
        // scalar function call: ident '(' args ')'
        if let Some(TokenKind::Ident(name)) = self.peek().map(|t| &t.kind).cloned()
            && matches!(self.tokens.get(self.pos + 1).map(|t| &t.kind),
                        Some(TokenKind::Punct(Punct::LParen)))
        {
            self.pos += 1; // function name
            self.expect_punct(Punct::LParen)?;
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
        // qualified column: ident '.' ident
        if matches!(self.peek().map(|t| &t.kind), Some(TokenKind::Ident(_)))
            && matches!(self.tokens.get(self.pos + 1).map(|t| &t.kind), Some(TokenKind::Punct(Punct::Dot)))
            && matches!(self.tokens.get(self.pos + 2).map(|t| &t.kind), Some(TokenKind::Ident(_)))
        {
            let table = match self.bump() {
                Some(Token { kind: TokenKind::Ident(s), .. }) => s.clone(),
                _ => unreachable!(),
            };
            self.pos += 1; // '.'
            let name = match self.bump() {
                Some(Token { kind: TokenKind::Ident(s), .. }) => s.clone(),
                _ => unreachable!(),
            };
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
                    if !self.eat_keyword("select") {
                        return Err(self.unexpected("select"));
                    }
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
