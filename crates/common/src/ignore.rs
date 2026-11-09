//! Ignore files (`.thecaignore`): gitignore patterns naming what is never
//! uploaded from their folder and the folders below it, with git's rules
//! (`gitignore(5)`): the last matching line of a file wins, a deeper file wins
//! over a parent's, and nothing is re-included below an ignored folder. Names
//! are matched by their key, as Theca compares them (without case, in NFC).

use std::collections::HashMap;
use std::fmt;

use ignore::Match;
use ignore::gitignore::{Gitignore, GitignoreBuilder};

use crate::name::key;

/// An ignore file's name (compared like any name: `is_ignore_file`).
pub const FILE_NAME: &str = ".thecaignore";

/// How much of an ignore file applies: its beginning, up to its last complete
/// line (the server reads these files, and needs a bound).
pub const MAX_LEN: usize = 64 * 1024;

/// Whether `name` is an ignore file's.
pub fn is_ignore_file(name: &str) -> bool {
    key(name) == FILE_NAME
}

/// What an ignore file holds that doesn't apply: said to the person, the rest
/// applies.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Problem {
    /// A line that isn't a pattern (a glob the matcher refuses, bytes that
    /// aren't UTF-8), numbered from 1.
    Line {
        /// The line's number.
        line: usize,
        /// Why.
        reason: String,
    },
    /// Past `MAX_LEN`: only the lines before it apply.
    TooLong,
}

impl fmt::Display for Problem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Problem::Line { line, reason } => write!(f, "line {line} skipped: {reason}"),
            Problem::TooLong => write!(f, "only its first {} KiB apply", MAX_LEN / 1024),
        }
    }
}

/// The ignore files of a tree (a library, a sync pair's folder), each by its
/// folder's path from the tree's root.
#[derive(Debug, Default, Clone)]
pub struct Ignores {
    files: HashMap<Vec<String>, Gitignore>,
}

impl Ignores {
    /// No ignore file.
    pub fn new() -> Self {
        Self::default()
    }

    /// The ignore file of `folder` (its names from the root; empty for the
    /// root), replacing any it had: what in it doesn't apply.
    pub fn add<S: AsRef<str>>(&mut self, folder: &[S], content: &[u8]) -> Vec<Problem> {
        let (gitignore, problems) = parse(content);
        self.files.insert(keys(folder), gitignore);
        problems
    }

    /// Whether `path` (its names from the root) is ignored: under an ignored
    /// folder, or matched by the deepest ignore file above it that matches it.
    pub fn is_ignored<S: AsRef<str>>(&self, path: &[S], is_dir: bool) -> bool {
        if self.files.is_empty() || path.is_empty() {
            return false;
        }
        let path = keys(path);
        (1..path.len()).any(|end| self.decides(&path[..end], true)) || self.decides(&path, is_dir)
    }

    /// Whether the ignore files above `path` ignore it, its folders aside.
    fn decides(&self, path: &[String], is_dir: bool) -> bool {
        for depth in (0..path.len()).rev() {
            let Some(gitignore) = self.files.get(&path[..depth]) else {
                continue;
            };
            match gitignore.matched(path[depth..].join("/"), is_dir) {
                Match::Ignore(_) => return true,
                Match::Whitelist(_) => return false,
                Match::None => {}
            }
        }
        false
    }
}

fn keys<S: AsRef<str>>(names: &[S]) -> Vec<String> {
    names.iter().map(|n| key(n.as_ref())).collect()
}

/// An ignore file's patterns, by their key, and what was left out.
fn parse(content: &[u8]) -> (Gitignore, Vec<Problem>) {
    let mut problems = Vec::new();
    let mut content = content;
    // The bound counts every byte read (a byte-order mark too): a line is
    // kept if it ends within it, its `\n` the byte after at most.
    if content.len() > MAX_LEN {
        let cut = content[..=MAX_LEN]
            .iter()
            .rposition(|&b| b == b'\n')
            .unwrap_or(0);
        content = &content[..cut];
        problems.push(Problem::TooLong);
    }
    let content = content.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(content);
    let mut builder = GitignoreBuilder::new("");
    for (i, line) in content.split(|&b| b == b'\n').enumerate() {
        // As git does: a line ending in CR LF is the line without the CR.
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        let line = match std::str::from_utf8(line) {
            Ok(line) => line,
            Err(_) => {
                problems.push(Problem::Line {
                    line: i + 1,
                    reason: "not UTF-8".into(),
                });
                continue;
            }
        };
        if let Err(e) = builder.add_line(None, &literal_braces(&key(line))) {
            problems.push(Problem::Line {
                line: i + 1,
                reason: e.to_string(),
            });
        }
    }
    let gitignore = builder.build().unwrap_or_else(|_| Gitignore::empty());
    // The last problem is TooLong's place: it reads best last.
    problems.sort_by_key(|p| matches!(p, Problem::TooLong));
    (gitignore, problems)
}

