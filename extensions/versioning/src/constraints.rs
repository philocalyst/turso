//! Constraint detectors over the merged working set, before commit.
//!
//! Mirrors doltlite's `verify_constraints.c`. Each detector maps a violated
//! rule onto a lowercase `violation_type` (`foreign key`, `unique`, `check`,
//! `not null`, `strict`) that the violations vtable renders verbatim.

use crate::merge_schema::SchemaIR;
use crate::vtab_log::{VcRow, VcValue};

/// One table of a merged working set.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MergedTable {
    pub name: String,
    pub schema: SchemaIR,
    pub rows: Vec<VcRow>,
}

/// The merged working set `dolt_verify_constraints` inspects.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MergedWork {
    pub tables: Vec<MergedTable>,
}

/// The kind of a violated constraint. Display strings are the lowercase
/// `violation_type` values dolt emits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ViolationKind {
    ForeignKey,
    Unique,
    Check,
    NotNull,
    Strict,
}

impl std::fmt::Display for ViolationKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}",
            match self {
                ViolationKind::ForeignKey => "foreign key",
                ViolationKind::Unique => "unique",
                ViolationKind::Check => "check",
                ViolationKind::NotNull => "not null",
                ViolationKind::Strict => "strict",
            }
        )
    }
}

/// One detected constraint violation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Violation {
    pub table: String,
    pub kind: ViolationKind,
    pub row_pk: Vec<VcValue>,
    pub detail: String,
}

/// Run every detector over the merged working set.
///
/// The returned order is stable: foreign key, unique, check, not null, strict,
/// walking tables in the merged order.
pub fn verify_constraints(merged: &MergedWork, schemas: &[SchemaIR]) -> Vec<Violation> {
    let mut out = Vec::new();
    for table in &merged.tables {
        check_foreign_keys(&mut out, merged, table, schemas);
        check_unique(&mut out, table);
        check_checks(&mut out, table);
        check_not_null(&mut out, table);
        check_strict(&mut out, table);
    }
    out
}

/// FK: a child row whose foreign-key cells match no parent row. A missing
/// parent table or a foreign-key column missing from a schema is a violation
/// too: silently passing would let a broken FK definition merge in unchecked.
fn check_foreign_keys(
    out: &mut Vec<Violation>,
    merged: &MergedWork,
    table: &MergedTable,
    schemas: &[SchemaIR],
) {
    for fk in extract_foreign_keys(&table.schema) {
        let Some(parent) = schemas.iter().find(|s| s.table == fk.parent_table) else {
            // The parent table is absent from the merged set: every child row
            // with a non-NULL key lacks its required parent.
            for row in &table.rows {
                if let Some(_key) = fk_key_of(row, &table.schema.columns, &fk.columns) {
                    out.push(Violation {
                        table: table.name.clone(),
                        kind: ViolationKind::ForeignKey,
                        row_pk: pk_of(row, &table.schema),
                        detail: format!(
                            "foreign key ({}) -> {} ({})",
                            fk.columns.join(", "),
                            fk.parent_table,
                            fk.parent_columns.join(", ")
                        ),
                    });
                }
            }
            continue;
        };
        let parent_rows = merged
            .tables
            .iter()
            .find(|t| t.name == fk.parent_table)
            .map(|t| t.rows.as_slice())
            .unwrap_or(&[]);
        let Some(_child_indices) = column_indices(&table.schema.columns, &fk.columns) else {
            for row in &table.rows {
                out.push(Violation {
                    table: table.name.clone(),
                    kind: ViolationKind::ForeignKey,
                    row_pk: pk_of(row, &table.schema),
                    detail: format!(
                        "foreign key columns not found in table {}: ({})",
                        table.name,
                        fk.columns.join(", ")
                    ),
                });
            }
            continue;
        };
        let Some(_parent_indices) = column_indices(&parent.columns, &fk.parent_columns) else {
            for row in &table.rows {
                out.push(Violation {
                    table: table.name.clone(),
                    kind: ViolationKind::ForeignKey,
                    row_pk: pk_of(row, &table.schema),
                    detail: format!(
                        "referenced columns not found in table {}: ({})",
                        fk.parent_table,
                        fk.parent_columns.join(", ")
                    ),
                });
            }
            continue;
        };
        let parent_keys: Vec<Vec<VcValue>> = parent_rows
            .iter()
            .filter_map(|row| fk_key_of(row, &parent.columns, &fk.parent_columns))
            .collect();
        for row in &table.rows {
            let Some(key) = fk_key_of(row, &table.schema.columns, &fk.columns) else {
                continue;
            };
            if !parent_keys.contains(&key) {
                out.push(Violation {
                    table: table.name.clone(),
                    kind: ViolationKind::ForeignKey,
                    row_pk: pk_of(row, &table.schema),
                    detail: format!(
                        "foreign key ({}) -> {} ({})",
                        fk.columns.join(", "),
                        fk.parent_table,
                        fk.parent_columns.join(", ")
                    ),
                });
            }
        }
    }
}

