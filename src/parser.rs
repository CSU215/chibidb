use crate::ast::{BinOp, ColumnDef, CreateTableStmt, DataType, Expr, SelectStmt, Stmt, UnOp};
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
        if self.eat_keyword("select") {
            let mut exprs = vec![self.parse_expr()?];
            while self.eat_punct(Punct::Comma) {
                exprs.push(self.parse_expr()?);
            }
            return Ok(Stmt::Select(SelectStmt { exprs }));
        }
        if self.eat_keyword("create") {
            if !self.eat_keyword("table") {
                return Err(self.unexpected("table"));
            }
            return self.parse_create_table();
        }
        Err(self.unexpected("statement"))
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

    fn parse_primary(&mut self) -> Result<Expr> {
        match self.bump() {
            Some(Token { kind: TokenKind::Int(n), .. }) => Ok(Expr::Int(*n)),
            Some(Token { kind: TokenKind::Float(x), .. }) => Ok(Expr::Float(*x)),
            Some(Token { kind: TokenKind::Str(s), .. }) => Ok(Expr::Str(s.clone())),
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