/// `{` and `}` as git reads them, literally: the matcher would expand `{a,b}`
/// (checked with git: `{a,b}.txt` matches only itself). Escaped, or inside a
/// class, they stay as written.
fn literal_braces(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut chars = line.chars();
    let mut class = false;
    while let Some(c) = chars.next() {
        match c {
            '\\' => {
                out.push(c);
                out.extend(chars.next());
            }
            '[' if !class => {
                class = true;
                out.push(c);
            }
            ']' if class => {
                class = false;
                out.push(c);
            }
            '{' | '}' if !class => {
                out.push('[');
                out.push(c);
                out.push(']');
            }
            c => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tree(files: &[(&str, &str)]) -> Ignores {
        let mut ignores = Ignores::new();
        for (folder, content) in files {
            let folder: Vec<&str> = folder.split('/').filter(|n| !n.is_empty()).collect();
            assert_eq!(ignores.add(&folder, content.as_bytes()), vec![]);
        }
        ignores
    }

    fn ignored(ignores: &Ignores, path: &str) -> bool {
        let is_dir = path.ends_with('/');
        let names: Vec<&str> = path.split('/').filter(|n| !n.is_empty()).collect();
        ignores.is_ignored(&names, is_dir)
    }

    /// Each case as `git check-ignore` answered it (git 2.47, 2026-11-09):
    /// the same files in a repository, each path asked.
    fn check(files: &[(&str, &str)], cases: &[(&str, bool)]) {
        let ignores = tree(files);
        for &(path, expected) in cases {
            assert_eq!(ignored(&ignores, path), expected, "{path}");
        }
    }

    #[test]
    fn git_s_patterns() {
        check(
            &[(
                "",
                "*.log\n/root.txt\nbuild/\ndocs/*.tmp\n**/cache\n# a comment\n\\#hash\n",
            )],
            &[
                ("a.log", true),
                ("x/y/a.log", true),
                ("root.txt", true),
                ("x/root.txt", false),
                ("build/", true),
                ("build/out.o", true),
                ("x/build/out.o", true),
                // A trailing / matches folders only.
                ("build", false),
                ("docs/a.tmp", true),
                ("docs/x/a.tmp", false),
                ("x/docs/a.tmp", false),
                ("cache", true),
                ("a/b/cache/", true),
                ("# a comment", false),
                ("#hash", true),
                ("a.txt", false),
            ],
        );
    }

    #[test]
    fn the_last_line_wins_and_negation_re_includes() {
        check(
            &[("", "*.log\n!keep.log\n")],
            &[("a.log", true), ("keep.log", false), ("x/keep.log", false)],
        );
        check(&[("", "!keep.log\n*.log\n")], &[("keep.log", true)]);
    }

    #[test]
    fn a_deeper_file_wins_over_a_parent_s() {
        check(
            &[("", "*.log\n"), ("a", "!keep.log\n")],
            &[
                ("a/keep.log", false),
                ("a/x.log", true),
                ("a/b/keep.log", false),
                ("keep.log", true),
                ("b/keep.log", true),
            ],
        );
        // And the other way round: a deeper file ignoring what a parent's
        // re-included.
        check(
            &[("", "*.log\n!keep.log\n"), ("a", "keep.log\n")],
            &[("a/keep.log", true), ("keep.log", false)],
        );
    }

    #[test]
    fn nothing_is_re_included_below_an_ignored_folder() {
        check(
            &[("", "build/\n"), ("build", "!z.txt\n")],
            &[("build/z.txt", true), ("build/.thecaignore", true)],
        );
        check(&[("", "build/\n!build/z.txt\n")], &[("build/z.txt", true)]);
    }

    #[test]
    fn patterns_are_relative_to_their_folder() {
        check(
            &[("a", "/x.txt\nb/c.txt\n")],
            &[
                ("a/x.txt", true),
                ("x.txt", false),
                ("a/y/x.txt", false),
                ("a/b/c.txt", true),
                ("a/z/b/c.txt", false),
            ],
        );
    }

    #[test]
    fn an_ignore_file_can_ignore_itself_and_still_applies() {
        check(
            &[("", ".thecaignore\n*.log\n")],
            &[(".thecaignore", true), ("a.log", true)],
        );
    }

    /// As Theca compares names, not as git does (case-sensitive by default).
    #[test]
    fn names_match_by_their_key() {
        check(
            &[("", "Node_Modules/\n*.LOG\ncafe\u{301}.txt\n")],
            &[
                ("node_modules/", true),
                ("NODE_MODULES/x.js", true),
                ("a.log", true),
                ("A.Log", true),
                // NFC: é composed matches é decomposed, both ways.
                ("caf\u{e9}.txt", true),
                ("CAFE\u{301}.TXT", true),
            ],
        );
        check(&[("Src", "*.o\n")], &[("src/a.o", true), ("SRC/A.O", true)]);
    }

    #[test]
    fn crlf_and_a_byte_order_mark_are_read_as_git_and_editors_do() {
        check(
            &[("", "\u{feff}*.log\r\nbuild/\r\n")],
            &[("a.log", true), ("build/x", true)],
        );
    }

    #[test]
    fn malformed_lines_are_skipped() {
        let mut ignores = Ignores::new();
        let problems = ignores.add(&[] as &[&str], b"*.log\n[z-a]\n\xff\xfe\nbuild/\nend\\\n");
        assert_eq!(problems.len(), 3, "{problems:?}");
        assert!(
            matches!(&problems[0], Problem::Line { line: 2, .. }),
            "{problems:?}"
        );
        assert_eq!(
            problems[1],
            Problem::Line {
                line: 3,
                reason: "not UTF-8".into()
            }
        );
        assert!(
            matches!(&problems[2], Problem::Line { line: 5, .. }),
            "{problems:?}"
        );
        assert!(ignored(&ignores, "a.log"));
        assert!(ignored(&ignores, "build/x"));
    }

    #[test]
    fn braces_are_literal_as_in_git() {
        check(
            &[("", "{a,b}.txt\n[{]x[}]\n\\{y\\}\n")],
            &[
                ("{a,b}.txt", true),
                ("a.txt", false),
                ("{x}", true),
                ("{y}", true),
            ],
        );
    }

    #[test]
    fn only_the_first_64_kib_apply_up_to_a_line() {
        let mut content = "*.log\n".to_owned();
        while content.len() < MAX_LEN - 3 {
            content.push_str("#\n");
        }
        // A line crossing the limit is cut whole, and what follows too.
        content.push_str("*.tmp\n*.bak\n");
        assert!(content.len() > MAX_LEN);
        let mut ignores = Ignores::new();
        assert_eq!(
            ignores.add(&[] as &[&str], content.as_bytes()),
            vec![Problem::TooLong]
        );
        assert!(ignored(&ignores, "a.log"));
        assert!(!ignored(&ignores, "a.tmp"));
        assert!(!ignored(&ignores, "a.bak"));
    }

    /// The bound counts a byte-order mark too, and keeps a line whose `\n` is
    /// the byte right after it: every reader keeps `MAX_LEN + 1` bytes.
    #[test]
    fn the_64_kib_bound_counts_every_byte_read() {
        let pad = |len: usize| {
            let mut content = b"\xEF\xBB\xBF*.log\n".to_vec();
            // Whole lines: `#` comments, then an empty line if one byte is left.
            while len - content.len() >= 2 {
                content.extend_from_slice(b"#\n");
            }
            if content.len() < len {
                content.push(b'\n');
            }
            content
        };
        // A BOM, then past the bound: cut, and said.
        let mut long = pad(MAX_LEN - 1);
        long.extend_from_slice(b"node_modules/\n");
        let mut ignores = Ignores::new();
        let problems = ignores.add(&[] as &[&str], &long[..MAX_LEN + 1]);
        assert_eq!(problems, vec![Problem::TooLong]);
        assert!(ignored(&ignores, "a.log"));
        assert!(!ignored(&ignores, "node_mo/"), "never a half line");
        // A line ending at the bound, its `\n` the byte after: kept.
        let mut exact = pad(MAX_LEN - 6);
        exact.extend_from_slice(b"*.tmp\n");
        assert_eq!(exact.len(), MAX_LEN);
        exact.extend_from_slice(b"*.bak\n");
        let mut ignores = Ignores::new();
        ignores.add(&[] as &[&str], &exact[..MAX_LEN + 1]);
        assert!(ignored(&ignores, "a.tmp"));
        assert!(!ignored(&ignores, "a.bak"));
    }

    #[test]
    fn a_later_add_replaces_the_folder_s_file() {
        let mut ignores = tree(&[("a", "*.log\n")]);
        ignores.add(&["a"], b"*.tmp\n");
        assert!(!ignored(&ignores, "a/x.log"));
        assert!(ignored(&ignores, "a/x.tmp"));
    }

    #[test]
    fn nothing_is_ignored_without_ignore_files() {
        let ignores = Ignores::new();
        assert!(!ignored(&ignores, "a.log"));
        assert!(!tree(&[("", "*.log\n")]).is_ignored(&[] as &[&str], true));
    }

    #[test]
    fn ignore_files_are_named_without_case() {
        assert!(is_ignore_file(".thecaignore"));
        assert!(is_ignore_file(".TheCaIgnore"));
        assert!(!is_ignore_file("thecaignore"));
        assert!(!is_ignore_file(".thecaignore.txt"));
    }
}