/// The foreign-key cells of one row in FK column order, or `None` when any
/// cell is NULL or the row is missing a column (NULL FK values are not
/// violations in SQL).
fn fk_key_of(
    row: &VcRow,
    columns: &[crate::merge_schema::ColIR],
    fk_columns: &[String],
) -> Option<Vec<VcValue>> {
    let mut key = Vec::with_capacity(fk_columns.len());
    for name in fk_columns {
        let value = columns
            .iter()
            .position(|c| c.name == *name)
            .and_then(|i| row.values.get(i).cloned())?;
        if value == VcValue::Null {
            return None;
        }
        key.push(value);
    }
    Some(key)
}

/// Unique: two rows with the same values on a UNIQUE or PK column set.
fn check_unique(out: &mut Vec<Violation>, table: &MergedTable) {
    for unique in extract_unique_sets(&table.schema) {
        let Some(indices) = column_indices(&table.schema.columns, &unique) else {
            continue;
        };
        let mut seen: std::collections::HashSet<Vec<VcValue>> = std::collections::HashSet::new();
        for row in &table.rows {
            let key: Vec<VcValue> = indices
                .iter()
                .filter_map(|i| row.values.get(*i).cloned())
                .collect();
            if key.contains(&VcValue::Null) {
                // NULL keys never collide in SQL.
                continue;
            }
            if !seen.insert(key.clone()) {
                out.push(Violation {
                    table: table.name.clone(),
                    kind: ViolationKind::Unique,
                    row_pk: pk_of(row, &table.schema),
                    detail: format!("duplicate key on columns ({})", unique.join(", ")),
                });
            }
        }
    }
}

/// Check: a stored CHECK expression evaluating false for a row. An expression
/// the evaluator cannot understand is a violation too — never a silent pass.
fn check_checks(out: &mut Vec<Violation>, table: &MergedTable) {
    let columns: Vec<String> = table
        .schema
        .columns
        .iter()
        .map(|c| c.name.clone())
        .collect();
    for check in extract_checks(&table.schema) {
        for row in &table.rows {
            match eval_check(&check, row, &columns) {
                Ok(Some(false)) => out.push(Violation {
                    table: table.name.clone(),
                    kind: ViolationKind::Check,
                    row_pk: pk_of(row, &table.schema),
                    detail: format!("check constraint failed: {check}"),
                }),
                Ok(_) => {}
                Err(unsupported) => out.push(Violation {
                    table: table.name.clone(),
                    kind: ViolationKind::Check,
                    row_pk: pk_of(row, &table.schema),
                    detail: format!("check expression not supported: {unsupported}"),
                }),
            }
        }
    }
}

/// Not null: a NULL cell in a NOT NULL column.
fn check_not_null(out: &mut Vec<Violation>, table: &MergedTable) {
    for (i, col) in table.schema.columns.iter().enumerate() {
        if !col.notnull {
            continue;
        }
        for row in &table.rows {
            if row.values.get(i) == Some(&VcValue::Null) {
                out.push(Violation {
                    table: table.name.clone(),
                    kind: ViolationKind::NotNull,
                    row_pk: pk_of(row, &table.schema),
                    detail: format!("NULL in NOT NULL column {}", col.name),
                });
            }
        }
    }
}

/// Strict: a value whose type disagrees with a STRICT column's declared type.
fn check_strict(out: &mut Vec<Violation>, table: &MergedTable) {
    if !table.schema.strict {
        return;
    }
    for (i, col) in table.schema.columns.iter().enumerate() {
        let want = strict_type(&col.decl);
        if want == StrictType::Any {
            continue;
        }
        for row in &table.rows {
            let Some(value) = row.values.get(i) else {
                continue;
            };
            let bad = !matches!(
                (want, value),
                (StrictType::Any, _)
                    | (_, VcValue::Null)
                    | (StrictType::Integer, VcValue::Integer(_))
                    | (StrictType::Real, VcValue::Real(_))
                    | (StrictType::Text, VcValue::Text(_))
                    | (StrictType::Blob, VcValue::Blob(_))
            );
            if bad {
                out.push(Violation {
                    table: table.name.clone(),
                    kind: ViolationKind::Strict,
                    row_pk: pk_of(row, &table.schema),
                    detail: format!(
                        "type mismatch in STRICT column {}: {} does not match {}",
                        col.name,
                        value_type_name(value),
                        strict_type_name(&want)
                    ),
                });
            }
        }
    }
}

/// The pk cells of a row for reporting.
fn pk_of(row: &VcRow, schema: &SchemaIR) -> Vec<VcValue> {
    schema
        .pk
        .iter()
        .filter_map(|name| {
            schema
                .columns
                .iter()
                .position(|c| c.name == *name)
                .and_then(|i| row.values.get(i).cloned())
        })
        .collect()
}

