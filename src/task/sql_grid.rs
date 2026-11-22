//! The rows of a result, as the table asks for them: typed, sorted by the
//! values themselves, added to while the server still sends them, looked
//! through, with columns put away and cell changes waiting to be applied.

use std::collections::{BTreeMap, BTreeSet};

use gpui_kit::component::input::{Input, InputState};
use gpui_kit::component::menu::PopupMenu;
use gpui_kit::component::table::{Column, ColumnSort, CopyCells, SelectAllCells, TableDelegate, TableState};

use super::*;
use crate::db::{
    config::Driver,
    session::ColumnInfo,
    value::{Cell, Row},
};

/// Characters of a value drawn in its cell; the whole value is copied.
const SHOWN_CHARS: usize = 300;
/// Rows looked at for a column's width.
const WIDTH_SAMPLE: usize = 200;

/// A cell's new value, not yet sent. The row is told by its key as it was read.
#[derive(Clone, Debug, PartialEq)]
pub(super) struct StagedEdit {
    pub table: String,
    pub column: String,
    pub key: Vec<(String, String)>,
    pub literal: String,
    pub value: Cell,
}

/// One `UPDATE` for each row changed.
pub(super) fn batch_statements(edits: &BTreeMap<(u32, usize), StagedEdit>, driver: Driver) -> Vec<String> {
    let mut rows: BTreeMap<(&str, &[(String, String)]), Vec<&StagedEdit>> = BTreeMap::new();
    for edit in edits.values() {
        rows.entry((edit.table.as_str(), edit.key.as_slice())).or_default().push(edit);
    }
    rows.into_iter()
        .map(|((table, key), edits)| {
            let set = edits.iter().map(|e| format!("{} = {}", driver.quoted(&e.column), e.literal)).collect::<Vec<_>>().join(", ");
            let condition = key.iter().map(|(column, value)| format!("{} = {value}", driver.quoted(column))).collect::<Vec<_>>().join(" AND ");
            format!("UPDATE {} SET {set} WHERE {condition}", driver.quoted(table))
        })
        .collect()
}

/// Which rows hold `needle`, in any case, for the search over a result.
pub(super) fn search_hits(rows: &[Row], needle: &str) -> Vec<bool> {
    let needle = needle.to_lowercase();
    rows.iter().map(|row| row.iter().any(|cell| !cell.is_null() && cell.shown().to_lowercase().contains(&needle))).collect()
}

type ServerSort = Rc<dyn Fn(usize, ColumnSort, &mut Window, &mut App)>;
/// A cell's menu past Copy, by the row's place in the result and the column's.
pub(super) type CellMenu = Rc<dyn Fn(&SqlRows, u32, usize, PopupMenu, &App) -> PopupMenu>;

pub(super) struct SqlRows {
    columns: Vec<ColumnInfo>,
    /// Shared with an export under way, which goes on with what it was given.
    rows: Arc<Vec<Row>>,
    order: Vec<u32>,
    /// The columns shown, by their place in the result.
    visible: Vec<usize>,
    widths: Vec<f32>,
    sorted: Option<(usize, ColumnSort)>,
    /// The order the server was asked for: these rows came in it.
    by_server: Option<(usize, ColumnSort)>,
    /// Only part of the result is here: a sort has to be the server's.
    cut: bool,
    server_sort: Option<ServerSort>,
    hits: Option<Vec<bool>>,
    edits: BTreeMap<(u32, usize), StagedEdit>,
    /// Rows added here, not in the database yet.
    added: BTreeSet<u32>,
    /// Rows to delete, with their table and key as read.
    deleted: BTreeMap<u32, (String, Vec<(String, String)>)>,
    /// Added, then taken back: out of the table until the next save or discard.
    dropped: BTreeSet<u32>,
    /// The cell being typed in.
    editing: Option<(u32, usize, Entity<InputState>)>,
    menu: Option<CellMenu>,
}

impl SqlRows {
    pub(super) fn new(columns: Vec<ColumnInfo>) -> Self {
        let widths = columns.iter().map(|c| width_of(c.name.chars().count())).collect();
        let visible = (0..columns.len()).collect();
        SqlRows { columns, rows: Arc::default(), order: Vec::new(), visible, widths, sorted: None, by_server: None, cut: false, server_sort: None, hits: None, edits: BTreeMap::new(), added: BTreeSet::new(), deleted: BTreeMap::new(), dropped: BTreeSet::new(), editing: None, menu: None }
    }

