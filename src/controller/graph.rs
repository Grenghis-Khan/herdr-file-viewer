//! Source-control feature submodule: the git graph section under the tree (paging, selection,
//! all-branches scope, commit-id copy) and its intents. All state is session-only and every git
//! access goes through the injected read-only [`GitService`] (AC-N2/N3).

use super::*;
use crate::history::{CommitRow, PAGE};
use crate::presenter::{GRAPH_PCT_MAX, GRAPH_PCT_MIN};

/// How many percentage points one `{`/`}` keypress moves the tree/graph divider.
const GRAPH_PCT_STEP: u16 = 10;
/// Load the next page when the cursor comes within this many rows of the loaded end.
const LOAD_MORE_MARGIN: usize = 10;

/// The graph section's session state: the loaded `git log --graph` rows (and their ingested
/// display lines, converted once per load — never per frame), the cursor, and the paging state.
pub(super) struct GraphState {
    /// The loaded rows, in display order. `rows[i].sha` maps row → commit.
    rows: Vec<CommitRow>,
    /// `rows[i].line` ingested through the escape-neutralizing ANSI path (AC-27), 1:1 with
    /// `rows`. Cached here so the per-frame view snapshot is a clone, not a re-parse.
    lines: Vec<ratatui::text::Line<'static>>,
    /// Index into `rows` of the selected row.
    cursor: usize,
    /// Whether the walk covers all refs (`--all`) instead of HEAD only.
    all: bool,
    /// How many COMMITS (rows with a sha — edge rows don't count) have been loaded, i.e. the
    /// `--skip` for the next page.
    commits: usize,
    /// The last page returned fewer commits than requested — the history is fully loaded.
    exhausted: bool,
}

impl Controller {
    /// `g` — toggle the graph section. Opening loads the first page (synchronously: one bounded
    /// `git log -nPAGE`, comfortably inside the AC-22 budget) and focuses the section; closing
    /// returns focus to the tree. Inert outside a repo (AC-26).
    pub(super) fn toggle_graph(&mut self) -> Effects {
        if self.graph.is_some() {
            self.graph = None;
            if self.focus == Focus::Graph {
                self.focus = Focus::Tree;
            }
            return Effects::redraw();
        }
        if !self.is_git_repo {
            return Effects::noop();
        }
        let rows = self.git.log_graph(false, 0, PAGE);
        self.graph = Some(GraphState::from_page(rows, false));
        self.focus = Focus::Graph;
        Effects::redraw()
    }

    /// `B` — switch the graph between current-branch and all-branches history. Reloads the
    /// first page under the new scope (selection restarts at the top — the row space changed).
    /// Inert while the section is hidden.
    pub(super) fn toggle_all_branches(&mut self) -> Effects {
        let Some(graph) = &self.graph else {
            return Effects::noop();
        };
        let all = !graph.all;
        let rows = self.git.log_graph(all, 0, PAGE);
        self.graph = Some(GraphState::from_page(rows, all));
        Effects::redraw()
    }

    /// `{` — shrink the graph section by one step.
    pub(super) fn shrink_graph(&mut self) -> Effects {
        self.resize_graph(-(GRAPH_PCT_STEP as i16))
    }

    /// `}` — grow the graph section by one step.
    pub(super) fn grow_graph(&mut self) -> Effects {
        self.resize_graph(GRAPH_PCT_STEP as i16)
    }

    /// Move the tree/graph divider by `delta` percentage points, clamped so neither section can
    /// collapse. Pure layout state. Inert while the section is hidden.
    fn resize_graph(&mut self, delta: i16) -> Effects {
        if self.graph.is_none() {
            return Effects::noop();
        }
        let next =
            (self.graph_pct as i16 + delta).clamp(GRAPH_PCT_MIN as i16, GRAPH_PCT_MAX as i16);
        self.graph_pct = next as u16;
        Effects::redraw()
    }

