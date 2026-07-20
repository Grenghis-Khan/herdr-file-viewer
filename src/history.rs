//! History Service — read-only git commit-graph queries for the source-control section.
//!
//! Delegates the DAG drawing to git itself: `git log --graph --color=always` with the commit
//! SHA embedded in the format string behind unit-separator (`\x1f`) markers, so each output
//! row maps back to its commit without hand-implementing graph layout. Pure edge rows (graph
//! lines with no commit) carry no SHA. Every invocation goes through the shared hardened
//! command builder ([`crate::git::git_command`]) and uses only read-only subcommands (AC-N2);
//! any git failure degrades to an empty result so the graph section just shows nothing
//! (AC-26).

use crate::git::git_command;
use std::path::Path;

/// The unit separator wrapping the embedded SHA in the `git log` format string. Chosen because
/// it cannot appear in a commit subject rendered by `%s` (git strips control bytes from
/// re-encoded subjects, and even a raw one cannot collide: we split on the FIRST pair).
const SEP: char = '\u{1f}';

/// How many commits one page of the graph loads; scrolling near the end loads another page.
pub const PAGE: usize = 200;

/// One row of the commit graph: the ANSI-styled display line (graph edges + decorated
/// subject) and the full SHA of the commit on this row — `None` for a pure edge row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitRow {
    pub sha: Option<String>,
    /// The ANSI display line, with the embedded SHA markers stripped. Untrusted (subjects and
    /// ref names are repo-controlled); the presenter ingests it through the same
    /// escape-neutralizing `ansi-to-tui` path as every renderer output (AC-27).
    pub line: String,
}

/// A commit id we are willing to pass back to git: pure hex, bounded length. Every sha the
/// viewer forwards to git came out of [`log_graph`]'s own parse, but re-validating at the
/// boundary means an untrusted or corrupted value can never become an argv item
/// (defense-in-depth, mirroring `git::is_safe_ref`).
pub fn is_valid_sha(s: &str) -> bool {
    (4..=64).contains(&s.len()) && s.chars().all(|c| c.is_ascii_hexdigit())
}

/// One page of the commit graph, `skip` commits in: `git log --graph` rows in display order.
/// `all` widens the walk from HEAD to every ref (`--all`). Fewer than `limit` rows back means
/// the history is exhausted. Not a repo / git failure → empty (AC-26).
pub fn log_graph(repo_root: &Path, all: bool, skip: usize, limit: usize) -> Vec<CommitRow> {
    let count = format!("-n{limit}");
    let skip = format!("--skip={skip}");
    // %x1f%H%x1f wraps the full SHA so the parser can cut it back out of the display line;
    // %C(auto) colors the short hash + decorations exactly as `git log --oneline` would.
    let format = "--format=%x1f%H%x1f%C(auto)%h%d%C(reset) %s";
    let mut args = vec![
        "log",
        "--graph",
        "--color=always",
        format,
        count.as_str(),
        skip.as_str(),
    ];
    if all {
        args.push("--all");
    }
    let out = git_command(repo_root, &args)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
        .unwrap_or_default();
    out.lines().map(parse_graph_line).collect()
}

/// Split one `git log --graph` output line into its display text and embedded SHA. The line is
/// `<graph prefix>\x1f<sha>\x1f<decorated subject>` for a commit row, or bare graph edges for a
/// continuation row (no format expansion → no markers → no SHA). A malformed line (one marker,
/// non-hex sha) degrades to a plain display row, never a panic.
fn parse_graph_line(line: &str) -> CommitRow {
    let Some(first) = line.find(SEP) else {
        return CommitRow {
            sha: None,
            line: line.to_string(),
        };
    };
    let (prefix, rest) = line.split_at(first);
    let rest = &rest[SEP.len_utf8()..];
    let Some(second) = rest.find(SEP) else {
        return CommitRow {
            sha: None,
            line: line.to_string(),
        };
    };
    let (sha, suffix) = rest.split_at(second);
    let suffix = &suffix[SEP.len_utf8()..];
    let sha = sha.to_string();
    let display = format!("{prefix}{suffix}");
    CommitRow {
        sha: is_valid_sha(&sha).then_some(sha),
        line: display,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_graph_line_extracts_sha_and_strips_markers() {
        let line = "* \u{1f}0123456789abcdef0123456789abcdef01234567\u{1f}\u{1b}[33m0123456\u{1b}[m subject";
        let row = parse_graph_line(line);
        assert_eq!(
            row.sha.as_deref(),
            Some("0123456789abcdef0123456789abcdef01234567")
        );
        assert_eq!(row.line, "* \u{1b}[33m0123456\u{1b}[m subject");
    }

    #[test]
    fn parse_graph_line_edge_row_has_no_sha() {
        // A continuation row: pure graph edges, no format expansion, no markers.
        let row = parse_graph_line("|\\");
        assert_eq!(row.sha, None);
        assert_eq!(row.line, "|\\");
    }

    #[test]
    fn parse_graph_line_malformed_markers_degrade_to_plain_row() {
        // One marker only → no SHA, the line displays as-is (no panic, no truncation).
        let row = parse_graph_line("* \u{1f}abcd123");
        assert_eq!(row.sha, None);
        assert_eq!(row.line, "* \u{1f}abcd123");
        // Non-hex between markers → markers stripped, sha rejected.
        let row = parse_graph_line("* \u{1f}not-hex!\u{1f} subject");
        assert_eq!(row.sha, None);
        assert_eq!(row.line, "*  subject");
    }

    #[test]
    fn is_valid_sha_accepts_hex_and_rejects_argv_shaped_values() {
        assert!(is_valid_sha("0123456789abcdef0123456789abcdef01234567"));
        assert!(is_valid_sha("abcd123")); // short form
        assert!(!is_valid_sha("--all")); // option injection
        assert!(!is_valid_sha("HEAD")); // not hex
        assert!(!is_valid_sha("abc")); // too short
        assert!(!is_valid_sha("")); // empty
    }
}
