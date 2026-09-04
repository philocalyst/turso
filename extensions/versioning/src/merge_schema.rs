//! Schema-IR parsing and the "match Dolt or refuse" schema merge matrix.
//!
//! `merge_schema` runs before any row merge: a schema conflict blocks the data
//! merge for that table and records a schema conflict, resolvable only by
//! `--ours`/`--theirs`. Rebuilds generate merged `CREATE TABLE` SQL with
//! base-order columns followed by ours-added then theirs-added.

use std::collections::HashSet;

/// One column of a parsed `CREATE TABLE`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ColIR {
    pub name: String,
    /// Everything after the column name: type plus column constraints.
    pub decl: String,
    pub notnull: bool,
    pub dflt: Option<String>,
    /// Position in the declared primary key, when this column is a PK.
    pub pk_pos: Option<usize>,
}

/// A parsed `CREATE TABLE` statement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SchemaIR {
    pub table: String,
    pub columns: Vec<ColIR>,
    pub pk: Vec<String>,
    /// The original create statement, kept verbatim.
    pub sql: String,
    /// Table-level constraints (`CHECK (...)`, `UNIQUE (...)`, ...) verbatim.
    pub constraints: Vec<String>,
    pub strict: bool,
}

impl SchemaIR {
    /// Column order stays stable: base columns first, then ours-added, then
    /// theirs-added. Column decls carry over from whichever side added them.
    pub fn rebuild(&self, ours_added: &[ColIR], theirs_added: &[ColIR]) -> String {
        let mut parts: Vec<String> = Vec::new();
        for col in &self.columns {
            parts.push(format!("\"{}\" {}", col.name, col.decl));
        }
        for col in ours_added {
            if !self.columns.iter().any(|c| c.name == col.name) {
                parts.push(format!("\"{}\" {}", col.name, col.decl));
            }
        }
        for col in theirs_added {
            if !self
                .columns
                .iter()
                .chain(ours_added.iter())
                .any(|c| c.name == col.name)
            {
                parts.push(format!("\"{}\" {}", col.name, col.decl));
            }
        }
        let body = parts.join(", ");
        let mut sql = format!("CREATE TABLE \"{}\" ({body})", self.table);
        if self.strict {
            sql.push_str(" STRICT");
        }
        sql
    }
}

/// Outcome of a schema merge.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SchemaDecision {
    Clean(SchemaIR),
    Conflict(String),
}

/// How one side's schema moved relative to base.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SideChange {
    Unchanged,
    Compatible,
    Breaking,
}

/// Merge base/ours/theirs table schemas.
///
/// The caller passes the table's schema from each snapshot; `None` means the
/// table does not exist there. Cases where the merged result is "table gone"
/// (deleted on both, or deleted on one side while the other never moved) are
/// decided by the caller from the presence pattern; every case where the table
/// survives lands here.
pub fn merge_schema(
    base: Option<&SchemaIR>,
    ours: Option<&SchemaIR>,
    theirs: Option<&SchemaIR>,
) -> SchemaDecision {
    let ours = match ours {
        Some(o) => o,
        None => {
            return match (base, theirs) {
                (None, Some(t)) => SchemaDecision::Clean(t.clone()),
                (None, None) => SchemaDecision::Conflict("table deleted on both sides".to_string()),
                // Base existed, ours deleted it, theirs still exists.
                (Some(b), Some(t)) => {
                    if same_schema(t, b) {
                        // Theirs didn't change the table: deletion still
                        // conflicts because theirs expects it to exist.
                        SchemaDecision::Conflict(
                            "table deleted on ours, unchanged on theirs".to_string(),
                        )
                    } else {
                        // Theirs modified the table: conflict.
                        SchemaDecision::Conflict(
                            "table deleted on ours, modified on theirs".to_string(),
                        )
                    }
                }
                // Base existed, ours deleted it, theirs is absent:
                // conflict — we cannot tell whether theirs deleted it
                // cleanly or never had it, so refuse.
                (Some(_), None) => {
                    SchemaDecision::Conflict("table deleted on ours, absent on theirs".to_string())
                }
            };
        }
    };
    let theirs = match theirs {
        Some(t) => t,
        None => {
            return match base {
                None => SchemaDecision::Clean(ours.clone()),
                // Base existed, theirs deleted it, ours is present:
                // distinguish whether ours modified the table from base.
                Some(b) => {
                    if same_schema(ours, b) {
                        SchemaDecision::Conflict(
                            "table deleted on theirs, unchanged on ours".to_string(),
                        )
                    } else {
                        SchemaDecision::Conflict(
                            "table deleted on theirs, modified on ours".to_string(),
                        )
                    }
                }
            };
        }
    };
    let base = match base {
        Some(b) => b,
        None => {
            if same_schema(ours, theirs) {
                return SchemaDecision::Clean(ours.clone());
            }
            return SchemaDecision::Conflict("table added differently on both sides".to_string());
        }
    };

    // PK-signature rule: both sides changed the PK to different results and
    // the data merge is refused outright.
    if pk_changed(ours, base) && pk_changed(theirs, base) && ours.pk != theirs.pk {
        return SchemaDecision::Conflict("primary key changed on both sides".to_string());
    }

    let o_change = classify(ours, base);
    let t_change = classify(theirs, base);
    use SideChange::*;
    match (o_change, t_change) {
        (Unchanged, Unchanged) => SchemaDecision::Clean(base.clone()),
        (Unchanged, Compatible) => SchemaDecision::Clean(theirs.clone()),
        (Unchanged, Breaking) => SchemaDecision::Clean(theirs.clone()),
        (Compatible, Unchanged) => SchemaDecision::Clean(ours.clone()),
        (Compatible, Compatible) => union_columns(base, ours, theirs),
        (Compatible, Breaking) | (Breaking, Compatible) | (Breaking, Breaking) => {
            if same_schema(ours, theirs) {
                SchemaDecision::Clean(ours.clone())
            } else {
                SchemaDecision::Conflict(conflict_detail(o_change, t_change))
            }
        }
        (Breaking, Unchanged) => SchemaDecision::Clean(ours.clone()),
    }
}

