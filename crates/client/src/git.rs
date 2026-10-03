//! Git, read-only: what changed, the history, and file contents for diffs.
//!
//! Every function runs `git` on the remote through the agent and parses its
//! machine-readable output. Nothing here writes to the repository.

use crate::Connection;
use serde::Serialize;
use yonder_proto::{Error, ErrorKind, Op, Reply};

/// Output larger than this is cut off; enough for any listing we show.
const MAX_LISTING: u64 = 32 << 20;

/// The largest file a diff shows, matching the editor's limit.
pub const MAX_DIFF_FILE: u64 = 16 << 20;

struct Output {
    code: Option<i32>,
    stdout: Vec<u8>,
    stderr: String,
    /// The output hit the limit; git was stopped part way.
    truncated: bool,
}

async fn git(conn: &Connection, cwd: &str, args: &[&str], max: u64) -> Result<Output, Error> {
    let op = Op::Git {
        cwd: cwd.into(),
        args: args.iter().map(|a| a.to_string()).collect(),
        max_bytes: max,
    };
    match conn.call(op).await? {
        Reply::Output {
            code,
            stdout,
            stderr,
            truncated,
        } => {
            if truncated && max == MAX_LISTING {
                return Err(Error::new(
                    ErrorKind::TooLarge,
                    "git printed too much to show",
                ));
            }
            Ok(Output {
                code,
                stdout,
                stderr,
                truncated,
            })
        }
        other => Err(Error::new(
            ErrorKind::Other,
            format!("unexpected reply {other:?}"),
        )),
    }
}

/// Run git and insist that it succeeds.
async fn git_ok(conn: &Connection, cwd: &str, args: &[&str]) -> Result<Vec<u8>, Error> {
    let out = git(conn, cwd, args, MAX_LISTING).await?;
    if out.code != Some(0) {
        let msg = out.stderr.trim();
        return Err(Error::new(
            ErrorKind::Other,
            if msg.is_empty() {
                format!("git {} failed", args[0])
            } else {
                msg.to_string()
            },
        ));
    }
    Ok(out.stdout)
}

/// The top of the repository containing `dir`, or `None` if it is not in one.
pub async fn repo_root(conn: &Connection, dir: &str) -> Result<Option<String>, Error> {
    let out = git(conn, dir, &["rev-parse", "--show-toplevel"], MAX_LISTING).await?;
    if out.code == Some(0) {
        Ok(Some(
            String::from_utf8_lossy(&out.stdout).trim().to_string(),
        ))
    } else if out.stderr.contains("not a git repository") {
        Ok(None)
    } else {
        Err(Error::new(ErrorKind::Other, out.stderr.trim()))
    }
}

#[derive(Serialize, Debug, Clone, PartialEq, Eq, Default)]
pub struct Status {
    /// `None` when HEAD is detached.
    pub branch: Option<String>,
    pub upstream: Option<String>,
    pub ahead: u32,
    pub behind: u32,
    pub files: Vec<Change>,
    /// Ignored paths, relative to the repository root. An ignored folder is
    /// listed once, ending in `/`, without its contents.
    pub ignored: Vec<String>,
}

#[derive(Serialize, Debug, Clone, PartialEq, Eq)]
pub struct Change {
    /// Relative to the repository root.
    pub path: String,
    /// The path before a rename or copy.
    pub old_path: Option<String>,
    /// One letter, as git shows it: M, A, D, R, C, U (conflict) or ? (untracked).
    pub status: char,
}

/// Uncommitted changes: staged and unstaged together, compared with HEAD.
pub async fn status(conn: &Connection, repo: &str) -> Result<Status, Error> {
    let out = git_ok(
        conn,
        repo,
        &[
            "status",
            "--porcelain=v2",
            "-z",
            "--branch",
            // Each untracked file, not just its folder, so every row opens.
            "--untracked-files=all",
            // Ignored folders as one entry each: never a walk of node_modules.
            "--ignored=matching",
        ],
    )
    .await?;
    Ok(parse_status(&out))
}