fn column_indices(columns: &[crate::merge_schema::ColIR], names: &[String]) -> Option<Vec<usize>> {
    let indices: Vec<usize> = names
        .iter()
        .map(|name| columns.iter().position(|c| c.name == *name))
        .collect::<Option<Vec<_>>>()?;
    Some(indices)
}

// ============================================================================
// Constraint extraction from a SchemaIR
// ============================================================================

struct ForeignKeyDef {
    columns: Vec<String>,
    parent_table: String,
    parent_columns: Vec<String>,
}

/// Parse every `FOREIGN KEY` (table-level and column-level) from a schema.
fn extract_foreign_keys(schema: &SchemaIR) -> Vec<ForeignKeyDef> {
    let mut out = Vec::new();
    for constraint in &schema.constraints {
        if let Some(fk) = parse_table_fk(constraint) {
            out.push(fk);
        }
    }
    for col in &schema.columns {
        if let Some(parent) = parse_column_fk(&col.decl) {
            out.push(ForeignKeyDef {
                columns: vec![col.name.clone()],
                parent_table: parent.0,
                parent_columns: parent.1,
            });
        }
    }
    out
}

/// `FOREIGN KEY (a, b) REFERENCES parent (x, y)`.
fn parse_table_fk(constraint: &str) -> Option<ForeignKeyDef> {
    let tokens = tokenize(constraint);
    let mut i = 0;
    if !next_is(&tokens, &mut i, "FOREIGN") || !next_is(&tokens, &mut i, "KEY") {
        return None;
    }
    let columns = read_paren_list(&tokens, &mut i)?;
    if !next_is(&tokens, &mut i, "REFERENCES") {
        return None;
    }
    let parent_table = unquote_ident(tokens.get(i)?.clone());
    i += 1;
    let parent_columns = if tokens.get(i) == Some(&"(".to_string()) {
        read_paren_list(&tokens, &mut i)?
    } else {
        columns.clone()
    };
    Some(ForeignKeyDef {
        columns,
        parent_table,
        parent_columns,
    })
}

/// Column-level `REFERENCES parent (x, y)` inside a decl.
fn parse_column_fk(decl: &str) -> Option<(String, Vec<String>)> {
    let tokens = tokenize(decl);
    let pos = tokens
        .iter()
        .position(|t| t.eq_ignore_ascii_case("REFERENCES"))?;
    let parent_table = unquote_ident(tokens.get(pos + 1)?.clone());
    let mut i = pos + 2;
    let parent_columns = if tokens.get(i) == Some(&"(".to_string()) {
        read_paren_list(&tokens, &mut i)?
    } else {
        Vec::new()
    };
    Some((parent_table, parent_columns))
}

/// Every UNIQUE column set: the primary key, column-level UNIQUEs, and
/// table-level UNIQUE clauses.
fn extract_unique_sets(schema: &SchemaIR) -> Vec<Vec<String>> {
    let mut out = Vec::new();
    if !schema.pk.is_empty() {
        out.push(schema.pk.clone());
    }
    for col in &schema.columns {
        if col
            .decl
            .split_whitespace()
            .any(|w| w.eq_ignore_ascii_case("UNIQUE"))
        {
            out.push(vec![col.name.clone()]);
        }
    }
    for constraint in &schema.constraints {
        if let Some(columns) = parse_unique_list(constraint) {
            out.push(columns);
        }
    }
    out
}

/// `UNIQUE (a, b)` → the ordered list.
fn parse_unique_list(constraint: &str) -> Option<Vec<String>> {
    let tokens = tokenize(constraint);
    let mut i = 0;
    if !next_is(&tokens, &mut i, "UNIQUE") {
        return None;
    }
    read_paren_list(&tokens, &mut i)
}

/// Every CHECK expression, column-level first then table-level.
fn extract_checks(schema: &SchemaIR) -> Vec<String> {
    let mut out = Vec::new();
    for col in &schema.columns {
        for check in find_check_exprs(&col.decl) {
            out.push(check);
        }
    }
    for constraint in &schema.constraints {
        for check in find_check_exprs(constraint) {
            out.push(check);
        }
    }
    out
}

/// Pull every `CHECK (expr)` group out of a decl or constraint text.
fn find_check_exprs(text: &str) -> Vec<String> {
    let tokens = tokenize(text);
    let mut out = Vec::new();
    let mut i = 0;
    while i < tokens.len() {
        if tokens[i].eq_ignore_ascii_case("CHECK") && tokens.get(i + 1) == Some(&"(".to_string()) {
            if let Some(expr) = read_paren_list_at(&tokens, &mut (i + 1)) {
                out.push(expr.join(" "));
                i += 1;
                continue;
            }
        }
        i += 1;
    }
    out
}

// ============================================================================
// Check-expression evaluator
// ============================================================================

