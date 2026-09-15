//! EXPLAIN: renders the logical plan (before lowering) and the physical plan,
//! so the optimizer's effect and the chosen access paths are both visible.

use crate::sql::ast::{ExplainStmt, Stmt};
use crate::sql::result::ResultSet;
use crate::{Database, Error, Result};

use super::logical;

pub(crate) fn execute_explain(db: &Database, e: &ExplainStmt) -> Result<ResultSet> {
    match &*e.stmt {
        Stmt::Select(s) => {
            // Plan once: the layers share the logical plan instead of rebuilding it.
            let layers = super::plan_statement_layers(db, &Stmt::Select(s.clone()))?;
            let mut out = String::from("LogicalPlan:\n");
            match &layers.logical {
                Some(node) => out.push_str(&logical::logical_tree(node)),
                None => out.push_str("  (no logical plan)\n"),
            }
            out.push_str("PhysicalPlan:\n");
            match &layers.physical {
                Some(op) => out.push_str(&crate::exec::operator::physical_tree(op.as_ref())),
                None => out.push_str("  (no physical plan)\n"),
            }
            Ok(ResultSet::Message(out))
        }
        _ => Err(Error::Runtime("explain supports select only".into())),
    }
}
