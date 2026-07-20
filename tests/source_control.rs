//! Hermetic controller tests for the source-control graph section (`g`): paging/selection, the
//! three-region focus cycle, all-branches toggle, resize clamps, and the focus-gated commit-id
//! copy. All stubs — no real git, no external renderers.

use herdr_file_viewer::controller::{
    Clipboard, Components, ContentProvider, Controller, EditorHandoff, EditorOutcome, GitService,
    RenderResult, RootProviders,
};
use herdr_file_viewer::git::{Baseline, Status};
use herdr_file_viewer::history::CommitRow;
use herdr_file_viewer::intent::Intent;
use herdr_file_viewer::presenter::Focus;
use herdr_file_viewer::root::Resolved;
use herdr_file_viewer::view_policy::ViewMode;
use ratatui::text::Text;
use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

const SHA_A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const SHA_B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

/// A git stub with a two-commit history (one page, so the second page marks it exhausted).
struct StubGit;
impl GitService for StubGit {
    fn status(&self) -> BTreeMap<PathBuf, Status> {
        BTreeMap::new()
    }
    fn changed_set(&self, _baseline: Baseline) -> BTreeMap<PathBuf, Status> {
        BTreeMap::new()
    }
    fn diff(&self, _rel: &Path, _baseline: Baseline, _full: bool) -> String {
        String::new()
    }
    fn diff_directory(&self, _rel_dir: &Path, _baseline: Baseline) -> String {
        String::new()
    }
    fn log_graph(&self, _all: bool, skip: usize, _limit: usize) -> Vec<CommitRow> {
        if skip > 0 {
            return Vec::new(); // one page of history — a short page marks it exhausted
        }
        vec![
            CommitRow {
                sha: Some(SHA_A.into()),
                line: "* aaaaaaa first".into(),
            },
            CommitRow {
                sha: None,
                line: "|".into(), // a pure edge row (no sha)
            },
            CommitRow {
                sha: Some(SHA_B.into()),
                line: "* bbbbbbb second".into(),
            },
        ]
    }
}

/// Content stub: a render echoes the raw diff text verbatim (enough for the controller to wire
/// up; the graph-section tests here never assert on rendered content).
struct StubContent;
impl ContentProvider for StubContent {
    fn render(&self, _path: &Path, _mode: ViewMode, raw_diff: Option<&str>) -> RenderResult {
        RenderResult {
            content: Text::raw(raw_diff.unwrap_or("").to_string()),
            notices: Vec::new(),
            source: None,
        }
    }
}

struct StubEditor;
impl EditorHandoff for StubEditor {
    fn open(&mut self, _file: &Path) -> EditorOutcome {
        EditorOutcome::NoTakeover
    }
}

/// A clipboard stub recording every copy, so the sha-copy tests can assert the payload.
#[derive(Clone, Default)]
struct RecordingClipboard(Arc<Mutex<Vec<String>>>);
impl Clipboard for RecordingClipboard {
    fn copy(&mut self, text: &str) -> io::Result<()> {
        self.0.lock().unwrap().push(text.to_string());
        Ok(())
    }
}

/// A controller over a real (empty-ish) temp dir, marked as a git repo, with the stub history.
fn controller_with(clip: RecordingClipboard) -> Controller {
    let root = std::env::temp_dir().join(format!(
        "hfv-srcctl-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    let _ = std::fs::create_dir_all(root.join("sub"));
    let _ = std::fs::write(root.join("a.txt"), "one\n");
    let _ = std::fs::write(root.join("sub/b.txt"), "two\n");
    let resolved = Resolved {
        repo_root: Some(root.clone()),
        root,
        is_git_repo: true,
        is_worktree: false,
        base_branch: None,
    };
    let components = Components {
        providers: Box::new(move |_r: &Resolved| RootProviders {
            git: Arc::new(StubGit),
            content: Box::new(StubContent),
        }),
        editor: Box::new(StubEditor),
        clipboard: Box::new(clip),
        renderers: None,
    };
    Controller::new(resolved, Baseline::Head, components)
}

fn controller() -> Controller {
    controller_with(RecordingClipboard::default())
}

#[test]
fn g_toggles_the_graph_section_and_focuses_it() {
    let mut ctrl = controller();
    assert!(ctrl.view_state().graph.is_none(), "hidden at launch");

    ctrl.handle(Intent::ToggleGraph);
    let view = ctrl.view_state();
    let graph = view.graph.expect("graph section open after g");
    assert_eq!(graph.lines.len(), 3, "the stub page's rows are loaded");
    assert_eq!(view.focus, Focus::Graph, "opening focuses the graph");

    ctrl.handle(Intent::ToggleGraph);
    let view = ctrl.view_state();
    assert!(view.graph.is_none(), "a second g closes the section");
    assert_eq!(view.focus, Focus::Tree, "focus returns to the tree");
}

#[test]
fn tab_cycles_three_regions_while_the_graph_is_open() {
    let mut ctrl = controller();
    ctrl.handle(Intent::ToggleGraph); // focus = Graph
    ctrl.handle(Intent::ToggleFocus);
    assert_eq!(ctrl.view_state().focus, Focus::Content, "graph → content");
    ctrl.handle(Intent::ToggleFocus);
    assert_eq!(ctrl.view_state().focus, Focus::Tree, "content → tree");
    ctrl.handle(Intent::ToggleFocus);
    assert_eq!(ctrl.view_state().focus, Focus::Graph, "tree → graph");
}

#[test]
fn y_copies_the_short_sha_and_shift_y_the_full_sha_when_graph_focused() {
    let clip = RecordingClipboard::default();
    let mut ctrl = controller_with(clip.clone());
    ctrl.handle(Intent::ToggleGraph); // focus = Graph, cursor 0 = SHA_A
    ctrl.handle(Intent::CopyRepoPath); // y
    ctrl.handle(Intent::CopyAbsPath); // Y
    let copied = clip.0.lock().unwrap().clone();
    assert_eq!(copied, vec!["aaaaaaa".to_string(), SHA_A.to_string()]);
}

#[test]
fn all_branches_toggle_reloads_and_edge_cases_stay_inert_when_hidden() {
    let mut ctrl = controller();
    // Hidden: B, {, } are inert no-ops.
    for intent in [
        Intent::ToggleAllBranches,
        Intent::ShrinkGraph,
        Intent::GrowGraph,
    ] {
        let fx = ctrl.handle(intent);
        assert!(!fx.redraw && !fx.quit, "{intent:?} is inert while hidden");
    }
    // Open: B reloads (the stub returns the same page) and the title reflects the scope.
    ctrl.handle(Intent::ToggleGraph);
    assert_eq!(ctrl.view_state().graph.unwrap().title, "History");
    ctrl.handle(Intent::ToggleAllBranches);
    assert_eq!(ctrl.view_state().graph.unwrap().title, "History · all");
}

#[test]
fn graph_resize_clamps_between_bounds() {
    let mut ctrl = controller();
    ctrl.handle(Intent::ToggleGraph);
    for _ in 0..20 {
        ctrl.handle(Intent::GrowGraph);
    }
    assert_eq!(
        ctrl.view_state().graph_pct,
        herdr_file_viewer::presenter::GRAPH_PCT_MAX,
        "grow clamps at the max"
    );
    for _ in 0..20 {
        ctrl.handle(Intent::ShrinkGraph);
    }
    assert_eq!(
        ctrl.view_state().graph_pct,
        herdr_file_viewer::presenter::GRAPH_PCT_MIN,
        "shrink clamps at the min"
    );
}
