//! A `.sql` file's database: the Schema tab under its text, and what the
//! editor offers as it is typed.

use std::{cell::RefCell, collections::{HashMap, HashSet}, rc::Weak as RcWeak};

use gpui_kit::component::input::{Input, InputState};

use super::*;
use crate::db::{
    schema::{self, Schema, SuggestionKind},
    statement,
};

/// The schema a file's session read; the editor's completions look at it.
pub(super) type SharedSchema = Rc<RefCell<Option<Arc<Schema>>>>;

thread_local! {
    /// By file: the provider is made before the file's view, and again when
    /// language servers stop, and both have to find the same schema.
    static SCHEMAS: RefCell<HashMap<PathBuf, RcWeak<RefCell<Option<Arc<Schema>>>>>> = RefCell::default();
}

pub(super) fn shared_schema(path: &std::path::Path) -> SharedSchema {
    SCHEMAS.with(|all| {
        let mut all = all.borrow_mut();
        all.retain(|_, schema| schema.strong_count() > 0);
        if let Some(schema) = all.get(path).and_then(RcWeak::upgrade) {
            return schema;
        }
        let schema: SharedSchema = Rc::default();
        all.insert(path.to_path_buf(), Rc::downgrade(&schema));
        schema
    })
}

/// What a `.sql` file's editor completes with; none for any other file.
pub(super) fn completions(path: &std::path::Path) -> Option<Rc<dyn CompletionProvider>> {
    super::sql::is_sql(path).then(|| Rc::new(SqlCompletions { schema: shared_schema(path) }) as Rc<dyn CompletionProvider>)
}

struct SqlCompletions {
    schema: SharedSchema,
}

/// The items for the statement around `offset`.
fn items(schema: &Schema, text: &Rope, offset: usize) -> Vec<lsp_types::CompletionItem> {
    use lsp_types::{CompletionItem, CompletionItemKind, CompletionTextEdit, Range, TextEdit};
    let whole = text.to_string();
    let Some(head) = whole.get(..offset) else { return Vec::new() };
    let statements = statement::split(&whole);
    let around = statements.iter().find(|s| s.start <= offset && offset <= s.end).cloned().unwrap_or(offset..offset);
    let before = &whole[around.start..offset];
    let statement_text = &whole[around.clone()];
    let typed = head.chars().rev().take_while(|c| c.is_alphanumeric() || *c == '_' || *c == '$').map(char::len_utf8).sum::<usize>();
    let edit_range = Range { start: text.offset_to_position(offset - typed), end: text.offset_to_position(offset) };
    schema::suggest(schema, before, statement_text)
        .into_iter()
        .enumerate()
        .map(|(ix, s)| CompletionItem {
            kind: Some(match s.kind {
                SuggestionKind::Join => CompletionItemKind::REFERENCE,
                SuggestionKind::Function => CompletionItemKind::FUNCTION,
                SuggestionKind::Column => CompletionItemKind::FIELD,
                SuggestionKind::Table => CompletionItemKind::CLASS,
                SuggestionKind::Keyword => CompletionItemKind::KEYWORD,
            }),
            detail: Some(s.detail),
            sort_text: Some(format!("{ix:05}")),
            filter_text: Some(s.text.clone()),
            text_edit: Some(CompletionTextEdit::Edit(TextEdit { range: edit_range, new_text: s.text.clone() })),
            label: s.text,
            ..Default::default()
        })
        .collect()
}

impl CompletionProvider for SqlCompletions {
    fn completions(&self, text: &Rope, offset: usize, _: lsp_types::CompletionContext, _: &mut Window, cx: &mut App) -> Task<anyhow::Result<lsp_types::CompletionResponse>> {
        let Some(schema) = self.schema.borrow().clone() else {
            return Task::ready(Ok(lsp_types::CompletionResponse::Array(Vec::new())));
        };
        let text = text.clone();
        cx.background_spawn(async move { Ok(lsp_types::CompletionResponse::Array(items(&schema, &text, offset))) })
    }

