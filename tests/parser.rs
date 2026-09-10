use chibidb::ast::{DataType, JoinKind, SelectItem, Stmt};
use chibidb::parser::parse;

fn single_exprs(sql: &str) -> Vec<String> {
    let stmts = parse(sql).unwrap();
    assert_eq!(stmts.len(), 1, "sql: {sql}");
    match stmts.into_iter().next().unwrap() {
        Stmt::Select(s) => s
            .items
            .iter()
            .map(|it| match it {
                SelectItem::Star => "*".to_string(),
                SelectItem::Expr(e) => e.to_string(),
                SelectItem::Aliased(e, _) => e.to_string(),
            })
            .collect(),
        other => panic!("expected select, got {other:?}"),
    }
}

fn err(sql: &str) {
    assert!(parse(sql).is_err(), "expected error for: {sql}");
}

#[test]
fn parses_select_literal() {
    assert_eq!(single_exprs("select 1;"), ["1"]);
}

#[test]
fn parses_comma_separated_exprs() {
    assert_eq!(single_exprs("select 1, 2.5, 'ab';"), ["1", "2.5", "'ab'"]);
}

#[test]
fn select_keyword_is_case_insensitive() {
    assert_eq!(single_exprs("SELECT 1;"), ["1"]);
}

#[test]
fn mul_binds_tighter_than_add() {
    assert_eq!(single_exprs("select 1+2*3;"), ["(+ 1 (* 2 3))"]);
}

#[test]
fn subtraction_is_left_associative() {
    assert_eq!(single_exprs("select 1-2-3;"), ["(- (- 1 2) 3)"]);
}

#[test]
fn parens_override_precedence() {
    assert_eq!(single_exprs("select (1+2)*3;"), ["(* (+ 1 2) 3)"]);
}

#[test]
fn unary_minus() {
    assert_eq!(single_exprs("select -1, -(1+2);"), ["(- 1)", "(- (+ 1 2))"]);
}

#[test]
fn unary_plus_is_identity() {
    assert_eq!(single_exprs("select +1;"), ["1"]);
}

#[test]
fn parses_column_ref() {
    assert_eq!(single_exprs("select ab_1;"), ["ab_1"]);
}

#[test]
fn parses_multiple_statements() {
    let stmts = parse("select 1; select 2;").unwrap();
    assert_eq!(stmts.len(), 2);
}

#[test]
fn trailing_semicolon_optional_and_empty_statements_skipped() {
    assert_eq!(single_exprs("select 1"), ["1"]);
    let stmts = parse(";;").unwrap();
    assert_eq!(stmts.len(), 0);
}

#[test]
fn syntax_errors() {
    err("select;");
    err("select 1 +;");
    err("select 1 2;");
    err("foo;");
    err("select )");
    err("select (1");
}

#[test]
fn parses_comparisons() {
    assert_eq!(single_exprs("select 1 = 2;"), ["(= 1 2)"]);
    assert_eq!(single_exprs("select 1 <> 2;"), ["(<> 1 2)"]);
    assert_eq!(single_exprs("select 1 != 2;"), ["(<> 1 2)"], "!= normalizes to <>");
    assert_eq!(single_exprs("select 1 < 2, 2 <= 2, 3 > 2, 3 >= 3;"),
        ["(< 1 2)", "(<= 2 2)", "(> 3 2)", "(>= 3 3)"]);
}

#[test]
fn comparison_is_looser_than_additive() {
    assert_eq!(single_exprs("select 1+1 = 2;"), ["(= (+ 1 1) 2)"]);
    assert_eq!(single_exprs("select 1 < 2+3;"), ["(< 1 (+ 2 3))"]);
}

#[test]
fn comparisons_are_not_chainable() {
    err("select 1 < 2 < 3;");
}

#[test]
fn parses_and_or_not() {
    assert_eq!(single_exprs("select 1 = 1 and 2 = 2;"), ["(and (= 1 1) (= 2 2))"]);
    assert_eq!(single_exprs("select 1 or 0;"), ["(or 1 0)"]);
    assert_eq!(single_exprs("select not 1;"), ["(not 1)"]);
    assert_eq!(single_exprs("select not not 1;"), ["(not (not 1))"]);
    assert_eq!(single_exprs("select 1 AND 2;"), ["(and 1 2)"]);
}

