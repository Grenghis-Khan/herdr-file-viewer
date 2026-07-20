//! Source-control feature submodule: the git graph section under the tree, commit mode (the
//! unified commit view + commit-scoped tree), and their intents. All state is session-only
//! and every git access goes through the injected read-only [`GitService`] (AC-N2/N3).

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

/// Commit mode: a commit selected from the graph. The content pane shows the unified commit
/// view and the tree is scoped to the commit's changed files until the mode is left.
pub(super) struct CommitState {
    /// The full commit id (validated hex — see [`crate::history::is_valid_sha`]).
    pub(super) sha: String,
    /// The commit's changed files (repo-root-relative → status) — the commit-scoped tree's
    /// changed-set source.
    files: BTreeMap<PathBuf, Status>,
    /// The tree cursor before the mode was entered, restored on leave so the working-tree
    /// selection survives a history detour.
    prev_cursor: usize,
}

impl Controller {
    /// `g` — toggle the graph section. Opening loads the first page (synchronously: one bounded
    /// `git log -nPAGE`, comfortably inside the AC-22 budget) and focuses the section; closing
    /// leaves commit mode if active and returns focus to the tree. Inert outside a repo (AC-26).
    pub(super) fn toggle_graph(&mut self) -> Effects {
        if self.graph.is_some() {
            if self.commit.is_some() {
                self.leave_commit_mode();
            }
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

    /// `Enter` on a graph row — enter commit mode for the row's commit: scope the tree to the
    /// commit's changed files (the working-tree cursor is saved for the way back) and render
    /// the unified commit view. A pure edge row (no commit) is inert. Re-entering for the
    /// commit already shown is a no-op.
    pub(super) fn open_selected_commit(&mut self) -> Effects {
        let Some(sha) = self
            .graph
            .as_ref()
            .and_then(|g| g.rows.get(g.cursor))
            .and_then(|r| r.sha.clone())
        else {
            return Effects::noop();
        };
        if self.commit.as_ref().is_some_and(|c| c.sha == sha) {
            return Effects::noop();
        }
        let files = self.git.commit_files(&sha);
        // Entering (or switching commits within) commit mode: save the working-tree cursor only
        // on first entry, so the restore returns to where the user actually was.
        let prev_cursor = match self.commit.take() {
            Some(prev) => prev.prev_cursor,
            None => self.tree.cursor(),
        };
        self.tree.set_changed_only(true, &files);
        self.tree.set_cursor(0);
        self.commit = Some(CommitState {
            sha,
            files,
            prev_cursor,
        });
        self.dispatch_render(); // routes to the commit patch while commit mode is active
        Effects::redraw()
    }

    /// Leave commit mode: restore the working-tree filter state (`d` status set, `c` changed
    /// set, or the full tree), the saved cursor, and the normal selection render.
    pub(super) fn leave_commit_mode(&mut self) {
        let Some(commit) = self.commit.take() else {
            return;
        };
        if self.status_mode {
            self.tree.set_changed_only(true, &self.git_status);
        } else {
            self.tree.set_changed_only(self.changed_only, &self.changed);
        }
        self.tree.set_cursor(commit.prev_cursor);
        self.dispatch_render();
    }

    /// Whether commit mode is active. Exposed for tests.
    pub fn commit_mode(&self) -> Option<&str> {
        self.commit.as_deref_sha()
    }

    /// `]` / `[` — select the commit's next/previous changed **file** and scroll the unified
    /// commit view to its diff section. Inert outside commit mode.
    pub(super) fn step_file_section(&mut self, dir: isize) -> Effects {
        if self.commit.is_none() {
            return Effects::noop();
        }
        let nodes = self.tree.visible_nodes();
        let files: Vec<usize> = nodes
            .iter()
            .enumerate()
            .filter(|(_, n)| n.kind == NodeKind::File)
            .map(|(i, _)| i)
            .collect();
        if files.is_empty() {
            return Effects::noop();
        }
        let cursor = self.tree.cursor();
        // The next file strictly after (or previous strictly before) the current cursor row,
        // whatever kind of row the cursor is on; clamped at the ends (no wrap — matches the
        // tree's own edge behavior).
        let target = if dir > 0 {
            files.iter().copied().find(|&i| i > cursor)
        } else {
            files.iter().copied().rev().find(|&i| i < cursor)
        };
        let Some(idx) = target else {
            return Effects::noop();
        };
        self.tree.set_cursor(idx);
        self.scroll_to_selected_commit_file();
        Effects::redraw()
    }

    /// Scroll the unified commit view to the selected file's diff section: the first displayed
    /// line containing the file's repo-relative path (delta's file headers and raw
    /// `diff --git` headers both carry it). Degrades to no scroll when the render is still in
    /// flight or the path text isn't found — never an error.
    pub(super) fn scroll_to_selected_commit_file(&mut self) {
        if self.content_rendering {
            return;
        }
        let Some(node) = self.tree.selected() else {
            return;
        };
        if node.kind != NodeKind::File {
            return;
        }
        let Some(rel) = self.rel(&node.path) else {
            return;
        };
        // git prints paths with forward slashes on every platform; normalize the native path.
        let needle = rel.to_string_lossy().replace('\\', "/");
        let lines = self.content_plain_lines();
        if let Some(idx) = lines.iter().position(|l| l.contains(&needle)) {
            let row = self.content_row_of_line(idx + 1);
            self.content_scroll =
                (row.min(u16::MAX as usize) as u16).min(self.max_content_scroll());
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
            commit_short: self
                .commit
                .as_ref()
                .map(|c| c.sha.chars().take(7).collect()),
        })
    }

    /// Dispatch the unified-commit-view render for the active commit: the whole-commit patch
    /// fetched by the worker (off the input thread) and rendered by the diff delegate. Mirrors
    /// [`dispatch_render`](Self::dispatch_render)'s view-state reset, which routes here while
    /// commit mode is active.
    pub(super) fn dispatch_commit_render(&mut self, sha: String) {
        let seq = self.latest_seq;
        if self
            .job_tx
            .send(RenderJob {
                seq,
                path: self.root.clone(),
                rel: None,
                mode: ViewMode::Diff,
                baseline: self.baseline,
                is_git: self.is_git_repo,
                directory_diff: false,
                commit_sha: Some(sha),
                wrap_width: None,
            })
            .is_ok()
        {
            self.content = Text::raw("Rendering\u{2026}");
            self.content_notices.clear();
            self.content_source = None;
            self.content_rendering = true;
        }
    }
}

impl CommitState {
    /// The commit's changed-file set (repo-root-relative), for tree re-scoping on refresh.
    pub(super) fn files(&self) -> &BTreeMap<PathBuf, Status> {
        &self.files
    }
}

/// A tiny adapter so `commit_mode()` can expose `Option<&str>` without cloning.
trait AsDerefSha {
    fn as_deref_sha(&self) -> Option<&str>;
}
impl AsDerefSha for Option<CommitState> {
    fn as_deref_sha(&self) -> Option<&str> {
        self.as_ref().map(|c| c.sha.as_str())
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