    fn is_completion_trigger(&self, _: usize, new_text: &str, _: &mut App) -> bool {
        let Some(last) = new_text.chars().last() else { return false };
        self.schema.borrow().is_some() && crate::state::with_settings(|s| s.complete_as_typed) && (last.is_alphanumeric() || last == '_' || last == '.')
    }
}

/// A line of the tree.
#[derive(Clone)]
pub(super) enum SchemaRow {
    Table { name: SharedString, view: bool, columns: usize, open: bool },
    Column { name: SharedString, kind: SharedString, key: SharedString, nullable: bool },
    /// An index or a foreign key under its table, or a routine at the end.
    Detail { label: SharedString, about: SharedString, nested: bool },
}

/// The tree as it shows: what the filter leaves, the open tables' columns under them.
pub(super) fn schema_rows(schema: &Schema, open: &HashSet<String>, filter: &str) -> Vec<SchemaRow> {
    let filter = filter.trim().to_lowercase();
    let mut rows = Vec::new();
    for table in &schema.tables {
        let by_name = filter.is_empty() || table.name.to_lowercase().contains(&filter);
        let columns: Vec<_> = table.columns.iter().filter(|c| by_name || c.name.to_lowercase().contains(&filter)).collect();
        if !by_name && columns.is_empty() {
            continue;
        }
        // Found by a column: shown with it, or the filter would seem to have found nothing.
        let is_open = open.contains(&table.name) || !by_name;
        rows.push(SchemaRow::Table { name: table.name.clone().into(), view: table.view, columns: table.columns.len(), open: is_open });
        if is_open {
            rows.extend(columns.into_iter().map(|c| SchemaRow::Column { name: c.name.clone().into(), kind: c.kind.clone().into(), key: c.key.clone().into(), nullable: c.nullable }));
            for index in schema.indexes.iter().filter(|i| i.table == table.name) {
                let about = format!("{}({})", if index.unique { "unique index " } else { "index " }, index.columns.join(", "));
                rows.push(SchemaRow::Detail { label: index.name.clone().into(), about: about.into(), nested: true });
            }
            for key in schema.foreign_keys.iter().filter(|k| k.table == table.name) {
                rows.push(SchemaRow::Detail { label: key.column.clone().into(), about: format!("→ {}.{}", key.to_table, key.to_column).into(), nested: true });
            }
            for key in schema.foreign_keys.iter().filter(|k| k.to_table == table.name) {
                rows.push(SchemaRow::Detail { label: key.to_column.clone().into(), about: format!("← {}.{}", key.table, key.column).into(), nested: true });
            }
        }
    }
    for routine in schema.routines.iter().filter(|r| filter.is_empty() || r.name.to_lowercase().contains(&filter)) {
        rows.push(SchemaRow::Detail { label: routine.name.clone().into(), about: routine.kind.clone().into(), nested: false });
    }
    rows
}

impl FileView {
    /// The tree again, after the schema, the filter or an open table changed.
    pub(super) fn refresh_schema_rows(&mut self, cx: &mut Context<Self>) {
        let Some(sql) = &mut self.sql else { return };
        let filter = sql.schema_filter.as_ref().map(|f| f.read(cx).value().to_string()).unwrap_or_default();
        let schema = sql.schema.borrow().clone();
        sql.schema_rows = Rc::new(schema.map(|s| schema_rows(&s, &sql.schema_open, &filter)).unwrap_or_default());
        cx.notify();
    }

    fn toggle_schema_table(&mut self, name: &str, cx: &mut Context<Self>) {
        let Some(sql) = &mut self.sql else { return };
        if !sql.schema_open.remove(name) {
            sql.schema_open.insert(name.to_string());
        }
        self.refresh_schema_rows(cx);
    }