/// Both sides only added columns: union them, refusing same-name columns with
/// different decls. Table-level constraints are also unioned; differing
/// constraint additions conflict.
fn union_columns(base: &SchemaIR, ours: &SchemaIR, theirs: &SchemaIR) -> SchemaDecision {
    let ours_added = added_columns(base, ours);
    let theirs_added = added_columns(base, theirs);
    for o in &ours_added {
        if let Some(t) = theirs_added.iter().find(|t| t.name == o.name) {
            if o.decl != t.decl {
                return SchemaDecision::Conflict(format!(
                    "column '{}' added with different declarations on both sides",
                    o.name
                ));
            }
        }
    }
    // Pre-compute new constraints on each side. theirs_new_set is a
    // HashSet so `contains` is O(1) instead of re-scanning theirs on each
    // iteration of ours_new.
    let ours_new: Vec<&str> = ours
        .constraints
        .iter()
        .filter(|c| !base.constraints.contains(c))
        .map(|s| s.as_str())
        .collect();
    let theirs_new_set: HashSet<&str> = theirs
        .constraints
        .iter()
        .filter(|c| !base.constraints.contains(c))
        .map(|s| s.as_str())
        .collect();

    let mut constraints = base.constraints.clone();
    for c in &ours_new {
        if !constraints.iter().any(|x| x == *c) {
            if theirs_new_set.contains(*c) {
                // Both sides added the same constraint: keep it.
                constraints.push(c.to_string());
            } else if !theirs_new_set.is_empty() {
                // Ours added a constraint theirs doesn't have, and theirs
                // also added different constraints: conflict.
                return SchemaDecision::Conflict(
                    "conflicting constraint additions on both sides".to_string(),
                );
            } else {
                // Ours added a constraint theirs doesn't have and theirs
                // added nothing new: ours-only addition, keep it.
                constraints.push(c.to_string());
            }
        }
    }
    for c in &theirs_new_set {
        if !constraints.iter().any(|x| x == *c) {
            constraints.push(c.to_string());
        }
    }
    SchemaDecision::Clean(SchemaIR {
        table: base.table.clone(),
        columns: merged_columns(base, &ours_added, &theirs_added),
        pk: base.pk.clone(),
        sql: base.sql.clone(),
        constraints,
        strict: base.strict,
    })
}

/// The base columns followed by both sides' added columns, deduplicated.
fn merged_columns(base: &SchemaIR, ours_added: &[ColIR], theirs_added: &[ColIR]) -> Vec<ColIR> {
    let mut columns = base.columns.clone();
    let mut names: Vec<String> = columns.iter().map(|c| c.name.clone()).collect();
    for col in ours_added {
        if !names.iter().any(|n| n == &col.name) {
            names.push(col.name.clone());
            columns.push(col.clone());
        }
    }
    for col in theirs_added {
        if !names.iter().any(|n| n == &col.name) {
            names.push(col.name.clone());
            columns.push(col.clone());
        }
    }
    columns
}

