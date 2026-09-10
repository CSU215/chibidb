use crate::ast::{
    AggFunc, BinOp, ColumnDef, CreateIndexStmt, CreateTableStmt, DataType, DeleteStmt,
    DropIndexStmt, DropTableStmt, ExplainStmt, Expr, InsertStmt, Limit, SelectItem, SelectStmt,
    Stmt, TableRef, TrxCtl, UnOp, UpdateStmt,
};
use crate::lexer::{Punct, Token, TokenKind, lex};
use crate::{Error, Result};

pub fn parse(sql: &str) -> Result<Vec<Stmt>> {
    let tokens = lex(sql)?;
    let mut p = Parser { tokens, pos: 0 };
    p.parse_statements()
}

struct Parser {
    tokens: Vec<Token>,
    pos: usize,
}

impl Parser {
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
            if self.eat_keyword("index") {
                let name = self.parse_ident("index name")?;
                return Ok(Stmt::DropIndex(DropIndexStmt { name }));
            }
            if !self.eat_keyword("table") {
                return Err(self.unexpected("index or table"));
            }
            let name = self.parse_ident("table name")?;
            return Ok(Stmt::DropTable(DropTableStmt { name }));
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

    const RESERVED: &[&str] =
        &["where", "group", "having", "order", "limit", "on", "join", "inner", "left", "right"];

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
        if self.eat_keyword("from") {
            from.push(self.parse_table_ref()?);
            loop {
                if self.eat_punct(Punct::Comma) {
                    from.push(self.parse_table_ref()?);
                    continue;
                }
                if self.eat_keyword("inner") && !self.at_keyword("join") {
                    return Err(self.unexpected("join"));
                }
                if self.eat_keyword("join") {
                    from.push(self.parse_table_ref()?);
                    if !self.eat_keyword("on") {
                        return Err(self.unexpected("on"));
                    }
                    on.push(self.parse_expr()?);
                    continue;
                }
                break;
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
        let mut order_by = Vec::new();
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
                order_by.push((expr, desc));
                if !self.eat_punct(Punct::Comma) {
                    break;
                }
            }
        }
        let limit = if self.eat_keyword("limit") {
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
            Some(Limit { count, offset })
        } else {
            None
        };
        Ok(Stmt::Select(Box::new(SelectStmt {
            items,
            from,
            on,
            selection,
            group_by,
            having,
            order_by,
            limit,
        })))
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
        Ok(Stmt::Insert(InsertStmt { table, rows }))
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
        Ok(ColumnDef { name, dtype })
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
        if let Some(func) = self.peek_agg_fn()
            && matches!(self.tokens.get(self.pos + 1).map(|t| &t.kind),
                        Some(TokenKind::Punct(Punct::LParen)))
            {
                self.pos += 2; // consume name and '('
                let arg = if self.eat_punct(Punct::Star) {
                    if func != AggFunc::Count {
                        return Err(Error::Syntax("* is only valid in count(*)".into()));
                    }
                    None
                } else {
                    Some(Box::new(self.parse_expr()?))
                };
                self.expect_punct(Punct::RParen)?;
                return Ok(Expr::Aggregate(func, arg));
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
                let e = self.parse_expr()?;
                self.expect_punct(Punct::RParen)?;
                Ok(e)
            }
            _ => Err(self.unexpected("expression")),
        }
    }
}