    /// What shows a table's rows, or a Redis key's value by its type.
    pub(super) fn table_data_statement(&self, name: &str) -> String {
        use crate::db::config::{self, Driver};
        let sql = self.sql.as_ref();
        let driver = sql.and_then(|s| s.connection_id()).and_then(|id| config::find(id)).map_or(Driver::MySql, |c| c.driver);
        if driver != Driver::Redis {
            return format!("SELECT * FROM {} LIMIT 500", driver.quoted(name));
        }
        let kind = sql.and_then(|s| s.schema.borrow().as_ref().and_then(|schema| schema.table(name).and_then(|t| t.columns.first().map(|c| c.kind.clone())))).unwrap_or_default();
        let key = format!("\"{}\"", name.replace('\\', "\\\\").replace('"', "\\\""));
        match kind.as_str() {
            "hash" => format!("HGETALL {key}"),
            "list" => format!("LRANGE {key} 0 499"),
            "set" => format!("SSCAN {key} 0 COUNT 500"),
            "zset" => format!("ZRANGE {key} 0 499 WITHSCORES"),
            "stream" => format!("XRANGE {key} - + COUNT 500"),
            _ => format!("GET {key}"),
        }
    }

    /// The filter's input, made when the tab is first shown.
    pub(super) fn ensure_schema_filter(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.sql.as_ref().is_none_or(|s| s.schema_filter.is_some()) {
            return;
        }
        let input = cx.new(|cx| InputState::new(window, cx).placeholder("Filter tables and columns"));
        cx.subscribe(&input, |this: &mut FileView, _, event: &InputEvent, cx| {
            if matches!(event, InputEvent::Change) {
                this.refresh_schema_rows(cx);
            }
        })
        .detach();
        if let Some(sql) = &mut self.sql {
            sql.schema_filter = Some(input);
        }
    }