    pub(super) fn len(&self) -> usize {
        self.rows.len()
    }

    /// Rows shown, a search leaving some out.
    pub(super) fn shown_len(&self) -> usize {
        self.order.len()
    }

    pub(super) fn columns(&self) -> &[ColumnInfo] {
        &self.columns
    }

    pub(super) fn set_cut(&mut self, cut: bool) {
        self.cut = cut;
    }

    pub(super) fn set_server_sort(&mut self, sort: ServerSort, current: Option<(usize, ColumnSort)>) {
        self.server_sort = Some(sort);
        self.by_server = current;
    }

    /// True for the first rows: the columns are then sized to them.
    pub(super) fn append(&mut self, batch: Vec<Row>) -> bool {
        let first = self.rows.is_empty();
        let from = self.rows.len() as u32;
        Arc::make_mut(&mut self.rows).extend(batch);
        if first {
            for (c, width) in self.widths.iter_mut().enumerate() {
                let longest = self.rows.iter().take(WIDTH_SAMPLE).filter_map(|r| r.get(c)).map(|v| v.shown().chars().count()).max().unwrap_or(0);
                *width = width.max(width_of(longest));
            }
        }
        if self.sorted.is_some() || self.hits.is_some() {
            self.rebuild();
        } else {
            self.order.extend(from..self.rows.len() as u32);
        }
        first
    }

    fn rebuild(&mut self) {
        self.order = (0..self.rows.len() as u32).collect();
        if let Some((column, sort)) = self.sorted.filter(|(_, sort)| *sort != ColumnSort::Default) {
            let rows = &self.rows;
            self.order.sort_by(|&a, &b| rows[a as usize][column].compare(&rows[b as usize][column]));
            if sort == ColumnSort::Descending {
                self.order.reverse();
            }
        }
        if let Some(hits) = &self.hits {
            self.order.retain(|&at| hits.get(at as usize).copied().unwrap_or(true) || self.added.contains(&at));
        }
        if !self.dropped.is_empty() {
            self.order.retain(|at| !self.dropped.contains(at));
        }
    }

    pub(super) fn set_menu(&mut self, menu: CellMenu) {
        self.menu = Some(menu);
    }

    /// How many changes wait for Save.
    pub(super) fn changes(&self) -> usize {
        self.edits.keys().filter(|(at, _)| !self.deleted.contains_key(at)).count() + self.deleted.len() + self.added.iter().filter(|at| !self.edits.keys().any(|(a, _)| a == *at)).count()
    }

    pub(super) fn has_changes(&self) -> bool {
        !self.edits.is_empty() || !self.deleted.is_empty() || !self.added.is_empty()
    }

    pub(super) fn is_added(&self, at: u32) -> bool {
        self.added.contains(&at)
    }

    pub(super) fn is_deleted(&self, at: u32) -> bool {
        self.deleted.contains_key(&at)
    }

    /// An empty row at the end, to be filled in; its place in the table.
    pub(super) fn add_row(&mut self) -> usize {
        let at = self.rows.len() as u32;
        let empty: Row = vec![Cell::Null; self.columns.len()].into_boxed_slice();
        Arc::make_mut(&mut self.rows).push(empty);
        self.added.insert(at);
        self.rebuild();
        self.order.iter().position(|a| *a == at).unwrap_or(0)
    }

    /// Marks a row for deleting, or takes the mark back. One added here just goes.
    pub(super) fn toggle_delete(&mut self, at: u32, table: String, key: Vec<(String, String)>) {
        if self.added.remove(&at) {
            self.edits.retain(|(a, _), _| *a != at);
            self.dropped.insert(at);
            self.rebuild();
        } else if self.deleted.remove(&at).is_none() {
            self.deleted.insert(at, (table, key));
        }
    }

    /// What Save runs: deletes, then updates, then inserts.
    pub(super) fn save_statements(&self, driver: Driver) -> Vec<String> {
        let mut out: Vec<String> = self.deleted.values().map(|(table, key)| {
            let condition = key.iter().map(|(column, value)| format!("{} = {value}", driver.quoted(column))).collect::<Vec<_>>().join(" AND ");
            format!("DELETE FROM {} WHERE {condition}", driver.quoted(table))
        }).collect();
        let updates: BTreeMap<(u32, usize), StagedEdit> = self.edits.iter().filter(|((at, _), _)| !self.added.contains(at) && !self.deleted.contains_key(at)).map(|(k, v)| (*k, v.clone())).collect();
        out.extend(batch_statements(&updates, driver));
        for at in &self.added {
            let row: Vec<&StagedEdit> = self.edits.iter().filter(|((a, _), _)| a == at).map(|(_, e)| e).collect();
            let Some(first) = row.first() else { continue };
            let names = row.iter().map(|e| driver.quoted(&e.column)).collect::<Vec<_>>().join(", ");
            let values = row.iter().map(|e| e.literal.as_str()).collect::<Vec<_>>().join(", ");
            out.push(format!("INSERT INTO {} ({names}) VALUES ({values})", driver.quoted(&first.table)));
        }
        out
    }