/// Columns present in `side` but not in `base`.
fn added_columns(base: &SchemaIR, side: &SchemaIR) -> Vec<ColIR> {
    side.columns
        .iter()
        .filter(|col| !base.columns.iter().any(|b| b.name == col.name))
        .cloned()
        .collect()
}

/// Classify one side's change against base: nothing, additive-only, or
/// breaking (drop/rename/type change/PK change/not-null add/constraint removal).
fn classify(side: &SchemaIR, base: &SchemaIR) -> SideChange {
    if same_schema(side, base) {
        return SideChange::Unchanged;
    }
    if side.pk != base.pk {
        return SideChange::Breaking;
    }
    if side.strict != base.strict {
        return SideChange::Breaking;
    }
    // Constraint removal is breaking; constraint addition is compatible.
    if side.constraints.len() < base.constraints.len() {
        return SideChange::Breaking;
    }
    // Modified constraints (same count, different content) are breaking.
    if side.constraints.len() == base.constraints.len() {
        let mut a: Vec<&str> = side.constraints.iter().map(|s| s.as_str()).collect();
        let mut b: Vec<&str> = base.constraints.iter().map(|s| s.as_str()).collect();
        a.sort_unstable();
        b.sort_unstable();
        if a != b {
            return SideChange::Breaking;
        }
    }
    if side.columns.len() < base.columns.len() {
        return SideChange::Breaking;
    }
    for (i, base_col) in base.columns.iter().enumerate() {
        let Some(side_col) = side.columns.get(i) else {
            return SideChange::Breaking;
        };
        if !same_column(base_col, side_col) {
            return SideChange::Breaking;
        }
    }
    // Extra columns must be nullable and outside the primary key.
    for col in &side.columns[base.columns.len()..] {
        if col.notnull || col.pk_pos.is_some() {
            return SideChange::Breaking;
        }
    }
    SideChange::Compatible
}

/// Byte-level schema equality: columns, decls, pk order, strict flag,
/// and table-level constraints.
pub fn same_schema(a: &SchemaIR, b: &SchemaIR) -> bool {
    if a.strict != b.strict || a.pk != b.pk {
        return false;
    }
    if a.constraints.len() != b.constraints.len() {
        return false;
    }
    let mut a_cons: Vec<&str> = a.constraints.iter().map(|s| s.as_str()).collect();
    let mut b_cons: Vec<&str> = b.constraints.iter().map(|s| s.as_str()).collect();
    a_cons.sort_unstable();
    b_cons.sort_unstable();
    if a_cons != b_cons {
        return false;
    }
    if a.columns.len() != b.columns.len() {
        return false;
    }
    a.columns
        .iter()
        .zip(b.columns.iter())
        .all(|(x, y)| same_column(x, y))
}

/// Whether one side moved the PK column set or order off base.
fn pk_changed(side: &SchemaIR, base: &SchemaIR) -> bool {
    side.pk != base.pk
}

/// Two columns are the same if name, decl, notnull, dflt, and pk position
/// agree. Types are compared case-insensitively through the normalized decl.
fn same_column(a: &ColIR, b: &ColIR) -> bool {
    a.name == b.name
        && normalize_decl(&a.decl) == normalize_decl(&b.decl)
        && a.notnull == b.notnull
        && a.dflt == b.dflt
        && a.pk_pos == b.pk_pos
}

/// Case-insensitive decl comparison: SQL identifiers are case-folded.
fn normalize_decl(decl: &str) -> String {
    decl.trim().to_uppercase()
}

/// Human detail for a refusal, used in the `schema conflict in table` error.
fn conflict_detail(o: SideChange, t: SideChange) -> String {
    use SideChange::*;
    match (o, t) {
        (Breaking, Breaking) => "breaking schema change on both sides".to_string(),
        (Compatible, Breaking) => "breaking schema change on theirs".to_string(),
        (Breaking, Compatible) => "breaking schema change on ours".to_string(),
        _ => "incompatible schema changes".to_string(),
    }
}

// ============================================================================
// CREATE TABLE parsing
// ============================================================================