    pub(super) fn render_sql_schema(&self, cx: &mut Context<Self>) -> AnyElement {
        let Some(sql) = &self.sql else { return div().into_any_element() };
        let muted = cx.theme().muted_foreground;
        let rows = sql.schema_rows.clone();
        let count = rows.len();
        let this = cx.weak_entity();
        let importable = sql.connection_id().and_then(crate::db::config::find).is_some_and(|c| c.driver != crate::db::config::Driver::Redis);
        let list = uniform_list("sql-schema-list", count, move |range, _, cx| {
            let muted = cx.theme().muted_foreground;
            range
                .map(|ix| match &rows[ix] {
                    SchemaRow::Table { name, view, columns, open } => {
                        let importable = importable && !*view;
                        let (toggle, data, ddl, copy) = (this.clone(), this.clone(), this.clone(), name.clone());
                        let (toggled, for_data, for_ddl) = (name.clone(), name.clone(), name.clone());
                        h_flex()
                            .id(("sql-schema-row", ix))
                            .w_full()
                            .h(px(24.))
                            .px_2()
                            .gap_2()
                            .items_center()
                            .cursor_pointer()
                            .hover(|row| row.bg(cx.theme().accent.opacity(0.5)))
                            .on_click(move |_, _, cx| {
                                let _ = toggle.update(cx, |f, cx| f.toggle_schema_table(&toggled, cx));
                            })
                            .child(div().w(px(10.)).flex_none().text_color(muted).child(if *open { "▾" } else { "▸" }))
                            .child(div().flex_none().child(name.clone()))
                            .child(div().flex_none().text_color(muted).child(format!("{}{columns} columns", if *view { "view · " } else { "" })))
                            .child(div().flex_1())
                            .child(Button::new(("sql-schema-data", ix)).xsmall().ghost().label("Data").on_click(move |_, window, cx| {
                                cx.stop_propagation();
                                let _ = data.update(cx, |f, cx| {
                                    let text = f.table_data_statement(&for_data);
                                    f.run_text(text, window, cx)
                                });
                            }))
                            .child(Button::new(("sql-schema-ddl", ix)).xsmall().ghost().label("DDL").on_click(move |_, _, cx| {
                                cx.stop_propagation();
                                let _ = ddl.update(cx, |f, cx| f.show_table_definition(for_ddl.to_string(), cx));
                            }))
                            .when(importable, |row| {
                                let (import, target) = (this.clone(), name.clone());
                                row.child(Button::new(("sql-schema-import", ix)).xsmall().ghost().label("Import CSV…").on_click(move |_, window, cx| {
                                    cx.stop_propagation();
                                    let _ = import.update(cx, |f, cx| f.import_csv(target.to_string(), window, cx));
                                }))
                            })
                            .child(Button::new(("sql-schema-copy", ix)).xsmall().ghost().label("Copy name").on_click(move |_, _, cx| {
                                cx.stop_propagation();
                                cx.write_to_clipboard(ClipboardItem::new_string(copy.to_string()));
                            }))
                            .into_any_element()
                    }
                    SchemaRow::Column { name, kind, key, nullable } => h_flex()
                        .id(("sql-schema-row", ix))
                        .h(px(24.))
                        .pl(px(34.))
                        .pr_2()
                        .gap_3()
                        .items_center()
                        .child(div().flex_none().child(name.clone()))
                        .child(div().flex_none().text_color(muted).child(kind.clone()))
                        .when(!key.is_empty(), |row| row.child(div().flex_none().text_color(cx.theme().primary).child(key.clone())))
                        .when(!*nullable, |row| row.child(div().flex_none().text_color(muted).child("not null")))
                        .into_any_element(),
                    SchemaRow::Detail { label, about, nested } => h_flex()
                        .id(("sql-schema-row", ix))
                        .h(px(24.))
                        .pl(px(if *nested { 34. } else { 28. }))
                        .pr_2()
                        .gap_3()
                        .items_center()
                        .text_color(muted)
                        .child(div().flex_none().child(label.clone()))
                        .child(div().flex_none().child(about.clone()))
                        .into_any_element(),
                })
                .collect::<Vec<_>>()
        })
        .flex_1()
        .min_h_0()
        .track_scroll(&sql.schema_scroll);
        let loaded = sql.schema.borrow().as_ref().map(|s| (s.database.clone(), s.tables.len()));
        v_flex()
            .id("sql-schema")
            .test_support()
            .size_full()
            .text_xs()
            .child(
                h_flex()
                    .flex_none()
                    .gap_2()
                    .px_2()
                    .py_1()
                    .items_center()
                    .children(sql.schema_filter.as_ref().map(|filter| div().w(px(280.)).child(Input::new(filter).xsmall())))
                    .when_some(loaded.clone(), |bar, (database, tables)| bar.child(div().text_color(muted).child(format!("{database} · {tables} tables{}", if sql.schema_checking { " · checking for changes…" } else { "" }))))
                    .child(div().flex_1())
                    .child(Button::new("sql-schema-refresh").xsmall().ghost().label("Refresh").on_click(cx.listener(|this, _, _, cx| this.load_schema(cx)))),
            )
            .when(loaded.is_none(), |view| view.child(div().p_3().text_color(muted).child(if sql.schema_checking { "Reading what this database holds…" } else if sql.database.is_some() { "Connect to see what this database holds" } else { "Choose a database" })))
            .when(loaded.is_some(), |view| view.child(list))
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::{HashSet, Schema, SchemaRow, schema_rows};

    fn schema() -> Schema {
        let row = |t: &str, c: &str| (t.to_string(), "BASE TABLE".to_string(), c.to_string(), "int".to_string(), "NO".to_string(), String::new());
        Schema::from_rows("shop".into(), [row("orders", "id"), row("orders", "total"), row("users", "id"), row("users", "email")])
    }

    fn names(rows: &[SchemaRow]) -> Vec<String> {
        rows.iter().map(|r| match r { SchemaRow::Table { name, .. } | SchemaRow::Column { name, .. } | SchemaRow::Detail { label: name, .. } => name.to_string() }).collect()
    }

    #[test]
    fn a_table_shows_its_columns_when_open() {
        let open = HashSet::from(["users".to_string()]);
        assert_eq!(names(&schema_rows(&schema(), &open, "")), ["orders", "users", "id", "email"]);
    }

    #[test]
    fn a_filter_finds_tables_and_the_tables_of_a_column() {
        assert_eq!(names(&schema_rows(&schema(), &HashSet::new(), "ord")), ["orders"]);
        assert_eq!(names(&schema_rows(&schema(), &HashSet::new(), "emai")), ["users", "email"]);
    }
}
