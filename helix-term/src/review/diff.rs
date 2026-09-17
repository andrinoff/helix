//! Parser for unified diffs as produced by `gh pr diff`.
//!
//! The parser only needs to understand enough structure to render the diff
//! nicely and to map rendered lines back to `(file, side, line)` anchors for
//! review comments. It intentionally ignores git plumbing headers such as
//! `index`, `mode`, and `similarity` lines.

/// Kind of a single diff line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiffLineKind {
    /// Unchanged context line (` ` prefix).
    Context,
    /// Added line (`+` prefix).
    Add,
    /// Removed line (`-` prefix).
    Del,
}

/// A single line of a hunk with its line numbers on both sides of the diff.
#[derive(Debug, Clone)]
pub struct PrDiffLine {
    pub kind: DiffLineKind,
    /// The line content without the leading `+`/`-`/` ` prefix.
    pub text: String,
    /// Line number in the old (base) side, if this line exists there.
    pub old_line: Option<u32>,
    /// Line number in the new (head) side, if this line exists there.
    pub new_line: Option<u32>,
}

/// A `@@ -a,b +c,d @@ section` hunk.
#[derive(Debug, Clone)]
pub struct PrHunk {
    pub old_start: u32,
    pub old_count: u32,
    pub new_start: u32,
    pub new_count: u32,
    /// The trailing section header (usually a function name), if any.
    pub section: String,
    pub lines: Vec<PrDiffLine>,
}

/// Status of a file within the PR.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileStatus {
    Added,
    Modified,
    Removed,
    Renamed,
    Binary,
}

impl FileStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            FileStatus::Added => "added",
            FileStatus::Modified => "modified",
            FileStatus::Removed => "removed",
            FileStatus::Renamed => "renamed",
            FileStatus::Binary => "binary",
        }
    }
}

/// The diff of a single file.
#[derive(Debug, Clone)]
pub struct PrFileDiff {
    pub status: FileStatus,
    /// Path in the base branch, or `/dev/null` for new files.
    pub old_path: String,
    /// Path in the head branch, or `/dev/null` for deleted files.
    pub new_path: String,
    pub hunks: Vec<PrHunk>,
}

/// A parsed PR diff.
#[derive(Debug, Clone, Default)]
pub struct PrDiff {
    pub files: Vec<PrFileDiff>,
}

/// Parse unified diff text into a [`PrDiff`].
pub fn parse_unified_diff(input: &str) -> PrDiff {
    let mut diff = PrDiff::default();
    let mut current: Option<PrFileDiff> = None;
    let mut old_line: u32 = 0;
    let mut new_line: u32 = 0;

    for line in input.lines() {
        let line = line.strip_suffix('\r').unwrap_or(line);
        if line.starts_with("diff --git ") {
            if let Some(file) = current.take() {
                diff.files.push(file);
            }
            current = Some(PrFileDiff {
                status: FileStatus::Modified,
                old_path: String::new(),
                new_path: String::new(),
                hunks: Vec::new(),
            });
            old_line = 0;
            new_line = 0;
            continue;
        }

        let Some(file) = current.as_mut() else {
            continue;
        };

        if line == "new file mode" || line.starts_with("new file mode ") {
            file.status = FileStatus::Added;
            continue;
        }
        if line == "deleted file mode" || line.starts_with("deleted file mode ") {
            file.status = FileStatus::Removed;
            continue;
        }
        if line.starts_with("rename from ") || line.starts_with("copy from ") {
            file.status = FileStatus::Renamed;
            if file.old_path.is_empty() {
                file.old_path = line
                    .split_once(' ')
                    .map(|(_, p)| p)
                    .unwrap_or_default()
                    .to_string();
            }
            continue;
        }
        if line.starts_with("rename to ") || line.starts_with("copy to ") {
            file.status = FileStatus::Renamed;
            if file.new_path.is_empty() {
                file.new_path = line
                    .split_once(' ')
                    .map(|(_, p)| p)
                    .unwrap_or_default()
                    .to_string();
            }
            continue;
        }
        if line.starts_with("Binary files ") || line == "GIT binary patch" {
            file.status = FileStatus::Binary;
            continue;
        }
        if let Some(path) = line.strip_prefix("--- ") {
            file.old_path = parse_diff_path(path);
            continue;
        }
        if let Some(path) = line.strip_prefix("+++ ") {
            file.new_path = parse_diff_path(path);
            continue;
        }
        if let Some(rest) = line.strip_prefix("@@ ") {
            let Some((old, rest)) = rest.split_once(' ') else {
                continue;
            };
            let Some((new, rest)) = rest.split_once(' ') else {
                continue;
            };
            let (old_start, old_count) = parse_range(old);
            let (new_start, new_count) = parse_range(new);
            let section = rest
                .strip_prefix("@@")
                .and_then(|s| s.strip_prefix(' '))
                .unwrap_or("")
                .to_string();
            if !rest.starts_with("@@") {
                continue; // malformed header, ignore
            }
            old_line = old_start;
            new_line = new_start;
            file.hunks.push(PrHunk {
                old_start,
                old_count,
                new_start,
                new_count,
                section,
                lines: Vec::new(),
            });
            continue;
        }

        // Lines inside a hunk, or uninteresting plumbing that we skip.
        let Some(hunk) = file.hunks.last_mut() else {
            continue;
        };
        let (kind, text, old_no, new_no) = if let Some(text) = line.strip_prefix('+') {
            (DiffLineKind::Add, text, None, Some(new_line))
        } else if let Some(text) = line.strip_prefix('-') {
            (DiffLineKind::Del, text, Some(old_line), None)
        } else if let Some(text) = line.strip_prefix(' ') {
            (DiffLineKind::Context, text, Some(old_line), Some(new_line))
        } else {
            // `\ No newline at end of file`, `index`, `mode`, ... markers
            continue;
        };
        match kind {
            DiffLineKind::Add => new_line += 1,
            DiffLineKind::Del => old_line += 1,
            DiffLineKind::Context => {
                old_line += 1;
                new_line += 1;
            }
        }
        hunk.lines.push(PrDiffLine {
            kind,
            text: text.to_string(),
            old_line: old_no,
            new_line: new_no,
        });
    }

    if let Some(file) = current.take() {
        diff.files.push(file);
    }

    // Drop plumbing-only entries (mode changes and the like) that carry no
    // reviewable content.
    diff.files.retain(|file| {
        file.status == FileStatus::Binary
            || file.status == FileStatus::Added
            || file.status == FileStatus::Removed
            || file.status == FileStatus::Renamed
            || !file.hunks.is_empty()
    });
    diff
}