    pub(super) fn begin_edit(&mut self, at: u32, real: usize, input: Entity<InputState>) {
        self.editing = Some((at, real, input));
    }

    pub(super) fn end_edit(&mut self) -> Option<(u32, usize, Entity<InputState>)> {
        self.editing.take()
    }

    pub(super) fn is_editing(&self, input: &Entity<InputState>) -> bool {
        self.editing.as_ref().is_some_and(|(_, _, i)| i == input)
    }

    pub(super) fn all_rows(&self) -> Arc<Vec<Row>> {
        self.rows.clone()
    }

    pub(super) fn set_hits(&mut self, hits: Option<Vec<bool>>) {
        self.hits = hits;
        self.rebuild();
    }

    /// The place of a column of the table in the result.
    pub(super) fn real_column(&self, column: usize) -> usize {
        self.visible.get(column).copied().unwrap_or(column)
    }

    pub(super) fn row_id(&self, row: usize) -> Option<u32> {
        self.order.get(row).copied()
    }

    pub(super) fn is_visible(&self, real: usize) -> bool {
        self.visible.contains(&real)
    }

    /// Puts a column away or back, keeping at least one.
    pub(super) fn toggle_column(&mut self, real: usize) {
        if let Some(at) = self.visible.iter().position(|c| *c == real) {
            if self.visible.len() > 1 {
                self.visible.remove(at);
            }
        } else {
            self.visible.push(real);
            self.visible.sort_unstable();
        }
    }

    pub(super) fn remember_widths(&mut self, widths: &[Pixels]) {
        for (table_column, width) in widths.iter().enumerate() {
            if let Some(&real) = self.visible.get(table_column)
                && let Some(kept) = self.widths.get_mut(real)
            {
                *kept = f32::from(*width);
            }
        }
    }

    /// The value as read, whatever waits to change it.
    pub(super) fn read_value(&self, at: u32, real: usize) -> Option<&Cell> {
        self.rows.get(at as usize)?.get(real)
    }

    /// The value as shown: a change waiting, else what was read.
    pub(super) fn value(&self, at: u32, real: usize) -> Option<&Cell> {
        self.edits.get(&(at, real)).map(|e| &e.value).or_else(|| self.read_value(at, real))
    }

    pub(super) fn stage(&mut self, at: u32, real: usize, edit: StagedEdit) {
        if !self.added.contains(&at) && self.read_value(at, real) == Some(&edit.value) {
            self.edits.remove(&(at, real));
        } else {
            self.edits.insert((at, real), edit);
        }
    }

    pub(super) fn edits(&self) -> &BTreeMap<(u32, usize), StagedEdit> {
        &self.edits
    }

    pub(super) fn discard_edits(&mut self) {
        self.edits.clear();
        self.deleted.clear();
        self.editing = None;
        let gone: BTreeSet<u32> = self.added.iter().chain(&self.dropped).copied().collect();
        self.added.clear();
        self.dropped.clear();
        self.remove_rows(&gone);
    }

    /// The server took them: they are what the rows hold now.
    pub(super) fn keep_edits(&mut self) {
        let edits = std::mem::take(&mut self.edits);
        let rows = Arc::make_mut(&mut self.rows);
        for ((at, real), edit) in edits {
            if let Some(cell) = rows.get_mut(at as usize).and_then(|r| r.get_mut(real)) {
                *cell = edit.value;
            }
        }
        let gone: BTreeSet<u32> = self.deleted.keys().chain(&self.dropped).copied().collect();
        self.deleted.clear();
        self.added.clear();
        self.dropped.clear();
        self.remove_rows(&gone);
    }

    fn remove_rows(&mut self, gone: &BTreeSet<u32>) {
        if !gone.is_empty() {
            let mut at = 0u32;
            Arc::make_mut(&mut self.rows).retain(|_| {
                let keep = !gone.contains(&at);
                at += 1;
                keep
            });
            // A search's marks are by place, and the places moved.
            self.hits = None;
        }
        self.rebuild();
    }

