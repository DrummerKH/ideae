//! The commit graph of History: which lane each commit is drawn in, and
//! the lines that pass, join and leave its row. Laid out once per load,
//! off the UI thread; a row is drawn from what is kept here.

/// Lanes drawn at most; the ones past it share the last, drawn gray.
pub const MOST_LANES: usize = 16;

/// A line longer than this many rows is drawn as two short ends with
/// arrows, as JetBrains does, and holds no lane in between: a branch that
/// left main long ago would otherwise take a lane for all those rows.
pub const LONG: usize = 30;

/// A lane and its color (an index into the palette).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Line {
    pub lane: usize,
    pub color: usize,
}

/// One commit's row.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Row {
    /// The commit's own lane and color.
    pub at: usize,
    pub color: usize,
    /// Lanes that cross the row without touching the commit.
    pub through: Vec<Line>,
    /// Lanes that end in the commit from above: its own, and branches merged
    /// into it by commits listed earlier.
    pub into: Vec<Line>,
    /// Lanes that leave it below, to its parents.
    pub out: Vec<Line>,
    /// Long lines: leaving it below for a parent far down, and reaching it
    /// from a child far up; each drawn short, with an arrow.
    pub down: Vec<Line>,
    pub up: Vec<Line>,
    /// The row each long line goes to or comes from: below for those that
    /// leave, above for those that reach it. `None` past the rows listed.
    pub down_to: Vec<Option<usize>>,
    pub up_from: Vec<usize>,
    /// More than one parent.
    pub merge: bool,
}

/// Lays out commits listed newest first, each with its parents.
pub fn layout<'a>(commits: impl IntoIterator<Item = (&'a str, &'a [String])>) -> Vec<Row> {
    let commits: Vec<(&str, &[String])> = commits.into_iter().collect();
    let place: std::collections::HashMap<&str, usize> = commits.iter().enumerate().map(|(i, (hash, _))| (*hash, i)).collect();
    // Long lines on their way, by the commit they go to: their colors.
    let mut arriving: std::collections::HashMap<&str, Vec<(usize, usize)>> = std::collections::HashMap::new();
    // What each lane waits for: the commit it goes down to, and its color.
    let mut lanes: Vec<Option<(&str, usize)>> = Vec::new();
    let mut colors = 0usize;
    let mut new_color = || {
        colors += 1;
        colors - 1
    };
    let mut rows = Vec::new();
    for (ix, &(hash, parents)) in commits.iter().enumerate() {
        let waiting: Vec<usize> = (0..lanes.len()).filter(|&i| lanes[i].is_some_and(|(h, _)| h == hash)).collect();
        let (at, color) = match waiting.first() {
            Some(&i) => (i, lanes[i].map_or(0, |(_, c)| c)),
            // A branch's newest commit: a lane of its own.
            None => {
                let free = lanes.iter().position(Option::is_none).unwrap_or(lanes.len());
                (free, new_color())
            }
        };
        let into = waiting.iter().map(|&i| Line { lane: i, color: lanes[i].map_or(color, |(_, c)| c) }).collect();
        let through = (0..lanes.len())
            .filter(|i| !waiting.contains(i))
            .filter_map(|i| lanes[i].map(|(_, color)| Line { lane: i, color }))
            .collect();
        for &i in &waiting {
            lanes[i] = None;
        }
        if at >= lanes.len() {
            lanes.resize(at + 1, None);
        }
        // Long lines reaching it: from above, in its own lane first when
        // nothing else comes down it, then in the free ones beside.
        let mut up = Vec::new();
        let mut up_from = Vec::new();
        let mut slot = at;
        for (color, from) in arriving.remove(hash).unwrap_or_default() {
            up_from.push(from);
            if !(up.is_empty() && waiting.is_empty()) {
                slot = (slot + 1..).find(|&i| lanes.get(i).is_none_or(Option::is_none)).unwrap_or(slot + 1);
            }
            up.push(Line { lane: slot, color });
        }
        let far = |parent: &str| place.get(parent).is_none_or(|&p| p > ix + LONG);
        let mut down = Vec::new();
        let mut down_to = Vec::new();
        let mut out = Vec::new();
        for (n, parent) in parents.iter().enumerate() {
            let parent = parent.as_str();
            // The first parent goes on in the commit's own lane, even when
            // another lane waits for it too: they meet at that parent, and a
            // branch's line stays straight.
            if far(parent) {
                let line_color = if n == 0 { color } else { new_color() };
                let lane = if n == 0 { at } else { (at + 1..).find(|&i| lanes.get(i).is_none_or(Option::is_none)).unwrap_or(at + 1) };
                down.push(Line { lane, color: line_color });
                down_to.push(place.get(parent).copied());
                if place.contains_key(parent) {
                    arriving.entry(parent).or_default().push((line_color, ix));
                }
                continue;
            }
            let (lane, line_color) = if n == 0 {
                (at, color)
            } else if let Some(i) = lanes.iter().position(|l| l.is_some_and(|(h, _)| h == parent)) {
                // A merged commit already awaited: the line joins its lane.
                out.push(Line { lane: i, color: lanes[i].map_or(color, |(_, c)| c) });
                continue;
            } else {
                let free = lanes.iter().position(Option::is_none).unwrap_or(lanes.len());
                (free, new_color())
            };
            if lane >= lanes.len() {
                lanes.resize(lane + 1, None);
            }
            lanes[lane] = Some((parent, line_color));
            out.push(Line { lane, color: line_color });
        }
        while lanes.last().is_some_and(Option::is_none) {
            lanes.pop();
        }
        rows.push(Row { at, color, through, into, out, down, up, down_to, up_from, merge: parents.len() > 1 });
    }
    rows
}