/// Parse `git status --porcelain=v2 -z --branch --ignored`.
pub fn parse_status(out: &[u8]) -> Status {
    let mut st = Status::default();
    let mut fields = out
        .split(|&b| b == 0)
        .map(|f| String::from_utf8_lossy(f).into_owned());
    while let Some(entry) = fields.next() {
        if let Some(header) = entry.strip_prefix("# ") {
            let (key, value) = header.split_once(' ').unwrap_or((header, ""));
            match key {
                "branch.head" if value != "(detached)" => st.branch = Some(value.into()),
                "branch.upstream" => st.upstream = Some(value.into()),
                "branch.ab" => {
                    for part in value.split(' ') {
                        if let Some(n) = part.strip_prefix('+') {
                            st.ahead = n.parse().unwrap_or(0);
                        } else if let Some(n) = part.strip_prefix('-') {
                            st.behind = n.parse().unwrap_or(0);
                        }
                    }
                }
                _ => {}
            }
            continue;
        }
        let kind = entry.chars().next().unwrap_or(' ');
        let change = match kind {
            // 1 XY sub mH mI mW hH hI path
            '1' => entry
                .splitn(9, ' ')
                .collect::<Vec<_>>()
                .get(8)
                .map(|path| Change {
                    status: status_letter(&entry[2..4]),
                    path: path.to_string(),
                    old_path: None,
                }),
            // 2 XY sub mH mI mW hH hI Xscore path NUL origPath
            '2' => {
                let path = entry.splitn(10, ' ').nth(9).map(str::to_string);
                let old = fields.next();
                path.map(|path| Change {
                    status: status_letter(&entry[2..4]),
                    path,
                    old_path: old,
                })
            }
            // u XY sub m1 m2 m3 mW h1 h2 h3 path
            'u' => entry.splitn(11, ' ').nth(10).map(|path| Change {
                status: 'U',
                path: path.to_string(),
                old_path: None,
            }),
            '!' => {
                st.ignored.push(entry[2..].to_string());
                None
            }
            '?' => Some(Change {
                status: '?',
                path: entry[2..].to_string(),
                old_path: None,
            }),
            _ => None,
        };
        st.files.extend(change);
    }
    st
}

/// One letter for an `XY` pair: what changed in the working tree if anything
/// did, otherwise what was staged.
fn status_letter(xy: &str) -> char {
    let mut c = xy.chars();
    let (x, y) = (c.next().unwrap_or('.'), c.next().unwrap_or('.'));
    let pick = if y != '.' { y } else { x };
    match pick {
        'T' => 'M',
        other => other,
    }
}

#[derive(Serialize, Debug, Clone, PartialEq, Eq)]
pub struct Commit {
    pub hash: String,
    pub short: String,
    pub author: String,
    /// Seconds since the epoch.
    pub time: i64,
    pub subject: String,
    /// More than one parent.
    pub merge: bool,
}

/// `limit` commits reachable from HEAD, newest first, after skipping `skip`.
pub async fn log(
    conn: &Connection,
    repo: &str,
    skip: u32,
    limit: u32,
) -> Result<Vec<Commit>, Error> {
    let skip = format!("--skip={skip}");
    let limit = format!("--max-count={limit}");
    let out = git(
        conn,
        repo,
        &[
            "log",
            "--format=%H%x1f%h%x1f%an%x1f%at%x1f%P%x1f%s%x1e",
            &skip,
            &limit,
        ],
        MAX_LISTING,
    )
    .await?;
    if out.code != Some(0) {
        // A repository without commits has no history yet.
        if out.stderr.contains("does not have any commits") {
            return Ok(Vec::new());
        }
        return Err(Error::new(ErrorKind::Other, out.stderr.trim()));
    }
    Ok(parse_log(&out.stdout))
}

pub fn parse_log(out: &[u8]) -> Vec<Commit> {
    String::from_utf8_lossy(out)
        .split('\x1e')
        .filter_map(|record| {
            let f: Vec<&str> = record.trim_start_matches('\n').split('\x1f').collect();
            if f.len() < 6 {
                return None;
            }
            Some(Commit {
                hash: f[0].into(),
                short: f[1].into(),
                author: f[2].into(),
                time: f[3].parse().unwrap_or(0),
                merge: f[4].split(' ').filter(|p| !p.is_empty()).count() > 1,
                subject: f[5..].join("\x1f"),
            })
        })
        .collect()
}