#[test]
fn logical_precedence() {
    // and binds tighter than or
    assert_eq!(single_exprs("select 1 or 0 and 0;"), ["(or 1 (and 0 0))"]);
    // not binds looser than comparison
    assert_eq!(single_exprs("select not 1 = 1;"), ["(not (= 1 1))"]);
    // left associativity
    assert_eq!(single_exprs("select 1 and 0 and 1;"), ["(and (and 1 0) 1)"]);
}

fn create_table(sql: &str) -> chibidb::ast::CreateTableStmt {
    let stmts = parse(sql).unwrap();
    assert_eq!(stmts.len(), 1, "sql: {sql}");
    match stmts.into_iter().next().unwrap() {
        Stmt::CreateTable(c) => c,
        _ => panic!("expected create table for: {sql}"),
    }
}

#[test]
fn parses_create_table() {
    let c = create_table("create table student (id int, name char(10), score float);");
    assert_eq!(c.name, "student");
    assert_eq!(c.columns.len(), 3);
    assert_eq!(c.columns[0].name, "id");
    assert_eq!(c.columns[0].dtype, DataType::Int);
    assert_eq!(c.columns[1].name, "name");
    assert_eq!(c.columns[1].dtype, DataType::Char(10));
    assert_eq!(c.columns[2].name, "score");
    assert_eq!(c.columns[2].dtype, DataType::Float);
}

#[test]
fn parses_column_constraints() {
    let c = create_table(
        "create table t (\
         id int primary key, \
         name char(10) not null, \
         email char(20) unique, \
         age int default 0, \
         city char(8) default 'unk');",
    );
    assert!(c.columns[0].primary_key);
    assert!(c.columns[0].not_null, "primary key implies not null");
    assert!(c.columns[0].unique, "primary key implies unique");
    assert!(!c.columns[1].primary_key);
    assert!(c.columns[1].not_null);
    assert!(c.columns[2].unique);
    assert!(!c.columns[2].not_null);
    assert_eq!(c.columns[3].default.as_ref().unwrap().to_string(), "0");
    assert_eq!(c.columns[4].default.as_ref().unwrap().to_string(), "'unk'");
}

#[test]
fn parses_insert_column_list() {
    let i = insert("insert into t (b, a) values (1, 2);");
    assert_eq!(i.columns.unwrap(), vec!["b".to_string(), "a".to_string()]);
    let i = insert("insert into t values (1, 2);");
    assert!(i.columns.is_none());
    err("insert into t (a,) values (1);");
    err("insert into t (a b) values (1);");
}

#[test]
fn parses_date_columns() {
    let c = create_table("create table t (d date);");
    assert_eq!(c.columns[0].dtype, DataType::Date);
}

#[test]
fn parses_text_columns() {
    let c = create_table("create table t (body text);");
    assert_eq!(c.columns[0].dtype, DataType::Text);
}

#[test]
fn create_table_is_case_insensitive() {
    let c = create_table("CREATE TABLE T (ID INT);");
    assert_eq!(c.name, "T");
    assert_eq!(c.columns[0].dtype, DataType::Int);
}

#[test]
fn create_table_syntax_errors() {
    err("create t (id int);");
    err("create table (id int);");
    err("create table t ();");
    err("create table t (id int,);");
    err("create table t (id int");
    err("create table t (id);");
    err("create table t (s char);");
    err("create table t (s char(0));");
    err("create table t (a bool);");
    err("create table t (id int,);");
}

fn insert(sql: &str) -> chibidb::ast::InsertStmt {
    let stmts = parse(sql).unwrap();
    assert_eq!(stmts.len(), 1, "sql: {sql}");
    match stmts.into_iter().next().unwrap() {
        Stmt::Insert(i) => i,
        other => panic!("expected insert, got {other:?}"),
    }
}

#[test]
fn parses_insert() {
    let i = insert("insert into student values (1, 'alice', 95.5);");
    assert_eq!(i.table, "student");
    assert_eq!(i.rows.len(), 1);
    let row: Vec<String> = i.rows[0].iter().map(|e| e.to_string()).collect();
    assert_eq!(row, ["1", "'alice'", "95.5"]);
}

#[test]
fn parses_multi_row_insert() {
    let i = insert("insert into t values (1), (2, 3.5);");
    assert_eq!(i.table, "t");
    assert_eq!(i.rows.len(), 2);
    assert_eq!(i.rows[0][0].to_string(), "1");
    assert_eq!(i.rows[1][0].to_string(), "2");
    assert_eq!(i.rows[1][1].to_string(), "3.5");
}