/// Parse a `CREATE TABLE` statement into a `SchemaIR`.
///
/// `None` means the SQL is not a create-table statement; anything else is an
/// error the caller surfaces rather than a silent pass.
pub fn parse_schema(sql: &str) -> Option<SchemaIR> {
    let tokens = tokenize(sql);
    let mut i = 0;
    if !next_is(&tokens, &mut i, "CREATE") {
        return None;
    }
    if !next_is(&tokens, &mut i, "TABLE") {
        return None;
    }
    while next_is(&tokens, &mut i, "IF") {
        let _ = next_is(&tokens, &mut i, "NOT");
        let _ = next_is(&tokens, &mut i, "EXISTS");
    }
    let table = unquote_ident(tokens.get(i)?.clone());
    i += 1;
    if !next_is(&tokens, &mut i, "(") {
        return None;
    }
    let body_items = collect_until_paren(&tokens, &mut i);
    let mut columns = Vec::new();
    let mut constraints = Vec::new();
    let mut pk = Vec::new();
    for item in body_items {
        if let Some(table_pk) = parse_table_pk(&item) {
            pk = table_pk;
        } else if is_table_constraint(&item) {
            constraints.push(item);
        } else if let Some((name, col)) = parse_column(&item) {
            if let Some(pos) = col.pk_pos {
                if !pk.contains(&name) {
                    let insert_at = pos.min(pk.len());
                    pk.insert(insert_at, name.clone());
                }
            }
            columns.push(col);
        } else {
            constraints.push(item);
        }
    }
    // A table-level PRIMARY KEY clause overrides inline pks and defines order.
    let strict = next_is(&tokens, &mut i, "STRICT");
    Some(SchemaIR {
        table,
        columns,
        pk,
        sql: sql.to_string(),
        constraints,
        strict,
    })
}

/// `PRIMARY KEY (a, b)` table constraint → the ordered pk column list.
fn parse_table_pk(item: &str) -> Option<Vec<String>> {
    let tokens = tokenize(item);
    let mut i = 0;
    if !next_is(&tokens, &mut i, "PRIMARY") || !next_is(&tokens, &mut i, "KEY") {
        return None;
    }
    read_paren_list(&tokens, &mut i)
}

/// A body item that opens with a table-level constraint keyword.
fn is_table_constraint(item: &str) -> bool {
    let first = tokenize(item).into_iter().next().unwrap_or_default();
    matches!(
        first.to_uppercase().as_str(),
        "CHECK" | "UNIQUE" | "FOREIGN" | "PRIMARY"
    )
}

/// One column definition → name and `ColIR`.
fn parse_column(item: &str) -> Option<(String, ColIR)> {
    let tokens = tokenize(item);
    let name = unquote_ident(tokens.first()?.clone());
    let mut i = 1;
    let mut decl_parts: Vec<String> = Vec::new();
    let mut notnull = false;
    let mut dflt = None;
    let mut pk_pos = None;
    let mut pk_index = 0;
    while i < tokens.len() {
        let tok = tokens[i].clone();
        match tok.to_uppercase().as_str() {
            "NOT" => {
                i += 1;
                if next_is(&tokens, &mut i, "NULL") {
                    notnull = true;
                    decl_parts.push("NOT NULL".to_string());
                }
            }
            "NULL" => {
                decl_parts.push("NULL".to_string());
                i += 1;
            }
            "DEFAULT" => {
                i += 1;
                let mut expr = String::new();
                while i < tokens.len() && !is_default_boundary(&tokens[i]) {
                    if !expr.is_empty() {
                        expr.push(' ');
                    }
                    expr.push_str(&tokens[i]);
                    i += 1;
                }
                dflt = Some(expr.clone());
                decl_parts.push(format!("DEFAULT {expr}"));
            }
            "PRIMARY" => {
                i += 1;
                let _ = next_is(&tokens, &mut i, "KEY");
                decl_parts.push("PRIMARY KEY".to_string());
                if next_is(&tokens, &mut i, "AUTOINCREMENT") {
                    decl_parts.push("AUTOINCREMENT".to_string());
                }
                if pk_pos.is_none() {
                    pk_pos = Some(pk_index);
                    pk_index += 1;
                }
            }
            "AUTOINCREMENT" => {
                decl_parts.push("AUTOINCREMENT".to_string());
                i += 1;
            }
            "CHECK" => {
                let (expr, consumed) = capture_paren_expr(&tokens, i);
                decl_parts.push(format!("CHECK ({expr})"));
                i += consumed;
            }
            "UNIQUE" => {
                decl_parts.push("UNIQUE".to_string());
                i += 1;
            }
            "COLLATE" => {
                i += 1;
                if let Some(coll) = tokens.get(i) {
                    decl_parts.push(format!("COLLATE {coll}"));
                    i += 1;
                }
            }
            "REFERENCES" => {
                let mut ref_part = String::from("REFERENCES");
                i += 1;
                while i < tokens.len() {
                    ref_part.push(' ');
                    ref_part.push_str(&tokens[i]);
                    i += 1;
                }
                decl_parts.push(ref_part);
            }
            _ => {
                // Type or anything else: keep verbatim.
                if !matches!(tokens[i].as_str(), ",") {
                    decl_parts.push(tokens[i].clone());
                }
                i += 1;
            }
        }
    }
    Some((
        name.clone(),
        ColIR {
            name,
            decl: decl_parts.join(" "),
            notnull,
            dflt,
            pk_pos,
        },
    ))
}