/// Tracked and untracked files under `dir`, relative to it, leaving out
/// ignored ones. `None` if `dir` is not in a repository.
pub async fn files(conn: &Connection, dir: &str) -> Result<Option<Vec<String>>, Error> {
    let args = [
        "ls-files",
        "-z",
        "--cached",
        "--others",
        "--exclude-standard",
    ];
    let out = git(conn, dir, &args, MAX_LISTING).await?;
    if out.code != Some(0) {
        if out.stderr.contains("not a git repository") {
            return Ok(None);
        }
        return Err(Error::new(ErrorKind::Other, out.stderr.trim()));
    }
    Ok(Some(
        out.stdout
            .split(|&b| b == 0)
            .filter(|f| !f.is_empty())
            .map(|f| String::from_utf8_lossy(f).into_owned())
            .collect(),
    ))
}

/// Search output past this is cut off; the results say so.
const MAX_SEARCH: u64 = 8 << 20;
/// At most this many matching lines are returned.
const MAX_MATCHES: usize = 20_000;

#[derive(Serialize, Debug, Clone, PartialEq, Eq)]
pub struct Match {
    /// Relative to the searched folder.
    pub path: String,
    pub line: u32,
    pub text: String,
}

#[derive(Serialize, Debug, Clone, PartialEq, Eq, Default)]
pub struct Found {
    pub matches: Vec<Match>,
    /// There were more matches than were returned.
    pub truncated: bool,
}

/// Lines matching `pattern` in the text files under `dir` that git does not
/// ignore. Works outside repositories too (`--no-index`).
pub async fn search(
    conn: &Connection,
    dir: &str,
    pattern: &str,
    regex: bool,
    case_sensitive: bool,
    word: bool,
) -> Result<Found, Error> {
    let mut args = vec!["grep", "--no-index", "--exclude-standard", "-n", "-z", "-I"];
    args.push(if regex { "-E" } else { "-F" });
    if !case_sensitive {
        args.push("-i");
    }
    if word {
        args.push("-w");
    }
    args.extend(["-e", pattern]);
    let out = git(conn, dir, &args, MAX_SEARCH).await?;
    // 1: nothing matched. A cut-off run was stopped, so it has no code.
    if !out.truncated && !matches!(out.code, Some(0 | 1)) {
        return Err(Error::new(ErrorKind::Other, out.stderr.trim()));
    }
    let mut found = parse_grep(&out.stdout);
    found.truncated |= out.truncated;
    Ok(found)
}

/// Parse `git grep -n -z`: `path\0line\0text\n` per match.
pub fn parse_grep(out: &[u8]) -> Found {
    let mut found = Found::default();
    for record in out.split(|&b| b == b'\n') {
        let mut f = record.splitn(3, |&b| b == 0);
        let (Some(path), Some(line), Some(text)) = (f.next(), f.next(), f.next()) else {
            continue; // the cut-off end
        };
        let Some(line) = std::str::from_utf8(line).ok().and_then(|l| l.parse().ok()) else {
            continue;
        };
        if found.matches.len() == MAX_MATCHES {
            found.truncated = true;
            break;
        }
        found.matches.push(Match {
            path: String::from_utf8_lossy(path).into_owned(),
            line,
            text: String::from_utf8_lossy(text).into_owned(),
        });
    }
    found
}

/// Files a commit changed, compared with its first parent.
pub async fn commit_files(conn: &Connection, repo: &str, hash: &str) -> Result<Vec<Change>, Error> {
    let out = git_ok(
        conn,
        repo,
        &[
            "diff-tree",
            "--no-commit-id",
            "-r",
            "-z",
            "-M",
            "--name-status",
            "--root",
            "-m",
            "--first-parent",
            hash,
        ],
    )
    .await?;
    Ok(parse_name_status(&out))
}

/// Parse `--name-status -z`: `M\0path\0`, or `R100\0old\0new\0` for renames.
pub fn parse_name_status(out: &[u8]) -> Vec<Change> {
    let mut fields = out
        .split(|&b| b == 0)
        .filter(|f| !f.is_empty())
        .map(|f| String::from_utf8_lossy(f).into_owned());
    let mut changes = Vec::new();
    while let Some(code) = fields.next() {
        let status = match code.chars().next() {
            Some('T') => 'M',
            Some(c) => c,
            None => continue,
        };
        let Some(first) = fields.next() else { break };
        if matches!(status, 'R' | 'C') {
            let Some(second) = fields.next() else { break };
            changes.push(Change {
                status,
                path: second,
                old_path: Some(first),
            });
        } else {
            changes.push(Change {
                status,
                path: first,
                old_path: None,
            });
        }
    }
    changes
}