/// Evaluate a CHECK expression against one row.
///
/// Supported: `=, !=, <, <=, >, >=, IS NULL, IS NOT NULL, LIKE, IN` plus
/// `AND`/`OR`/`NOT` and parentheses, over local columns and literals. `Ok(None)`
/// is SQL's unknown (NULL) result, which satisfies a CHECK. Anything else is
/// `Err` with the expression text so the caller can report it as unsupported
/// rather than silently pass.
pub fn eval_check(expr: &str, row: &VcRow, columns: &[String]) -> Result<Option<bool>, String> {
    let tokens = tokenize(expr);
    if tokens.is_empty() {
        return Err(expr.to_string());
    }
    let mut parser = CheckParser {
        tokens: &tokens,
        pos: 0,
        row,
        columns,
    };
    match parser.parse_or() {
        Ok(value) => Ok(value),
        Err(()) => Err(expr.to_string()),
    }
}

struct CheckParser<'a> {
    tokens: &'a [String],
    pos: usize,
    row: &'a VcRow,
    columns: &'a [String],
}

impl CheckParser<'_> {
    fn parse_or(&mut self) -> Result<Option<bool>, ()> {
        let mut left = self.parse_and()?;
        while self.peek_is("OR") {
            self.pos += 1;
            let right = self.parse_and()?;
            left = bool_or(left, right);
        }
        Ok(left)
    }

    fn parse_and(&mut self) -> Result<Option<bool>, ()> {
        let mut left = self.parse_not()?;
        while self.peek_is("AND") {
            self.pos += 1;
            let right = self.parse_not()?;
            left = bool_and(left, right);
        }
        Ok(left)
    }

    fn parse_not(&mut self) -> Result<Option<bool>, ()> {
        if self.peek_is("NOT") {
            self.pos += 1;
            let inner = self.parse_not()?;
            return Ok(inner.map(|b| !b));
        }
        self.parse_primary()
    }

    fn parse_primary(&mut self) -> Result<Option<bool>, ()> {
        if self.peek_is("(") {
            self.pos += 1;
            let inner = self.parse_or()?;
            if !self.peek_is(")") {
                return Err(());
            }
            self.pos += 1;
            return Ok(inner);
        }
        let left = self.parse_value()?;
        if self.pos >= self.tokens.len() {
            return Err(());
        }
        let op = self.tokens[self.pos].to_uppercase();
        match op.as_str() {
            "=" | "==" | "!=" | "<>" | "<" | "<=" | ">" | ">=" => {
                self.pos += 1;
                let right = self.parse_value()?;
                let a = self.value_of(&left)?;
                let b = self.value_of(&right)?;
                Ok(compare_values(&op, &a, &b))
            }
            "IS" => {
                self.pos += 1;
                let negate = self.peek_is("NOT");
                if negate {
                    self.pos += 1;
                }
                if !self.peek_is("NULL") {
                    return Err(());
                }
                self.pos += 1;
                let left_value = self.value_of(&left)?;
                let is_null = left_value == VcValue::Null;
                Ok(Some(if negate { !is_null } else { is_null }))
            }
            "LIKE" => {
                self.pos += 1;
                let right = self.parse_value()?;
                let left_value = self.value_of(&left)?;
                let right_value = self.value_of(&right)?;
                like_values(&left_value, &right_value)
            }
            "NOT" => {
                self.pos += 1;
                if self.peek_is("LIKE") {
                    self.pos += 1;
                    let right = self.parse_value()?;
                    let left_value = self.value_of(&left)?;
                    let right_value = self.value_of(&right)?;
                    like_values(&left_value, &right_value).map(|inner| inner.map(|b| !b))
                } else if self.peek_is("IN") {
                    self.pos += 1;
                    self.parse_in(&left).map(|inner| inner.map(|b| !b))
                } else {
                    Err(())
                }
            }
            "IN" => {
                self.pos += 1;
                self.parse_in(&left)
            }
            _ => Err(()),
        }
    }

    fn parse_in(&mut self, left: &ValueRef) -> Result<Option<bool>, ()> {
        // The caller consumed `IN`; `NOT IN` is handled by `parse_not`.
        if !self.peek_is("(") {
            return Err(());
        }
        self.pos += 1;
        let mut members = Vec::new();
        loop {
            members.push(self.parse_value()?);
            if self.peek_is(",") {
                self.pos += 1;
                continue;
            }
            break;
        }
        if !self.peek_is(")") {
            return Err(());
        }
        self.pos += 1;
        let left_value = self.value_of(left)?;
        // A NULL probe makes the whole `IN` unknown in SQL, never true.
        if left_value == VcValue::Null {
            return Ok(None);
        }
        let mut found = false;
        for member in &members {
            if self.value_of(member)? == left_value {
                found = true;
            }
        }
        Ok(Some(found))
    }

    fn parse_value(&mut self) -> Result<ValueRef, ()> {
        let Some(tok) = self.tokens.get(self.pos) else {
            return Err(());
        };
        self.pos += 1;
        if tok.eq_ignore_ascii_case("NULL") {
            return Ok(ValueRef::Null);
        }
        if is_number(tok) {
            return Ok(ValueRef::Literal(VcValue::Integer(
                tok.parse::<i64>().map_err(|_| ())?,
            )));
        }
        if is_string(tok) {
            let inner = tok.trim_matches(|c| c == '\'' || c == '"');
            return Ok(ValueRef::Literal(VcValue::Text(inner.to_string())));
        }
        // Bare identifier: a column of the row.
        Ok(ValueRef::Column(tok.clone()))
    }

    fn value_of(&self, value: &ValueRef) -> Result<VcValue, ()> {
        match value {
            ValueRef::Null => Ok(VcValue::Null),
            ValueRef::Literal(v) => Ok(v.clone()),
            ValueRef::Column(name) => self
                .columns
                .iter()
                .position(|c| c == name)
                .and_then(|i| self.row.values.get(i).cloned())
                // A CHECK naming a column the table does not have is not
                // silently-NULL: it is an expression this evaluator cannot
                // judge, so the caller reports it as unsupported.
                .ok_or(()),
        }
    }

    fn peek_is(&self, tok: &str) -> bool {
        self.tokens
            .get(self.pos)
            .is_some_and(|t| t.eq_ignore_ascii_case(tok))
    }
}