#[test]
fn parses_negative_literals() {
    let i = insert("insert into t values (-1, -2.5);");
    assert_eq!(i.rows[0][0].to_string(), "-1");
    assert_eq!(i.rows[0][1].to_string(), "-2.5");
}

#[test]
fn insert_is_case_insensitive() {
    let i = insert("INSERT INTO T VALUES (1);");
    assert_eq!(i.table, "T");
}

#[test]
fn insert_syntax_errors() {
    err("insert t values (1);");
    err("insert into values (1);");
    err("insert into t (1);");
    err("insert into t values ();");
    err("insert into t values (1,);");
    err("insert into t values (1); extra");
    err("insert into t values (1, 2))");
}

fn select(sql: &str) -> chibidb::ast::SelectStmt {
    let stmts = parse(sql).unwrap();
    assert_eq!(stmts.len(), 1, "sql: {sql}");
    match stmts.into_iter().next().unwrap() {
        Stmt::Select(s) => *s,
        other => panic!("expected select, got {other:?}"),
    }
}

#[test]
fn parses_select_from_where() {
    let s = select("select id, name from student where id = 1;");
    assert_eq!(s.items.len(), 2);
    assert!(matches!(&s.items[0], SelectItem::Expr(e) if e.to_string() == "id"));
    assert!(matches!(&s.items[1], SelectItem::Expr(e) if e.to_string() == "name"));
    let from = &s.from[0];
    assert_eq!(from.name, "student");
    assert_eq!(from.alias, None);
    assert_eq!(s.selection.as_ref().unwrap().to_string(), "(= id 1)");
}

#[test]
fn parses_star() {
    let s = select("select * from t;");
    assert_eq!(s.items, [SelectItem::Star]);
    assert_eq!(s.from[0].name, "t");
    let s = select("select *, id from t;");
    assert_eq!(s.items.len(), 2);
}

#[test]
fn parses_table_alias() {
    let s = select("select 1 from t as a;");
    assert_eq!(s.from[0].alias.as_deref(), Some("a"));
    let s = select("select 1 from t a;");
    assert_eq!(s.from[0].alias.as_deref(), Some("a"));
}

#[test]
fn select_without_from_has_no_selection() {
    let s = select("select 1;");
    assert!(s.from.is_empty());
    assert_eq!(s.selection, None);
    let s = select("select 1 from t where 1 and 2;");
    assert_eq!(s.selection.as_ref().unwrap().to_string(), "(and 1 2)");
}

#[test]
fn keywords_in_select_are_case_insensitive() {
    let s = select("SELECT * FROM t WHERE 1;");
    assert_eq!(s.from[0].name, "t");
    assert!(s.selection.is_some());
}

#[test]
fn select_from_syntax_errors() {
    err("select * from;");
    err("select 1 from;");
    err("select * from t where;");
    err("select * from t where 1 =;");
}

#[test]
fn parses_delete() {
    let stmts = parse("delete from t;").unwrap();
    match &stmts[0] {
        Stmt::Delete(d) => {
            assert_eq!(d.table, "t");
            assert_eq!(d.selection, None);
        }
        other => panic!("expected delete, got {other:?}"),
    }
    let stmts = parse("delete from t where id = 1 or name = 'x';").unwrap();
    match &stmts[0] {
        Stmt::Delete(d) => {
            assert_eq!(d.table, "t");
            assert_eq!(d.selection.as_ref().unwrap().to_string(), "(or (= id 1) (= name 'x'))");
        }
        other => panic!("expected delete, got {other:?}"),
    }
}

#[test]
fn parses_update() {
    let stmts = parse("update t set score = 100;").unwrap();
    match &stmts[0] {
        Stmt::Update(u) => {
            assert_eq!(u.table, "t");
            assert_eq!(u.assignments.len(), 1);
            assert_eq!(u.assignments[0].0, "score");
            assert_eq!(u.assignments[0].1.to_string(), "100");
            assert_eq!(u.selection, None);
        }
        other => panic!("expected update, got {other:?}"),
    }
    let stmts = parse("update t set a = a + 1, b = 'x' where id = 3;").unwrap();
    match &stmts[0] {
        Stmt::Update(u) => {
            assert_eq!(u.assignments.len(), 2);
            assert_eq!(u.assignments[0].1.to_string(), "(+ a 1)");
            assert_eq!(u.assignments[1].1.to_string(), "'x'");
            assert_eq!(u.selection.as_ref().unwrap().to_string(), "(= id 3)");
        }
        other => panic!("expected update, got {other:?}"),
    }
}