    /// The column names, the rows and their order as shown, for an export.
    pub(super) fn snapshot(&self) -> (Vec<String>, Arc<Vec<Row>>, Vec<u32>) {
        (self.columns.iter().map(|c| c.name.clone()).collect(), self.rows.clone(), self.order.clone())
    }

    fn cell(&self, row: usize, column: usize) -> Option<&Cell> {
        self.value(*self.order.get(row)?, self.real_column(column))
    }
}

fn width_of(chars: usize) -> f32 {
    (chars.min(48) as f32 * 7.5 + 24.).max(64.)
}

/// One line of at most [`SHOWN_CHARS`]: rows are all of one height.
fn one_line(text: &str) -> SharedString {
    let mut out = String::with_capacity(text.len().min(SHOWN_CHARS));
    for (i, c) in text.chars().enumerate() {
        if i == SHOWN_CHARS {
            out.push('…');
            break;
        }
        out.push(if matches!(c, '\n' | '\r' | '\t') { ' ' } else { c });
    }
    out.into()
}

impl TableDelegate for SqlRows {
    fn columns_count(&self, _: &App) -> usize {
        self.visible.len()
    }

    fn rows_count(&self, _: &App) -> usize {
        self.order.len()
    }

    fn column(&self, col_ix: usize, _: &App) -> Column {
        let real = self.real_column(col_ix);
        let info = &self.columns[real];
        let column = Column::new(SharedString::from(format!("c{real}")), info.name.clone())
            .width(px(self.widths.get(real).copied().unwrap_or(100.)))
            .max_width(px(1200.))
            .sortable();
        let column = match self.sorted.or(self.by_server) {
            Some((sorted, sort)) if sorted == real => column.sort(sort),
            _ => column,
        };
        if info.kind.right_aligned() { column.text_right() } else { column }
    }

    fn perform_sort(&mut self, col_ix: usize, sort: ColumnSort, window: &mut Window, cx: &mut Context<TableState<Self>>) {
        let real = self.real_column(col_ix);
        // Sorting the rows that are here would look like sorting all of them.
        if self.cut
            && let Some(server) = self.server_sort.clone()
        {
            window.defer(cx, move |window, cx| server(real, sort, window, cx));
            return;
        }
        self.by_server = None;
        self.sorted = (sort != ColumnSort::Default).then_some((real, sort));
        self.rebuild();
    }

    fn row_header_width(&self) -> Option<Pixels> {
        let digits = self.rows.len().max(1).ilog10() as f32 + 1.;
        Some(px(digits * 7.5 + 14.))
    }

    fn row_header_text(&self, row_ix: usize) -> Option<SharedString> {
        Some((self.order.get(row_ix)? + 1).to_string().into())
    }

    fn render_td(&mut self, row_ix: usize, col_ix: usize, _: &mut Window, cx: &mut Context<TableState<Self>>) -> impl IntoElement {
        let muted = cx.theme().muted_foreground;
        let at = self.order.get(row_ix).copied();
        let real = self.real_column(col_ix);
        if let Some((_, _, input)) = self.editing.as_ref().filter(|(a, r, _)| Some(*a) == at && *r == real) {
            let input = input.clone();
            return div()
                .id("sql-cell-editor")
                .test_support()
                .size_full()
                .capture_key_down(cx.listener(|table, event: &KeyDownEvent, _, cx| {
                    if event.keystroke.key == "escape" {
                        table.delegate_mut().end_edit();
                        cx.stop_propagation();
                        cx.notify();
                    }
                }))
                .child(Input::new(&input).xsmall())
                .into_any_element();
        }
        let waiting = at.is_some_and(|at| self.edits.contains_key(&(at, real)));
        let added = at.is_some_and(|at| self.added.contains(&at));
        let deleted = at.is_some_and(|at| self.deleted.contains_key(&at));
        let element = match self.cell(row_ix, col_ix) {
            Some(cell) if cell.is_null() => div().truncate().italic().text_color(muted).child("NULL"),
            Some(cell) => div().truncate().child(one_line(&cell.shown())),
            None => div(),
        };
        element
            .when(added, |cell| cell.size_full().bg(hsla(0.38, 0.7, 0.45, 0.16)))
            .when(waiting, |cell| cell.size_full().bg(hsla(0.12, 0.9, 0.55, 0.28)))
            .when(deleted, |cell| cell.size_full().line_through().text_color(muted).bg(hsla(0.0, 0.8, 0.55, 0.16)))
            .into_any_element()
    }