#[derive(Debug, Clone)]
enum ValueRef {
    Null,
    Literal(VcValue),
    Column(String),
}

fn bool_and(a: Option<bool>, b: Option<bool>) -> Option<bool> {
    match (a, b) {
        (Some(false), _) | (_, Some(false)) => Some(false),
        (Some(true), Some(true)) => Some(true),
        _ => None,
    }
}

fn bool_or(a: Option<bool>, b: Option<bool>) -> Option<bool> {
    match (a, b) {
        (Some(true), _) | (_, Some(true)) => Some(true),
        (Some(false), Some(false)) => Some(false),
        _ => None,
    }
}

fn compare_values(op: &str, a: &VcValue, b: &VcValue) -> Option<bool> {
    if *a == VcValue::Null || *b == VcValue::Null {
        return None;
    }
    Some(match op {
        "=" | "==" => a == b,
        "!=" | "<>" => a != b,
        "<" => a < b,
        "<=" => a <= b,
        ">" => a > b,
        ">=" => a >= b,
        _ => return None,
    })
}

fn like_values(left: &VcValue, right: &VcValue) -> Result<Option<bool>, ()> {
    let VcValue::Text(pattern_text) = right else {
        return Err(());
    };
    let VcValue::Text(text) = left else {
        return Ok(None);
    };
    Ok(Some(simple_like(text, pattern_text)))
}

/// Minimal `LIKE` matcher: `%` matches any run, `_` matches one character.
fn simple_like(text: &str, pattern: &str) -> bool {
    let text: Vec<char> = text.chars().collect();
    let pattern: Vec<char> = pattern.chars().collect();
    let mut memo = std::collections::HashMap::new();
    fn walk(
        t: &[char],
        p: &[char],
        ti: usize,
        pi: usize,
        memo: &mut std::collections::HashMap<(usize, usize), bool>,
    ) -> bool {
        if let Some(&r) = memo.get(&(ti, pi)) {
            return r;
        }
        let r = if pi == p.len() {
            ti == t.len()
        } else if p[pi] == '%' {
            walk(t, p, ti, pi + 1, memo) || (ti < t.len() && walk(t, p, ti + 1, pi, memo))
        } else if p[pi] == '_' {
            ti < t.len() && walk(t, p, ti + 1, pi + 1, memo)
        } else {
            ti < t.len() && t[ti] == p[pi] && walk(t, p, ti + 1, pi + 1, memo)
        };
        memo.insert((ti, pi), r);
        r
    }
    walk(&text, &pattern, 0, 0, &mut memo)
}

// ============================================================================
// Token helpers shared with the schema parser shape
// ============================================================================

fn is_number(tok: &str) -> bool {
    tok.parse::<i64>().is_ok()
}

fn is_string(tok: &str) -> bool {
    (tok.starts_with('\'') && tok.ends_with('\'')) || (tok.starts_with('"') && tok.ends_with('"'))
}

fn next_is(tokens: &[String], i: &mut usize, tok: &str) -> bool {
    if tokens.get(*i).is_some_and(|t| t.eq_ignore_ascii_case(tok)) {
        *i += 1;
        true
    } else {
        false
    }
}

/// Read `(a, b, c)` starting at the token after `(`.
fn read_paren_list_at(tokens: &[String], i: &mut usize) -> Option<Vec<String>> {
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
    Some(names)
}

/// Strip one layer of SQL identifier quoting, matching `merge_schema` so FK
/// and unique column lists line up with parsed schema column names.
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