#[test]
fn delete_update_syntax_errors() {
    err("delete t;");
    err("delete from;");
    err("delete from t where;");
    err("update t;");
    err("update t set;");
    err("update t set a;");
    err("update t set a =;");
    err("update t set a = 1,;");
}

#[test]
fn parses_is_null() {
    assert_eq!(single_exprs("select a is null;"), ["(is-null a)"]);
    assert_eq!(single_exprs("select a is not null;"), ["(is-not-null a)"]);
    assert_eq!(single_exprs("select 1+1 is null;"), ["(is-null (+ 1 1))"]);
    err("select a is;");
    err("select a is not;");
    err("select a is maybe;");
}

#[test]
fn parses_left_join() {
    let s = select("select * from a left join b on a.id = b.a_id;");
    assert_eq!(s.from.len(), 2);
    assert_eq!(s.joins, vec![JoinKind::Cross, JoinKind::Left]);
    assert_eq!(s.on.len(), 1);

    // optional OUTER keyword
    let s = select("select * from a left outer join b on a.id = b.a_id;");
    assert_eq!(s.joins, vec![JoinKind::Cross, JoinKind::Left]);

    let s = select("select * from a join b on a.id = b.a_id;");
    assert_eq!(s.joins, vec![JoinKind::Cross, JoinKind::Inner]);

    let s = select("select * from a right join b on a.id = b.a_id;");
    assert_eq!(s.joins, vec![JoinKind::Cross, JoinKind::Right]);

    let s = select("select * from a right outer join b on a.id = b.a_id;");
    assert_eq!(s.joins, vec![JoinKind::Cross, JoinKind::Right]);

    let s = select("select * from a, b;");
    assert_eq!(s.joins, vec![JoinKind::Cross, JoinKind::Cross]);

    err("select * from a outer join b on a.id = b.a_id;");
    err("select * from a left b on a.id = b.a_id;");
    err("select * from a left join b;");
}

#[test]
fn parses_distinct() {
    let s = select("select distinct id from t;");
    assert!(s.distinct, "distinct flag must be set");
    let s = select("select id from t;");
    assert!(!s.distinct);
    let s = select("select distinct * from t;");
    assert!(s.distinct);
}

#[test]
fn parses_in_list() {
    let s = select("select * from t where id in (1, 2, 3);");
    let sel = s.selection.as_ref().unwrap();
    assert_eq!(
        sel.to_string(),
        "(or (or (= id 1) (= id 2)) (= id 3))",
        "in-list desugars to an OR of equalities"
    );

    let s = select("select * from t where id not in (1, 2);");
    let sel = s.selection.as_ref().unwrap();
    assert_eq!(
        sel.to_string(),
        "(and (<> id 1) (<> id 2))",
        "not-in desugars to an AND of inequalities"
    );

    err("select * from t where id in ();");
}

#[test]
fn parses_like_and_mod() {
    assert_eq!(single_exprs("select name like 'a%';"), ["(like name 'a%')"]);
    assert_eq!(single_exprs("select name not like 'a_%';"), ["(not-like name 'a_%')"]);
    assert_eq!(single_exprs("select 5 % 2;"), ["(% 5 2)"]);
    assert_eq!(single_exprs("select 1 + 2 % 3;"), ["(+ 1 (% 2 3))"]);
    assert_eq!(single_exprs("select 1 * 2 % 3;"), ["(% (* 1 2) 3)"]);
    err("select name like;");
    err("select name not 'x';");
}

#[test]
fn parses_scalar_functions() {
    assert_eq!(single_exprs("select concat('a', 'b');"), ["(concat 'a' 'b')"]);
    assert_eq!(single_exprs("select upper(name);"), ["(upper name)"]);
    assert_eq!(single_exprs("select length(name) + 1;"), ["(+ (length name) 1)"]);
    assert_eq!(single_exprs("select substring(a, 1, 2);"), ["(substring a 1 2)"]);
    assert_eq!(single_exprs("select CONCAT(a, b);"), ["(concat a b)"], "name lowercased");
    assert_eq!(single_exprs("select length(name) from t where upper(name) = 'A';"),
        ["(length name)"]);
    err("select upper(;");
}