/// Parse a `--- a/path` / `+++ b/path` argument: strip quoting first (git
/// quotes the whole path, prefix included) and then the `a/`/`b/` prefix.
fn parse_diff_path(path: &str) -> String {
    if path == "/dev/null" {
        return path.to_string();
    }
    let unquoted = unquote_path(path);
    unquoted
        .strip_prefix("a/")
        .or_else(|| unquoted.strip_prefix("b/"))
        .unwrap_or(&unquoted)
        .to_string()
}

/// Parse `-a` or `-a,b` / `+c` or `+c,d` (git omits the count when it is 1)
/// into `(start, count)`.
fn parse_range(range: &str) -> (u32, u32) {
    let range = range
        .strip_prefix('-')
        .or_else(|| range.strip_prefix('+'))
        .unwrap_or(range);
    match range.split_once(',') {
        Some((start, count)) => (start.parse().unwrap_or(1), count.parse().unwrap_or(1)),
        None => (range.parse().unwrap_or(1), 1),
    }
}

/// Undo git's C-style quoting of paths containing special characters.
fn unquote_path(s: &str) -> String {
    if s.len() < 2 || !s.starts_with('"') || !s.ends_with('"') {
        return s.to_string();
    }
    let inner = &s[1..s.len() - 1];
    let mut out = String::with_capacity(inner.len());
    let mut chars = inner.chars();
    while let Some(c) = chars.next() {
        match c {
            '\\' => match chars.next() {
                Some('n') => out.push('\n'),
                Some('t') => out.push('\t'),
                Some('r') => out.push('\r'),
                Some('a') => out.push('\x07'),
                Some('b') => out.push('\x08'),
                Some('f') => out.push('\x0c'),
                Some('v') => out.push('\x0b'),
                Some('"') => out.push('"'),
                Some('\\') => out.push('\\'),
                Some(first @ '0'..='7') => {
                    let mut value = first as u32 - '0' as u32;
                    for _ in 0..2 {
                        match chars.next() {
                            Some(d @ '0'..='7') => value = value * 8 + (d as u32 - '0' as u32),
                            _ => break,
                        }
                    }
                    out.push(char::from_u32(value).unwrap_or('\u{fffd}'));
                }
                Some(c) => out.push(c),
                None => out.push('\\'),
            },
            c => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_modified_file() {
        let input = "\
diff --git a/src/main.rs b/src/main.rs
index 1111111..2222222 100644
--- a/src/main.rs
+++ b/src/main.rs
@@ -12,7 +12,9 @@ fn main() {
-    let x = parse(input);
+    let x = parse(input)?;
     handle(x);
 }
\\ No newline at end of file
";
        let diff = parse_unified_diff(input);
        assert_eq!(diff.files.len(), 1);
        let file = &diff.files[0];
        assert_eq!(file.status, FileStatus::Modified);
        assert_eq!(file.old_path, "src/main.rs");
        assert_eq!(file.new_path, "src/main.rs");
        assert_eq!(file.hunks.len(), 1);
        let hunk = &file.hunks[0];
        assert_eq!((hunk.old_start, hunk.old_count), (12, 7));
        assert_eq!((hunk.new_start, hunk.new_count), (12, 9));
        assert_eq!(hunk.section, "fn main() {");
        assert_eq!(hunk.lines.len(), 4);
        assert_eq!(hunk.lines[0].kind, DiffLineKind::Del);
        assert_eq!(hunk.lines[0].text, "    let x = parse(input);");
        assert_eq!(hunk.lines[0].old_line, Some(12));
        assert_eq!(hunk.lines[0].new_line, None);
        assert_eq!(hunk.lines[1].kind, DiffLineKind::Add);
        assert_eq!(hunk.lines[1].text, "    let x = parse(input)?;");
        assert_eq!(hunk.lines[1].old_line, None);
        assert_eq!(hunk.lines[1].new_line, Some(12));
        assert_eq!(hunk.lines[2].kind, DiffLineKind::Context);
        assert_eq!(hunk.lines[2].text, "    handle(x);");
        assert_eq!(hunk.lines[2].old_line, Some(13));
        assert_eq!(hunk.lines[2].new_line, Some(13));
    }

    #[test]
    fn parses_new_and_deleted_files() {
        let input = "\
diff --git a/README.md b/README.md
new file mode 100644
index 0000000..3333333
--- /dev/null
+++ b/README.md
@@ -0,0 +1,2 @@
+hello
+world
diff --git a/old.txt b/old.txt
deleted file mode 100644
index 4444444..0000000
--- a/old.txt
+++ /dev/null
@@ -1,3 +0,0 @@
-gone
-byebye
";
        let diff = parse_unified_diff(input);
        assert_eq!(diff.files.len(), 2);
        let added = &diff.files[0];
        assert_eq!(added.status, FileStatus::Added);
        assert_eq!(added.old_path, "/dev/null");
        assert_eq!(added.new_path, "README.md");
        assert_eq!(added.hunks[0].lines[0].new_line, Some(1));
        let deleted = &diff.files[1];
        assert_eq!(deleted.status, FileStatus::Removed);
        assert_eq!(deleted.old_path, "old.txt");
        assert_eq!(deleted.new_path, "/dev/null");
        assert_eq!(deleted.hunks[0].lines[1].old_line, Some(2));
        assert_eq!(deleted.hunks[0].lines[1].new_line, None);
    }

    #[test]
    fn parses_renames_and_binary() {
        let input = "\
diff --git a/one.txt b/two.txt
similarity index 90%
rename from one.txt
rename to two.txt
index 5555555..6666666 100644
--- a/one.txt
+++ b/two.txt
@@ -1 +1 @@
-x
+y
diff --git a/image.png b/image.png
index 7777777..8888888 100644
Binary files a/image.png and b/image.png differ
";
        let diff = parse_unified_diff(input);
        assert_eq!(diff.files.len(), 2);
        assert_eq!(diff.files[0].status, FileStatus::Renamed);
        assert_eq!(diff.files[0].old_path, "one.txt");
        assert_eq!(diff.files[0].new_path, "two.txt");
        assert_eq!(diff.files[1].status, FileStatus::Binary);
        assert!(diff.files[1].hunks.is_empty());
    }

    #[test]
    fn parses_hunk_without_counts() {
        let input = "\
diff --git a/a.txt b/a.txt
--- a/a.txt
+++ b/a.txt
@@ -1 +1,3 @@
-a
+b
+c
";
        let diff = parse_unified_diff(input);
        let hunk = &diff.files[0].hunks[0];
        assert_eq!((hunk.old_start, hunk.old_count), (1, 1));
        assert_eq!((hunk.new_start, hunk.new_count), (1, 3));
    }

    #[test]
    fn unquotes_paths() {
        let input = "\
diff --git a/a b/space.txt b/a b/space.txt
--- \"a/a b/space.txt\"
+++ \"b/a b/space.txt\"
@@ -1 +1 @@
-x
+y
diff --git a/escaped.txt b/escaped.txt
--- \"a/esc\\341pe.txt\"
+++ \"b/esc\\341pe.txt\"
@@ -1 +1 @@
-x
+y
";
        let diff = parse_unified_diff(input);
        assert_eq!(diff.files[0].old_path, "a b/space.txt");
        assert_eq!(diff.files[0].new_path, "a b/space.txt");
        assert_eq!(diff.files[1].old_path, "escápe.txt");
        assert_eq!(diff.files[1].new_path, "escápe.txt");
    }

    #[test]
    fn drops_plumbing_only_files() {
        let input = "\
diff --git a/exec.sh b/exec.sh
old mode 100644
new mode 100755
diff --git a/real.txt b/real.txt
--- a/real.txt
+++ b/real.txt
@@ -1 +1 @@
-x
+y
";
        let diff = parse_unified_diff(input);
        assert_eq!(diff.files.len(), 1);
        assert_eq!(diff.files[0].new_path, "real.txt");
        assert_eq!(diff.files[0].hunks.len(), 1);
    }

    #[test]
    fn empty_input() {
        let diff = parse_unified_diff("");
        assert_eq!(diff.files.len(), 0);
        assert_eq!(diff.files.iter().flat_map(|f| f.hunks.iter()).count(), 0);
    }
}