/// `PRIMARY KEY (...)`-style helper: read `(...)` when the caller has already
/// consumed `UNIQUE`/`FOREIGN KEY`.
fn read_paren_list(tokens: &[String], i: &mut usize) -> Option<Vec<String>> {
    read_paren_list_at(tokens, i)
}

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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StrictType {
    Any,
    Integer,
    Real,
    Text,
    Blob,
}

fn strict_type(decl: &str) -> StrictType {
    let ty = decl.split_whitespace().next().unwrap_or("").to_uppercase();
    match ty.as_str() {
        "INT" | "INTEGER" | "BIGINT" | "SMALLINT" | "TINYINT" | "MEDIUMINT" | "INT2" | "INT8" => {
            StrictType::Integer
        }
        "REAL" | "FLOAT" | "DOUBLE" => StrictType::Real,
        "TEXT" | "VARCHAR" | "CHAR" | "CLOB" => StrictType::Text,
        "BLOB" => StrictType::Blob,
        _ => StrictType::Any,
    }
}

fn strict_type_name(ty: &StrictType) -> &'static str {
    match ty {
        StrictType::Integer => "INTEGER",
        StrictType::Real => "REAL",
        StrictType::Text => "TEXT",
        StrictType::Blob => "BLOB",
        StrictType::Any => "ANY",
    }
}