    /// Move the graph cursor by `delta` rows (the j/k/arrow path while the graph has focus),
    /// loading the next page when the cursor nears the loaded end (auto-load-more).
    pub(super) fn graph_move(&mut self, delta: isize) -> Effects {
        let Some(graph) = &mut self.graph else {
            return Effects::noop();
        };
        if graph.rows.is_empty() {
            return Effects::noop();
        }
        let max = graph.rows.len() - 1;
        graph.cursor = (graph.cursor as isize + delta).clamp(0, max as isize) as usize;
        self.maybe_load_more();
        Effects::redraw()
    }

    /// Set the graph cursor to an absolute row (a mouse click), clamped to the loaded rows.
    pub(super) fn graph_set_cursor(&mut self, idx: usize) {
        if let Some(graph) = &mut self.graph {
            if graph.rows.is_empty() {
                return;
            }
            graph.cursor = idx.min(graph.rows.len() - 1);
            self.maybe_load_more();
        }
    }

    /// Append the next page when the cursor is near the loaded end and more history remains.
    /// One bounded `git log` per page; `exhausted` stops the viewer from re-asking a finished
    /// history on every keystroke at the bottom.
    fn maybe_load_more(&mut self) {
        let Some(graph) = &self.graph else { return };
        if graph.exhausted || graph.cursor + LOAD_MORE_MARGIN < graph.rows.len() {
            return;
        }
        let (all, skip) = (graph.all, graph.commits);
        let page = self.git.log_graph(all, skip, PAGE);
        if let Some(graph) = &mut self.graph {
            graph.append_page(page);
        }
    }

    /// `y`/`Y` while the graph has focus — copy the selected commit's short (`y`) or full
    /// (`Y`) id, through the same sanitize + OSC 52 path as the tree's path copy.
    pub(super) fn copy_commit_sha(&mut self, full: bool) -> Effects {
        let Some(sha) = self
            .graph
            .as_ref()
            .and_then(|g| g.rows.get(g.cursor))
            .and_then(|r| r.sha.as_deref())
        else {
            return Effects::noop();
        };
        let text = if full {
            sha.to_string()
        } else {
            sha.chars().take(7).collect()
        };
        self.action_notice = Some(match self.clipboard.copy(&text) {
            Ok(()) => format!("Copied {text}"),
            Err(e) => format!("Could not copy commit id: {e}"),
        });
        Effects::redraw()
    }

    /// The graph section's draw model for this frame, or `None` while hidden.
    pub(super) fn graph_view(&self) -> Option<crate::presenter::GraphView> {
        let graph = self.graph.as_ref()?;
        let title = if graph.all {
            "History · all".to_string()
        } else {
            "History".to_string()
        };
        Some(crate::presenter::GraphView {
            lines: graph.lines.clone(),
            cursor: graph.cursor,
            scroll: self.geom.graph_scroll,
            title,
        })
    }
}

impl GraphState {
    /// Build the state from a freshly-loaded first page.
    fn from_page(rows: Vec<CommitRow>, all: bool) -> Self {
        let mut state = GraphState {
            rows: Vec::new(),
            lines: Vec::new(),
            cursor: 0,
            all,
            commits: 0,
            exhausted: false,
        };
        state.append_page(rows);
        state
    }

    /// Append one loaded page: ingest the display lines (once, not per frame) and advance the
    /// paging state. A short page marks the history exhausted.
    fn append_page(&mut self, page: Vec<CommitRow>) {
        let commits = page.iter().filter(|r| r.sha.is_some()).count();
        if commits < PAGE {
            self.exhausted = true;
        }
        self.commits += commits;
        for row in &page {
            // One row each: `to_text` neutralizes escapes (AC-27) and maps the ANSI styling;
            // a graph row is a single line, so take the first (empty rows stay empty).
            let text = crate::render::to_text(&row.line);
            self.lines
                .push(text.lines.into_iter().next().unwrap_or_default());
        }
        self.rows.extend(page);
    }
}