/// Words that end a DEFAULT expression: column-level keywords.
fn is_default_boundary(tok: &str) -> bool {
    matches!(
        tok.to_uppercase().as_str(),
        "NOT"
            | "NULL"
            | "PRIMARY"
            | "DEFAULT"
            | "CHECK"
            | "UNIQUE"
            | "COLLATE"
            | "REFERENCES"
            | ","
    )
}

/// Capture a parenthesized expression following `CHECK`, returning the text
/// and how many tokens it consumed (keyword through the closing paren).
fn capture_paren_expr(tokens: &[String], i: usize) -> (String, usize) {
    let mut depth = 0usize;
    let mut parts = Vec::new();
    let mut j = i;
    while j < tokens.len() && tokens[j] != "(" {
        j += 1;
    }
    if j >= tokens.len() {
        return (String::new(), 0);
    }
    j += 1;
    while j < tokens.len() {
        match tokens[j].as_str() {
            "(" => {
                depth += 1;
                parts.push(tokens[j].clone());
            }
            ")" => {
                if depth == 0 {
                    j += 1;
                    break;
                }
                depth -= 1;
                parts.push(tokens[j].clone());
            }
            _ => parts.push(tokens[j].clone()),
        }
        j += 1;
    }
    (parts.join(" "), j - i)
}

/// Collect the top-level comma-separated items inside a `( ... )` group.
fn collect_until_paren(tokens: &[String], i: &mut usize) -> Vec<String> {
    let mut depth = 1usize;
    let mut items = Vec::new();
    let mut current = String::new();
    while *i < tokens.len() {
        match tokens[*i].as_str() {
            "(" => {
                depth += 1;
                current.push('(');
                current.push(' ');
                *i += 1;
            }
            ")" => {
                depth -= 1;
                if depth == 0 {
                    if !current.trim().is_empty() {
                        items.push(current.trim().to_string());
                    }
                    *i += 1;
                    break;
                }
                current.push(')');
                current.push(' ');
                *i += 1;
            }
            "," if depth == 1 => {
                items.push(current.trim().to_string());
                current.clear();
                *i += 1;
            }
            tok => {
                current.push_str(tok);
                current.push(' ');
                *i += 1;
            }
        }
    }
    items
}

/// Consume `tok` at position `i` if present, advancing past it.
fn next_is(tokens: &[String], i: &mut usize, tok: &str) -> bool {
    if tokens.get(*i).is_some_and(|t| t.eq_ignore_ascii_case(tok)) {
        *i += 1;
        true
    } else {
        false
    }
}

/// Read `(a, b, c)` starting at the `(` token.
fn read_paren_list(tokens: &[String], i: &mut usize) -> Option<Vec<String>> {
    if !next_is(tokens, i, "(") {
        return None;
    }
    let mut names = Vec::new();
    while *i < tokens.len() && tokens[*i] != ")" {
        let tok = tokens[*i].clone();
        if tok != "," {
            names.push(unquote_ident(tok));
        }
        *i += 1;
    }
    if tokens.get(*i) != Some(&")".to_string()) {
        return None;
    }
    *i += 1;
    if names.is_empty() {
        None
    } else {
        Some(names)
    }
}

/// Strip one layer of SQL identifier quoting (`"name"`, `` `name` ``, or
/// `[name]`). A table's `CREATE TABLE` from `sqlite_schema` and the merged
/// `CREATE TABLE` the rebuild emits both quote identifiers, so parsing must
/// normalize them or the column names stop matching the snapshot's.
fn unquote_ident(tok: String) -> String {
    let bytes = tok.as_bytes();
    let (open, _close) = match (bytes.first(), bytes.last()) {
        (Some(b'"'), Some(b'"')) => (b'"', b'"'),
        (Some(b'`'), Some(b'`')) => (b'`', b'`'),
        (Some(b'['), Some(b']')) => (b'[', b']'),
        _ => return tok,
    };
    let inner = &tok[1..tok.len() - 1];
    match open {
        b'"' => inner.replace("\"\"", "\""),
        b'`' => inner.replace("``", "`"),
        _ => inner.to_string(),
    }
}