#[test]
fn parses_create_and_drop_view() {
    let stmts = parse("create view v as select id, score from student where score > 60;").unwrap();
    match &stmts[0] {
        Stmt::CreateView(c) => {
            assert_eq!(c.name, "v");
            assert_eq!(c.sql, "select id, score from student where score > 60");
        }
        other => panic!("expected create view, got {other:?}"),
    }

    let stmts = parse("create view w as select 1;").unwrap();
    match &stmts[0] {
        Stmt::CreateView(c) => assert_eq!(c.sql, "select 1"),
        other => panic!("expected create view, got {other:?}"),
    }

    let stmts = parse("drop view v;").unwrap();
    match &stmts[0] {
        Stmt::DropView(d) => assert_eq!(d.name, "v"),
        other => panic!("expected drop view, got {other:?}"),
    }

    err("create view v select 1;");
    err("create view v as;");
    err("create view v as insert into t values (1);");
    err("drop view;");
}

#[test]
fn parses_create_index() {
    let stmts = parse("create index idx_name on student (id);").unwrap();
    match &stmts[0] {
        Stmt::CreateIndex(c) => {
            assert_eq!(c.name, "idx_name");
            assert_eq!(c.table, "student");
            assert_eq!(c.column, "id");
        }
        other => panic!("expected create index, got {other:?}"),
    }
}

#[test]
fn parses_drop_index() {
    let stmts = parse("drop index idx_name;").unwrap();
    match &stmts[0] {
        Stmt::DropIndex(d) => assert_eq!(d.name, "idx_name"),
        other => panic!("expected drop index, got {other:?}"),
    }
}

#[test]
fn parses_drop_table() {
    let stmts = parse("drop table student;").unwrap();
    match &stmts[0] {
        Stmt::DropTable(d) => assert_eq!(d.name, "student"),
        other => panic!("expected drop table, got {other:?}"),
    }
    err("drop table;");
}

#[test]
fn index_ddl_syntax_errors() {
    err("create index on t (c);");
    err("create index i t (c);");
    err("create index i on (c);");
    err("create index i on t ();");
    err("create index i on t;");
    err("drop index;");
}

#[test]
fn parses_explain() {
    let stmts = parse("explain select 1;").unwrap();
    match &stmts[0] {
        Stmt::Explain(inner) => {
            assert_eq!(
                *inner.stmt,
                Stmt::Select(Box::new(chibidb::ast::SelectStmt {
                    distinct: false,
                    items: vec![chibidb::ast::SelectItem::Expr(chibidb::ast::Expr::Int(1))],
                    from: vec![],
                    joins: vec![],
                    on: vec![],
                    selection: None,
                    group_by: vec![],
                    having: None,
                    order_by: vec![],
                    limit: None,
                    set_ops: vec![],
                }))
            );
        }
        other => panic!("expected explain, got {other:?}"),
    }
    err("explain;");
    err("explain create table t (id int);");
}

#[test]
fn parses_group_by_and_having() {
    let s = select("select dept, count(*) from emp group by dept;");
    assert_eq!(s.group_by, [chibidb::ast::Expr::Column("dept".into())]);
    assert_eq!(s.having, None);

    let s = select("select dept, avg(score) from emp where age > 18 group by dept having avg(score) > 60;");
    assert_eq!(s.group_by, [chibidb::ast::Expr::Column("dept".into())]);
    assert_eq!(
        s.having.as_ref().unwrap().to_string(),
        "(> (avg score) 60)"
    );
}

#[test]
fn group_by_syntax_errors() {
    err("select 1 from t group;");
    err("select 1 from t group by;");
    err("select 1 from t having;");
    err("select 1 from t group by 1 having;");
}

#[test]
fn parses_order_by() {
    let s = select("select id from t order by id;");
    assert_eq!(s.order_by, [(chibidb::ast::Expr::Column("id".into()), false)]);

    let s = select("select id from t order by score desc, name asc, id;");
    assert_eq!(s.order_by.len(), 3);
    assert!(s.order_by[0].1);
    assert!(!s.order_by[1].1);
    assert!(!s.order_by[2].1, "default is asc");

    err("select 1 from t order;");
    err("select 1 from t order by;");
}

#[test]
fn parses_limit() {
    let s = select("select id from t limit 5;");
    assert_eq!(s.limit.as_ref().unwrap().count.to_string(), "5");
    assert_eq!(s.limit.as_ref().unwrap().offset, None);

    let s = select("select id from t limit 5 offset 2;");
    assert_eq!(s.limit.as_ref().unwrap().offset.as_ref().unwrap().to_string(), "2");

    err("select 1 from t limit;");
    err("select 1 from t limit 5 offset;");
    err("select 1 from t limit 'a';");
}