    fn cell_context_menu(&mut self, row: usize, col: usize, menu: PopupMenu, _: &mut Window, cx: &mut Context<TableState<Self>>) -> PopupMenu {
        let menu = menu.menu("Copy", Box::new(CopyCells)).menu("Select All", Box::new(SelectAllCells));
        match (self.menu.clone(), self.order.get(row).copied()) {
            (Some(more), Some(at)) => more(self, at, self.real_column(col), menu, cx),
            _ => menu,
        }
    }

    fn cell_text(&self, row_ix: usize, col_ix: usize, _: &App) -> String {
        self.cell(row_ix, col_ix).map(|c| c.copied().into_owned()).unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::{BTreeMap, ColumnInfo, ColumnSort, Driver, SHOWN_CHARS, SqlRows, StagedEdit, batch_statements, one_line, search_hits};
    use crate::db::value::{Cell, Kind};

    fn info(name: &str, kind: Kind) -> ColumnInfo {
        ColumnInfo { name: name.into(), kind, source: None }
    }

    fn rows() -> SqlRows {
        let mut rows = SqlRows::new(vec![info("amount", Kind::Decimal)]);
        rows.append(["9.99", "10.00", "-1.00"].map(|v| vec![Cell::Decimal(v.into())].into_boxed_slice()).into());
        rows.append(vec![vec![Cell::Null].into_boxed_slice()]);
        rows
    }

    fn column(rows: &SqlRows) -> Vec<String> {
        (0..rows.order.len()).map(|r| rows.cell(r, 0).unwrap().copied().into_owned()).collect()
    }

    #[test]
    fn a_column_sorts_by_value_and_rows_that_come_later_join_the_order() {
        let mut rows = rows();
        rows.sorted = Some((0, ColumnSort::Ascending));
        rows.rebuild();
        assert_eq!(column(&rows), ["NULL", "-1.00", "9.99", "10.00"]);
        rows.append(vec![vec![Cell::Decimal("5.00".into())].into_boxed_slice()]);
        assert_eq!(column(&rows), ["NULL", "-1.00", "5.00", "9.99", "10.00"]);
        rows.sorted = None;
        rows.rebuild();
        assert_eq!(column(&rows)[0], "9.99");
    }

    #[test]
    fn a_search_keeps_the_rows_that_hold_it_in_their_order() {
        let mut rows = rows();
        let hits = search_hits(&rows.all_rows(), "0.0");
        rows.set_hits(Some(hits));
        assert_eq!(column(&rows), ["10.00"]);
        assert_eq!(rows.row_id(0), Some(1), "its place in the result, for its number");
        rows.set_hits(None);
        assert_eq!(rows.shown_len(), 4);
        assert!(!search_hits(&rows.all_rows(), "null")[3], "NULL is no text to find");
    }

    #[test]
    fn a_column_put_away_leaves_the_others_their_values() {
        let mut rows = SqlRows::new(vec![info("a", Kind::Text), info("b", Kind::Text)]);
        rows.append(vec![vec![Cell::Text("1".into()), Cell::Text("2".into())].into_boxed_slice()]);
        rows.toggle_column(0);
        assert_eq!(rows.cell(0, 0), Some(&Cell::Text("2".into())));
        rows.toggle_column(1);
        assert_eq!(rows.visible, [1], "one stays");
        rows.toggle_column(0);
        assert_eq!(rows.visible, [0, 1]);
    }

    fn edit(column: &str, id: &str, value: &str) -> StagedEdit {
        StagedEdit { table: "t".into(), column: column.into(), key: vec![("id".into(), id.into())], literal: format!("'{value}'"), value: Cell::Text(value.into()) }
    }

    #[test]
    fn changes_of_one_row_are_one_update() {
        let mut edits = BTreeMap::new();
        edits.insert((0, 1), edit("a", "1", "x"));
        edits.insert((0, 2), edit("b", "1", "y"));
        edits.insert((1, 1), edit("a", "2", "z"));
        assert_eq!(batch_statements(&edits, Driver::MySql), ["UPDATE `t` SET `a` = 'x', `b` = 'y' WHERE `id` = 1", "UPDATE `t` SET `a` = 'z' WHERE `id` = 2"]);
    }

    #[test]
    fn a_change_shows_until_applied_and_a_value_put_back_is_no_change() {
        let mut rows = SqlRows::new(vec![info("a", Kind::Text)]);
        rows.append(vec![vec![Cell::Text("old".into())].into_boxed_slice()]);
        rows.stage(0, 0, edit("a", "1", "new"));
        assert_eq!(rows.value(0, 0), Some(&Cell::Text("new".into())));
        assert_eq!(rows.read_value(0, 0), Some(&Cell::Text("old".into())));
        rows.stage(0, 0, edit("a", "1", "old"));
        assert!(rows.edits().is_empty());
        rows.stage(0, 0, edit("a", "1", "new"));
        rows.keep_edits();
        assert_eq!(rows.read_value(0, 0), Some(&Cell::Text("new".into())));
    }

    #[test]
    fn save_runs_deletes_then_updates_then_inserts() {
        let mut rows = SqlRows::new(vec![info("id", Kind::Text), info("a", Kind::Text)]);
        rows.append(vec![vec![Cell::Text("1".into()), Cell::Text("x".into())].into_boxed_slice(), vec![Cell::Text("2".into()), Cell::Text("y".into())].into_boxed_slice()]);
        rows.toggle_delete(0, "t".into(), vec![("id".into(), "1".into())]);
        rows.stage(1, 1, edit("a", "2", "z"));
        let new = rows.add_row() as u32;
        let mut added = edit("a", "", "n");
        added.key.clear();
        rows.stage(new, 1, added);
        assert_eq!(rows.changes(), 3);
        assert_eq!(rows.save_statements(Driver::MySql), ["DELETE FROM `t` WHERE `id` = 1", "UPDATE `t` SET `a` = 'z' WHERE `id` = 2", "INSERT INTO `t` (`a`) VALUES ('n')"]);
        rows.keep_edits();
        assert!(!rows.has_changes());
        assert_eq!(rows.len(), 2, "the deleted row is gone");
        assert_eq!(rows.value(0, 1), Some(&Cell::Text("z".into())));
    }

    #[test]
    fn a_row_added_and_taken_back_or_discarded_leaves_nothing() {
        let mut rows = rows();
        let before = rows.len();
        let new = rows.add_row() as u32;
        rows.toggle_delete(new, String::new(), Vec::new());
        assert_eq!(rows.shown_len(), before);
        assert!(rows.save_statements(Driver::MySql).is_empty());
        rows.add_row();
        rows.discard_edits();
        assert_eq!(rows.len(), before);
        assert!(!rows.has_changes());
    }

    #[gpui_kit::test]
    fn a_range_of_cells_is_copied_as_a_spreadsheet_pastes_it(cx: &mut gpui_kit::TestAppContext) {
        use gpui_kit::AppContext as _;
        use gpui_kit::component::table::TableState;
        cx.update(gpui_kit::init);
        let window = cx.add_empty_window();
        let table = window.update(|window, cx| {
            let mut rows = SqlRows::new(vec![info("a", Kind::Text), info("b", Kind::Text), info("c", Kind::Text)]);
            let row = |a: &str, b: Cell, c: &str| vec![Cell::Text(a.into()), b, Cell::Text(c.into())].into_boxed_slice();
            rows.append(vec![row("1", Cell::Null, "x"), row("2", Cell::Text("two\tcells".into()), "y"), row("3", Cell::Text("say \"hi\"".into()), "z")]);
            cx.new(|cx| TableState::new(rows, window, cx).cell_selectable(true))
        });
        table.update(window, |t, cx| {
            t.set_selected_cell(0, 1, cx);
            t.extend_selection(2, 2, false, cx);
            assert_eq!(t.selected_cells(), Some((0..3, 1..3)));
            assert_eq!(t.selected_text(cx).unwrap(), "NULL\tx\n\"two\tcells\"\ty\n\"say \"\"hi\"\"\"\tz");
            t.extend_selection(0, 0, false, cx);
            assert_eq!(t.selected_cells(), Some((0..1, 0..2)));
            t.set_selected_cell(1, 1, cx);
            assert_eq!(t.selected_cells(), Some((1..2, 1..2)), "a plain click ends the range");
        });
    }

    #[test]
    fn a_long_value_is_drawn_on_one_line_and_cut() {
        let text = format!("a\nb{}", "x".repeat(SHOWN_CHARS * 2));
        let line = one_line(&text);
        assert!(line.starts_with("a b"));
        assert!(line.ends_with('…'));
        assert_eq!(line.chars().count(), SHOWN_CHARS + 1);
    }
}