/// The lanes a row needs drawn: its widest.
pub fn width(row: &Row) -> usize {
    row.through.iter().chain(&row.into).chain(&row.out).chain(&row.down).chain(&row.up).map(|l| l.lane).chain([row.at]).max().map_or(1, |w| w + 1).min(MOST_LANES)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rows(commits: &[(&str, &[&str])]) -> Vec<Row> {
        let owned: Vec<(String, Vec<String>)> = commits.iter().map(|(h, p)| (h.to_string(), p.iter().map(|s| s.to_string()).collect())).collect();
        layout(owned.iter().map(|(h, p)| (h.as_str(), p.as_slice())))
    }

    #[test]
    fn a_straight_history_is_one_lane() {
        let rows = rows(&[("c", &["b"]), ("b", &["a"]), ("a", &[])]);
        assert!(rows.iter().all(|r| r.at == 0 && r.through.is_empty()));
        assert_eq!(rows[2].out, []);
    }

    #[test]
    fn a_branch_leaves_and_a_merge_joins() {
        // m merges f into main; f and b both come from a.
        let rows = rows(&[("m", &["b", "f"]), ("f", &["a"]), ("b", &["a"]), ("a", &[])]);
        assert!(rows[0].merge);
        assert_eq!(rows[0].out.iter().map(|l| l.lane).collect::<Vec<_>>(), [0, 1], "main below, the branch beside it");
        assert_eq!((rows[1].at, rows[1].through.len()), (1, 1), "f in its lane, main passing by");
        assert_eq!(rows[2].at, 0);
        // f's line goes on to a in its lane, and ends there.
        assert_eq!(rows[3].into.iter().map(|l| l.lane).collect::<Vec<_>>(), [0, 1]);
        assert_eq!(rows[3].at, 0);
        assert_ne!(rows[0].color, rows[1].color);
    }

    #[test]
    fn two_tips_get_two_lanes_and_lanes_are_reused() {
        let rows = rows(&[("x", &["a"]), ("y", &["a"]), ("a", &["z"]), ("z", &[])]);
        assert_eq!((rows[0].at, rows[1].at), (0, 1));
        assert_eq!(rows[1].out, [Line { lane: 1, color: rows[1].color }], "y goes down its own lane");
        assert_eq!((rows[2].at, rows[2].into.len()), (0, 2), "and both meet at a");
        assert_eq!(rows[3].through, [], "nothing left beside");
        assert_eq!(width(&rows[1]), 2);
    }

    #[test]
    fn a_long_line_is_two_arrows_and_holds_no_lane() {
        // A branch tip whose parent is far down main.
        let main: Vec<String> = (0..=LONG + 5).map(|i| format!("m{i}")).collect();
        let mut owned: Vec<(String, Vec<String>)> = vec![("t".into(), vec![main[LONG + 2].clone()])];
        for (i, m) in main.iter().enumerate() {
            owned.push((m.clone(), main.get(i + 1).cloned().into_iter().collect()));
        }
        let rows = layout(owned.iter().map(|(h, p)| (h.as_str(), p.as_slice())));
        assert_eq!(rows[0].down.len(), 1, "the tip leaves with an arrow");
        assert!(rows[0].out.is_empty());
        assert!(rows[1..=LONG + 2].iter().all(|r| r.through.is_empty()), "no lane held in between");
        assert_eq!(rows[LONG + 3].up.len(), 1, "and its parent is reached by one");
        assert_eq!(rows[LONG + 3].up[0].color, rows[0].color);
        assert_eq!((rows[0].down_to.clone(), rows[LONG + 3].up_from.clone()), (vec![Some(LONG + 3)], vec![0]), "each end knows the other");
        assert_eq!(rows.last().unwrap().out, [], "main's oldest goes nowhere");
    }
}