/// A file's content at `rev` (`HEAD`, a hash, or `<hash>^` for its parent),
/// or `None` if it did not exist there. Fails with `TooLarge` past
/// [`MAX_DIFF_FILE`].
pub async fn file_at(
    conn: &Connection,
    repo: &str,
    rev: &str,
    path: &str,
) -> Result<Option<Vec<u8>>, Error> {
    let spec = format!("{rev}:{path}");
    let out = git(conn, repo, &["show", &spec], MAX_DIFF_FILE).await?;
    if out.truncated {
        return Err(Error::new(
            ErrorKind::TooLarge,
            "the file is too large to compare",
        ));
    }
    match out.code {
        Some(0) => Ok(Some(out.stdout)),
        // The path, or the parent commit, does not exist at that revision.
        Some(128) => Ok(None),
        _ => Err(Error::new(ErrorKind::Other, out.stderr.trim())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_entries() {
        let out = b"# branch.oid abc\0# branch.head main\0# branch.upstream origin/main\0# branch.ab +2 -1\0\
1 .M N... 100644 100644 100644 aaa aaa src/main.rs\0\
1 A. N... 000000 100644 100644 000 bbb new file.txt\0\
1 D. N... 100644 000000 000000 ccc 000 gone.txt\0\
2 R. N... 100644 100644 100644 ddd ddd R100 lib/new.rs\0lib/old.rs\0\
u UU N... 100644 100644 100644 100644 e1 e2 e3 both.txt\0\
? notes/todo.md\0! target/\0";
        let st = parse_status(out);
        assert_eq!(st.branch.as_deref(), Some("main"));
        assert_eq!(st.upstream.as_deref(), Some("origin/main"));
        assert_eq!((st.ahead, st.behind), (2, 1));
        let got: Vec<_> = st
            .files
            .iter()
            .map(|c| (c.status, c.path.as_str(), c.old_path.as_deref()))
            .collect();
        assert_eq!(
            got,
            vec![
                ('M', "src/main.rs", None),
                ('A', "new file.txt", None),
                ('D', "gone.txt", None),
                ('R', "lib/new.rs", Some("lib/old.rs")),
                ('U', "both.txt", None),
                ('?', "notes/todo.md", None),
            ]
        );
        assert_eq!(st.ignored, vec!["target/"]);
    }

    #[test]
    fn detached_head() {
        let st = parse_status(b"# branch.oid abc\0# branch.head (detached)\0");
        assert_eq!(st.branch, None);
        assert!(st.files.is_empty());
    }

    #[test]
    fn log_records() {
        let out = "aaaa\x1faa\x1fAda\x1f1700000000\x1fpppp\x1fFix the thing\x1e\n\
bbbb\x1fbb\x1fBo\x1f1690000000\x1fp1 p2\x1fMerge branch 'x'\x1e\n";
        let log = parse_log(out.as_bytes());
        assert_eq!(log.len(), 2);
        assert_eq!(log[0].subject, "Fix the thing");
        assert_eq!(log[0].time, 1_700_000_000);
        assert!(!log[0].merge);
        assert!(log[1].merge);
        assert_eq!(log[1].author, "Bo");
    }

    #[test]
    fn grep_matches() {
        let out = b"src/a.rs\x0012\x00    let x = 1;\nb c.txt\x003\x00x: y\x00z\nsrc/cut\x004";
        let found = parse_grep(out);
        let got: Vec<_> = found
            .matches
            .iter()
            .map(|m| (m.path.as_str(), m.line, m.text.as_str()))
            .collect();
        assert_eq!(
            got,
            vec![
                ("src/a.rs", 12, "    let x = 1;"),
                ("b c.txt", 3, "x: y\0z")
            ]
        );
        assert!(!found.truncated);
    }

    #[test]
    fn name_status() {
        let out = b"M\0a.txt\0R087\0old name.rs\0new name.rs\0A\0b.txt\0D\0c.txt\0";
        let got: Vec<_> = parse_name_status(out)
            .into_iter()
            .map(|c| (c.status, c.path, c.old_path))
            .collect();
        assert_eq!(
            got,
            vec![
                ('M', "a.txt".into(), None),
                ('R', "new name.rs".into(), Some("old name.rs".into())),
                ('A', "b.txt".into(), None),
                ('D', "c.txt".into(), None),
            ]
        );
    }
}