/// Split SQL into words, keeping parenthesized groups glued to their opener
/// keyword so `CHECK (a < 3)` stays one item.
fn tokenize(sql: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut quote: Option<char> = None;
    let mut i = 0;
    let chars: Vec<char> = sql.chars().collect();
    while i < chars.len() {
        let c = chars[i];
        if let Some(q) = quote {
            current.push(c);
            if c == q {
                quote = None;
            }
            i += 1;
            continue;
        }
        match c {
            '\'' | '"' | '`' => {
                quote = Some(c);
                current.push(c);
                i += 1;
            }
            c if c.is_whitespace() => {
                if !current.is_empty() {
                    tokens.push(std::mem::take(&mut current));
                }
                i += 1;
            }
            '(' => {
                if !current.is_empty() {
                    tokens.push(std::mem::take(&mut current));
                }
                tokens.push("(".to_string());
                i += 1;
            }
            ')' => {
                if !current.is_empty() {
                    tokens.push(std::mem::take(&mut current));
                }
                tokens.push(")".to_string());
                i += 1;
            }
            ',' => {
                if !current.is_empty() {
                    tokens.push(std::mem::take(&mut current));
                }
                tokens.push(",".to_string());
                i += 1;
            }
            _ => {
                current.push(c);
                i += 1;
            }
        }
    }
    if !current.is_empty() {
        tokens.push(current);
    }
    tokens
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ir(_table: &str, sql: &str) -> SchemaIR {
        parse_schema(sql).unwrap_or_else(|| panic!("failed to parse {sql}"))
    }

    #[test]
    fn parse_inline_pk_and_types() {
        let s = ir("t", "CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)");
        assert_eq!(s.table, "t");
        assert_eq!(s.columns.len(), 2);
        assert_eq!(s.columns[0].name, "id");
        assert_eq!(s.pk, vec!["id"]);
        assert_eq!(s.columns[1].name, "v");
        assert!(!s.strict);
    }

    #[test]
    fn parse_table_level_pk_order() {
        let s = ir("t", "CREATE TABLE t (a TEXT, b TEXT, PRIMARY KEY (a, b))");
        assert_eq!(s.pk, vec!["a", "b"]);
    }

    #[test]
    fn parse_quoted_identifiers_unquotes_names() {
        // `merged_sql` and `sqlite_schema` both quote identifiers; parsing must
        // strip the quotes so the names still match the snapshot's plain ones.
        let s =
            parse_schema("CREATE TABLE \"t\" (\"id\" INTEGER PRIMARY KEY, \"v\" TEXT)").unwrap();
        assert_eq!(s.table, "t");
        assert_eq!(s.columns[0].name, "id");
        assert_eq!(s.columns[1].name, "v");
        assert_eq!(s.pk, vec!["id"]);
    }

    #[test]
    fn parse_bracketed_and_backtick_identifiers() {
        let s = parse_schema("CREATE TABLE [t] ([id] INTEGER PRIMARY KEY, `v` TEXT)").unwrap();
        assert_eq!(s.table, "t");
        assert_eq!(s.columns[0].name, "id");
        assert_eq!(s.columns[1].name, "v");
    }

    #[test]
    fn parse_strict_and_notnull() {
        let s = ir(
            "t",
            "CREATE TABLE t (id INTEGER NOT NULL, v TEXT, PRIMARY KEY (id)) STRICT",
        );
        assert!(s.strict);
        assert!(s.columns[0].notnull);
        assert_eq!(s.pk, vec!["id"]);
    }

    #[test]
    fn parse_default_and_check() {
        let s = ir(
            "t",
            "CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT DEFAULT 'x' CHECK (v <> ''))",
        );
        assert_eq!(s.columns[1].dflt.as_deref(), Some("'x'"));
        assert!(s.columns[1].decl.contains("CHECK"));
    }

    #[test]
    fn parse_types_case_insensitively() {
        let a = ir("t", "CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)");
        let b = ir("t", "CREATE TABLE t (id integer primary key, v text)");
        assert!(same_schema(&a, &b));
    }

    #[test]
    fn pk_unchanged_matrix_take_base() {
        let base = ir("t", "CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)");
        let ours = base.clone();
        let theirs = base.clone();
        let decision = merge_schema(Some(&base), Some(&ours), Some(&theirs));
        assert!(matches!(decision, SchemaDecision::Clean(_)));
    }

    #[test]
    fn pk_changed_on_both_sides_to_different_results_refused() {
        let base = ir("t", "CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)");
        let ours = ir("t", "CREATE TABLE t (a TEXT PRIMARY KEY, v TEXT)");
        let theirs = ir("t", "CREATE TABLE t (b TEXT PRIMARY KEY, v TEXT)");
        let decision = merge_schema(Some(&base), Some(&ours), Some(&theirs));
        match decision {
            SchemaDecision::Conflict(detail) => {
                assert_eq!(detail, "primary key changed on both sides")
            }
            _ => panic!("expected conflict"),
        }
    }

    #[test]
    fn pk_changed_on_one_side_is_breaking_and_wins() {
        let base = ir("t", "CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)");
        let ours = ir("t", "CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)");
        let theirs = ir("t", "CREATE TABLE t (a TEXT PRIMARY KEY, v TEXT)");
        let decision = merge_schema(Some(&base), Some(&ours), Some(&theirs));
        assert!(matches!(decision, SchemaDecision::Clean(_)));
    }

    #[test]
    fn compatible_unchanged_takes_ours() {
        let base = ir("t", "CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)");
        let ours = ir(
            "t",
            "CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT, extra TEXT)",
        );
        let theirs = base.clone();
        let decision = merge_schema(Some(&base), Some(&ours), Some(&theirs));
        assert_eq!(decision, SchemaDecision::Clean(ours));
    }

    #[test]
    fn compatible_compatible_unions_columns() {
        let base = ir("t", "CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)");
        let ours = ir(
            "t",
            "CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT, a TEXT)",
        );
        let theirs = ir(
            "t",
            "CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT, b TEXT)",
        );
        let decision = merge_schema(Some(&base), Some(&ours), Some(&theirs));
        match decision {
            SchemaDecision::Clean(merged) => {
                let names: Vec<&str> = merged.columns.iter().map(|c| c.name.as_str()).collect();
                assert_eq!(names, vec!["id", "v", "a", "b"]);
            }
            _ => panic!("expected clean"),
        }
    }

    #[test]
    fn compatible_compatible_same_name_different_decl_conflicts() {
        let base = ir("t", "CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)");
        let ours = ir(
            "t",
            "CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT, a TEXT)",
        );
        let theirs = ir(
            "t",
            "CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT, a INTEGER)",
        );
        let decision = merge_schema(Some(&base), Some(&ours), Some(&theirs));
        assert!(matches!(decision, SchemaDecision::Conflict(_)));
    }

    #[test]
    fn breaking_breaking_conflicts_unless_identical() {
        let base = ir("t", "CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)");
        let ours = ir("t", "CREATE TABLE t (a TEXT PRIMARY KEY, w INTEGER)");
        let theirs = ir("t", "CREATE TABLE t (b TEXT PRIMARY KEY, w INTEGER)");
        let decision = merge_schema(Some(&base), Some(&ours), Some(&theirs));
        assert!(matches!(decision, SchemaDecision::Conflict(_)));
        let ours = ir("t", "CREATE TABLE t (a TEXT PRIMARY KEY, w INTEGER)");
        let theirs = ours.clone();
        assert!(matches!(
            merge_schema(Some(&base), Some(&ours), Some(&theirs)),
            SchemaDecision::Clean(_)
        ));
    }

    #[test]
    fn breaking_unchanged_takes_ours() {
        let base = ir("t", "CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)");
        let ours = ir("t", "CREATE TABLE t (a TEXT PRIMARY KEY, v TEXT)");
        let theirs = base.clone();
        let decision = merge_schema(Some(&base), Some(&ours), Some(&theirs));
        assert_eq!(decision, SchemaDecision::Clean(ours));
    }

    #[test]
    fn added_on_one_side_only_is_clean() {
        let ours = ir("t", "CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)");
        let decision = merge_schema(None, Some(&ours), None);
        assert_eq!(decision, SchemaDecision::Clean(ours.clone()));
        let decision = merge_schema(None, None, Some(&ours));
        assert_eq!(decision, SchemaDecision::Clean(ours));
    }

    #[test]
    fn added_on_both_identically_is_clean() {
        let ours = ir("t", "CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)");
        let theirs = ours.clone();
        let decision = merge_schema(None, Some(&ours), Some(&theirs));
        assert!(matches!(decision, SchemaDecision::Clean(_)));
    }

    #[test]
    fn added_on_both_differently_conflicts() {
        let ours = ir("t", "CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)");
        let theirs = ir("t", "CREATE TABLE t (a TEXT PRIMARY KEY, v TEXT)");
        let decision = merge_schema(None, Some(&ours), Some(&theirs));
        assert!(matches!(decision, SchemaDecision::Conflict(_)));
    }

    #[test]
    fn rebuild_base_then_added() {
        let base = ir("t", "CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)");
        let ours = ir(
            "t",
            "CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT, a TEXT)",
        );
        let theirs = ir(
            "t",
            "CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT, b TEXT)",
        );
        let ours_added = added_columns(&base, &ours);
        let theirs_added = added_columns(&base, &theirs);
        let rebuilt = base.rebuild(&ours_added, &theirs_added);
        assert!(rebuilt.contains(
            "CREATE TABLE \"t\" (\"id\" INTEGER PRIMARY KEY, \"v\" TEXT, \"a\" TEXT, \"b\" TEXT)"
        ));
    }

    #[test]
    fn b1_delete_on_ours_unmodified_on_theirs_is_gone() {
        let base = ir("t", "CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)");
        let theirs = base.clone();
        let decision = merge_schema(Some(&base), None, Some(&theirs));
        assert!(matches!(decision, SchemaDecision::Conflict(_)));
        assert_eq!(
            decision,
            SchemaDecision::Conflict("table deleted on ours, unchanged on theirs".to_string())
        );
    }

    #[test]
    fn b1_delete_on_ours_modified_on_theirs_conflicts() {
        let base = ir("t", "CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)");
        let theirs = ir(
            "t",
            "CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT, extra TEXT)",
        );
        let decision = merge_schema(Some(&base), None, Some(&theirs));
        assert!(matches!(decision, SchemaDecision::Conflict(_)));
        assert_eq!(
            decision,
            SchemaDecision::Conflict("table deleted on ours, modified on theirs".to_string())
        );
    }

    #[test]
    fn b1_delete_on_theirs_unmodified_on_ours_is_gone() {
        let base = ir("t", "CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)");
        let ours = base.clone();
        let decision = merge_schema(Some(&base), Some(&ours), None);
        assert!(matches!(decision, SchemaDecision::Conflict(_)));
        assert_eq!(
            decision,
            SchemaDecision::Conflict("table deleted on theirs, unchanged on ours".to_string())
        );
    }

    #[test]
    fn b1_delete_on_theirs_modified_on_ours_conflicts() {
        let base = ir("t", "CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)");
        let ours = ir(
            "t",
            "CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT, extra TEXT)",
        );
        let decision = merge_schema(Some(&base), Some(&ours), None);
        assert!(matches!(decision, SchemaDecision::Conflict(_)));
        assert_eq!(
            decision,
            SchemaDecision::Conflict("table deleted on theirs, modified on ours".to_string())
        );
    }

    #[test]
    fn b1_no_base_added_on_theirs_only_takes_theirs() {
        let theirs = ir("t", "CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)");
        let decision = merge_schema(None, None, Some(&theirs));
        assert_eq!(decision, SchemaDecision::Clean(theirs));
    }

    #[test]
    fn b1_no_base_added_on_ours_only_takes_ours() {
        let ours = ir("t", "CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)");
        let decision = merge_schema(None, Some(&ours), None);
        assert_eq!(decision, SchemaDecision::Clean(ours));
    }

    #[test]
    fn b2_add_check_on_one_side_is_compatible() {
        let base = ir("t", "CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)");
        let ours = ir(
            "t",
            "CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT, CHECK (id > 0))",
        );
        let decision = merge_schema(Some(&base), Some(&ours), Some(&base));
        assert!(matches!(decision, SchemaDecision::Clean(_)));
    }

    #[test]
    fn b2_differing_check_constraints_conflict() {
        let base = ir("t", "CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)");
        let ours = ir(
            "t",
            "CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT, CHECK (id > 0))",
        );
        let theirs = ir(
            "t",
            "CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT, CHECK (id > 1))",
        );
        let decision = merge_schema(Some(&base), Some(&ours), Some(&theirs));
        assert!(matches!(decision, SchemaDecision::Conflict(_)));
    }

    #[test]
    fn b2_both_added_same_check_takes_base_with_check() {
        let base = ir("t", "CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)");
        let ours = ir(
            "t",
            "CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT, CHECK (id > 0))",
        );
        let theirs = ir(
            "t",
            "CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT, CHECK (id > 0))",
        );
        let decision = merge_schema(Some(&base), Some(&ours), Some(&theirs));
        match decision {
            SchemaDecision::Clean(merged) => {
                assert!(
                    merged.constraints.iter().any(|c| c.contains("CHECK")),
                    "merged should contain the CHECK constraint, got {:?}",
                    merged.constraints
                );
            }
            _ => panic!("expected clean"),
        }
    }
}