fn value_type_name(value: &VcValue) -> &'static str {
    match value {
        VcValue::Null => "NULL",
        VcValue::Integer(_) => "INTEGER",
        VcValue::Real(_) => "REAL",
        VcValue::Text(_) => "TEXT",
        VcValue::Blob(_) => "BLOB",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::merge_schema::parse_schema;

    fn schema(sql: &str) -> SchemaIR {
        parse_schema(sql).unwrap()
    }

    fn table(name: &str, schema_sql: &str, rows: Vec<VcRow>) -> MergedTable {
        MergedTable {
            name: name.to_string(),
            schema: schema(&sql_string(name, schema_sql)),
            rows,
        }
    }

    fn sql_string(name: &str, body: &str) -> String {
        format!("CREATE TABLE {name} {body}")
    }

    #[test]
    fn unique_detects_duplicate_single_column() {
        let t = table(
            "t",
            "(id INTEGER PRIMARY KEY, v TEXT UNIQUE)",
            vec![
                VcRow::new(vec![VcValue::Integer(1), VcValue::Text("a".into())]),
                VcRow::new(vec![VcValue::Integer(2), VcValue::Text("a".into())]),
            ],
        );
        let work = MergedWork { tables: vec![t] };
        let schemas: Vec<SchemaIR> = work.tables.iter().map(|t| t.schema.clone()).collect();
        let violations = verify_constraints(&work, &schemas);
        assert_eq!(violations.len(), 1);
        assert_eq!(violations[0].kind, ViolationKind::Unique);
        assert_eq!(violations[0].row_pk, vec![VcValue::Integer(2)]);
    }

    #[test]
    fn unique_detects_duplicate_multi_column() {
        let t = table(
            "t",
            "(a TEXT, b TEXT, PRIMARY KEY (a, b))",
            vec![
                VcRow::new(vec![VcValue::Text("x".into()), VcValue::Text("y".into())]),
                VcRow::new(vec![VcValue::Text("x".into()), VcValue::Text("y".into())]),
            ],
        );
        let work = MergedWork { tables: vec![t] };
        let schemas: Vec<SchemaIR> = work.tables.iter().map(|t| t.schema.clone()).collect();
        let violations = verify_constraints(&work, &schemas);
        assert_eq!(violations.len(), 1);
        assert_eq!(violations[0].kind, ViolationKind::Unique);
    }

    #[test]
    fn not_null_detects_null_in_notnull_column() {
        let t = table(
            "t",
            "(id INTEGER PRIMARY KEY, v TEXT NOT NULL)",
            vec![VcRow::new(vec![VcValue::Integer(1), VcValue::Null])],
        );
        let work = MergedWork { tables: vec![t] };
        let schemas: Vec<SchemaIR> = work.tables.iter().map(|t| t.schema.clone()).collect();
        let violations = verify_constraints(&work, &schemas);
        assert_eq!(violations.len(), 1);
        assert_eq!(violations[0].kind, ViolationKind::NotNull);
        assert_eq!(violations[0].detail, "NULL in NOT NULL column v");
    }

    #[test]
    fn check_detects_false_row() {
        let t = table(
            "t",
            "(id INTEGER PRIMARY KEY, v TEXT, CHECK (v <> ''))",
            vec![VcRow::new(vec![
                VcValue::Integer(1),
                VcValue::Text("".into()),
            ])],
        );
        let work = MergedWork { tables: vec![t] };
        let schemas: Vec<SchemaIR> = work.tables.iter().map(|t| t.schema.clone()).collect();
        let violations = verify_constraints(&work, &schemas);
        assert_eq!(violations.len(), 1);
        assert_eq!(violations[0].kind, ViolationKind::Check);
    }

    #[test]
    fn check_null_result_passes() {
        let t = table(
            "t",
            "(id INTEGER PRIMARY KEY, v TEXT, CHECK (v <> ''))",
            vec![VcRow::new(vec![VcValue::Integer(1), VcValue::Null])],
        );
        let work = MergedWork { tables: vec![t] };
        let schemas: Vec<SchemaIR> = work.tables.iter().map(|t| t.schema.clone()).collect();
        assert!(verify_constraints(&work, &schemas).is_empty());
    }

    #[test]
    fn check_unsupported_expression_is_violation_not_silent_pass() {
        let t = table(
            "t",
            "(id INTEGER PRIMARY KEY, v TEXT, CHECK (v GLOB '*'))",
            vec![VcRow::new(vec![
                VcValue::Integer(1),
                VcValue::Text("x".into()),
            ])],
        );
        let work = MergedWork { tables: vec![t] };
        let schemas: Vec<SchemaIR> = work.tables.iter().map(|t| t.schema.clone()).collect();
        let violations = verify_constraints(&work, &schemas);
        assert_eq!(violations.len(), 1);
        assert_eq!(violations[0].kind, ViolationKind::Check);
        assert!(
            violations[0]
                .detail
                .contains("check expression not supported: v GLOB '*'"),
            "{}",
            violations[0].detail
        );
    }

    #[test]
    fn strict_rejects_text_into_integer_column() {
        let t = table(
            "t",
            "(id INTEGER PRIMARY KEY, v INTEGER) STRICT",
            vec![VcRow::new(vec![
                VcValue::Integer(1),
                VcValue::Text("x".into()),
            ])],
        );
        let work = MergedWork { tables: vec![t] };
        let schemas: Vec<SchemaIR> = work.tables.iter().map(|t| t.schema.clone()).collect();
        let violations = verify_constraints(&work, &schemas);
        assert_eq!(violations.len(), 1);
        assert_eq!(violations[0].kind, ViolationKind::Strict);
        assert!(violations[0].detail.contains("STRICT column v"));
    }

    #[test]
    fn strict_allows_matching_types() {
        let t = table(
            "t",
            "(id INTEGER PRIMARY KEY, v INTEGER) STRICT",
            vec![VcRow::new(vec![VcValue::Integer(1), VcValue::Integer(7)])],
        );
        let work = MergedWork { tables: vec![t] };
        let schemas: Vec<SchemaIR> = work.tables.iter().map(|t| t.schema.clone()).collect();
        assert!(verify_constraints(&work, &schemas).is_empty());
    }

    #[test]
    fn foreign_key_missing_parent_key_detected() {
        let child = table(
            "child",
            "(id INTEGER PRIMARY KEY, parent_id INTEGER REFERENCES parent (id))",
            vec![VcRow::new(vec![VcValue::Integer(1), VcValue::Integer(99)])],
        );
        let parent = table(
            "parent",
            "(id INTEGER PRIMARY KEY)",
            vec![VcRow::new(vec![VcValue::Integer(1)])],
        );
        let work = MergedWork {
            tables: vec![child, parent],
        };
        let schemas: Vec<SchemaIR> = work.tables.iter().map(|t| t.schema.clone()).collect();
        let violations = verify_constraints(&work, &schemas);
        assert_eq!(violations.len(), 1);
        assert_eq!(violations[0].kind, ViolationKind::ForeignKey);
    }

    #[test]
    fn foreign_key_matching_parent_key_passes() {
        let child = table(
            "child",
            "(id INTEGER PRIMARY KEY, parent_id INTEGER REFERENCES parent (id))",
            vec![VcRow::new(vec![VcValue::Integer(1), VcValue::Integer(1)])],
        );
        let parent = table(
            "parent",
            "(id INTEGER PRIMARY KEY)",
            vec![VcRow::new(vec![VcValue::Integer(1)])],
        );
        let work = MergedWork {
            tables: vec![child, parent],
        };
        let schemas: Vec<SchemaIR> = work.tables.iter().map(|t| t.schema.clone()).collect();
        assert!(verify_constraints(&work, &schemas).is_empty());
    }

    #[test]
    fn foreign_key_missing_parent_table_is_violation() {
        let child = table(
            "child",
            "(id INTEGER PRIMARY KEY, parent_id INTEGER REFERENCES missing (id))",
            vec![VcRow::new(vec![VcValue::Integer(1), VcValue::Integer(99)])],
        );
        let work = MergedWork {
            tables: vec![child],
        };
        let schemas: Vec<SchemaIR> = work.tables.iter().map(|t| t.schema.clone()).collect();
        let violations = verify_constraints(&work, &schemas);
        assert_eq!(violations.len(), 1);
        assert_eq!(violations[0].kind, ViolationKind::ForeignKey);
        assert!(violations[0].detail.contains("-> missing (id)"));
    }

    #[test]
    fn foreign_key_missing_column_indices_is_violation() {
        let child = table(
            "child",
            "(id INTEGER PRIMARY KEY, parent_id INTEGER REFERENCES parent (id))",
            vec![VcRow::new(vec![VcValue::Integer(1), VcValue::Integer(99)])],
        );
        // The referenced column is absent from the parent schema.
        let parent = table(
            "parent",
            "(other INTEGER)",
            vec![VcRow::new(vec![VcValue::Integer(1)])],
        );
        let work = MergedWork {
            tables: vec![child, parent],
        };
        let schemas: Vec<SchemaIR> = work.tables.iter().map(|t| t.schema.clone()).collect();
        let violations = verify_constraints(&work, &schemas);
        assert_eq!(violations.len(), 1);
        assert_eq!(violations[0].kind, ViolationKind::ForeignKey);
    }

    #[test]
    fn check_unknown_column_is_unsupported_violation() {
        let t = table(
            "t",
            "(id INTEGER PRIMARY KEY, v TEXT, CHECK (ghost > 0))",
            vec![VcRow::new(vec![
                VcValue::Integer(1),
                VcValue::Text("x".into()),
            ])],
        );
        let work = MergedWork { tables: vec![t] };
        let schemas: Vec<SchemaIR> = work.tables.iter().map(|t| t.schema.clone()).collect();
        let violations = verify_constraints(&work, &schemas);
        assert_eq!(violations.len(), 1);
        assert!(
            violations[0]
                .detail
                .contains("check expression not supported: ghost > 0"),
            "{}",
            violations[0].detail
        );
    }

    #[test]
    fn check_null_in_list_is_unknown_not_true() {
        let row = VcRow::new(vec![VcValue::Null, VcValue::Text("abc".into())]);
        let cols = vec!["id".to_string(), "v".to_string()];
        assert_eq!(eval_check("id IN (1, 2, NULL)", &row, &cols).unwrap(), None);
        assert_eq!(
            eval_check("id NOT IN (1, 2, NULL)", &row, &cols).unwrap(),
            None
        );
    }

    #[test]
    fn check_null_like_is_unknown() {
        let row = VcRow::new(vec![VcValue::Null, VcValue::Null]);
        let cols = vec!["id".to_string(), "v".to_string()];
        assert_eq!(eval_check("v LIKE '%'", &row, &cols).unwrap(), None);
    }

    #[test]
    fn eval_check_comparisons() {
        let row = VcRow::new(vec![VcValue::Integer(5), VcValue::Text("abc".into())]);
        let cols = vec!["id".to_string(), "v".to_string()];
        assert_eq!(eval_check("id = 5", &row, &cols).unwrap(), Some(true));
        assert_eq!(eval_check("id != 6", &row, &cols).unwrap(), Some(true));
        assert_eq!(eval_check("id < 6", &row, &cols).unwrap(), Some(true));
        assert_eq!(eval_check("id >= 5", &row, &cols).unwrap(), Some(true));
        assert_eq!(
            eval_check("v IS NOT NULL", &row, &cols).unwrap(),
            Some(true)
        );
        assert_eq!(eval_check("v IS NULL", &row, &cols).unwrap(), Some(false));
        assert_eq!(eval_check("v LIKE 'a%'", &row, &cols).unwrap(), Some(true));
        assert_eq!(
            eval_check("id IN (1, 2, 5)", &row, &cols).unwrap(),
            Some(true)
        );
        assert_eq!(
            eval_check("id NOT IN (1, 2)", &row, &cols).unwrap(),
            Some(true)
        );
    }

    #[test]
    fn eval_check_and_or_and_parentheses() {
        let row = VcRow::new(vec![VcValue::Integer(5), VcValue::Text("abc".into())]);
        let cols = vec!["id".to_string(), "v".to_string()];
        assert_eq!(
            eval_check("id > 0 AND v IS NOT NULL", &row, &cols).unwrap(),
            Some(true)
        );
        assert_eq!(
            eval_check("(id > 0 OR id < 0) AND v <> 'x'", &row, &cols).unwrap(),
            Some(true)
        );
    }

    #[test]
    fn violation_type_display_is_dolt_exact() {
        assert_eq!(ViolationKind::ForeignKey.to_string(), "foreign key");
        assert_eq!(ViolationKind::Unique.to_string(), "unique");
        assert_eq!(ViolationKind::Check.to_string(), "check");
        assert_eq!(ViolationKind::NotNull.to_string(), "not null");
        assert_eq!(ViolationKind::Strict.to_string(), "strict");
    }

    #[test]
    fn parse_table_level_unique_and_check() {
        let s = schema("CREATE TABLE t (a TEXT, b TEXT, UNIQUE (a, b), CHECK (a <> ''))");
        assert_eq!(extract_unique_sets(&s).len(), 1);
        assert_eq!(extract_checks(&s).len(), 1);
    }
}
