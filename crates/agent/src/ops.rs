use std::fs::{self, File, Metadata, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};
use yonder_proto::{
    content_hash, ContentHasher, Entry, EntryKind, Error, ErrorKind, FileStat, HelloInfo, Op,
    Reply, PROTOCOL_VERSION,
};

pub fn handle(op: Op) -> Result<Reply, Error> {
    match op {
        Op::Hello => Ok(Reply::Hello(hello())),
        Op::Resolve { path } => resolve(&path),
        Op::ListDir { path } => list_dir(Path::new(&path)).map(Reply::Entries),
        Op::Stat { path } => Ok(Reply::Stat(stat_of(&fs::metadata(path)?))),
        Op::ReadLink { path } => Ok(Reply::Path {
            is_dir: false,
            path: fs::read_link(path)?.to_string_lossy().into_owned(),
        }),
        Op::ReadFile { path, max_bytes } => read_file(Path::new(&path), max_bytes),
        Op::WriteFile {
            path,
            data,
            expected_hash,
        } => write_file(Path::new(&path), &data, expected_hash),
        Op::Git {
            cwd,
            args,
            max_bytes,
        } => run_git(Path::new(&cwd), &args, max_bytes),
        other => Err(Error::new(
            ErrorKind::Other,
            format!("not a file request: {other:?}"),
        )),
    }
}

fn home() -> String {
    std::env::var("HOME").unwrap_or_else(|_| "/".into())
}

fn hello() -> HelloInfo {
    let mut buf = [0u8; 256];
    let hostname = if unsafe { libc::gethostname(buf.as_mut_ptr().cast(), buf.len()) } == 0 {
        let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
        String::from_utf8_lossy(&buf[..end]).into_owned()
    } else {
        String::new()
    };
    HelloInfo {
        agent_version: env!("CARGO_PKG_VERSION").into(),
        protocol: PROTOCOL_VERSION,
        hostname,
        home: home(),
        pid: std::process::id(),
    }
}

fn resolve(path: &str) -> Result<Reply, Error> {
    let home = PathBuf::from(home());
    let expanded = if path.is_empty() || path == "~" {
        home
    } else if let Some(rest) = path.strip_prefix("~/") {
        home.join(rest)
    } else if path.starts_with('/') {
        PathBuf::from(path)
    } else {
        home.join(path)
    };
    let canonical = fs::canonicalize(&expanded).map_err(|e| {
        let message = format!("{}: {e}", expanded.display());
        Error::new(Error::from(e).kind, message)
    })?;
    Ok(Reply::Path {
        is_dir: canonical.is_dir(),
        path: canonical.to_string_lossy().into_owned(),
    })
}

fn stat_of(m: &Metadata) -> FileStat {
    FileStat {
        size: m.len(),
        mtime_s: m.mtime(),
        mtime_ns: m.mtime_nsec() as u32,
    }
}

fn list_dir(dir: &Path) -> Result<Vec<Entry>, Error> {
    let mut out = Vec::new();
    for item in fs::read_dir(dir)? {
        let item = item?;
        // Paths travel as UTF-8. A name that is not UTF-8 is still listed so
        // nothing hides, but as `Other`, since a path rebuilt from its lossy
        // form would not reach it.
        let (name, utf8) = match item.file_name().into_string() {
            Ok(name) => (name, true),
            Err(raw) => (raw.to_string_lossy().into_owned(), false),
        };
        let Ok(lmeta) = item.metadata() else {
            continue; // vanished between readdir and stat
        };
        let symlink = lmeta.file_type().is_symlink();
        let (kind, stat) = if !utf8 {
            (EntryKind::Other, stat_of(&lmeta))
        } else if symlink {
            match fs::metadata(item.path()) {
                Ok(m) => (kind_of(&m), stat_of(&m)),
                Err(_) => (EntryKind::BrokenLink, stat_of(&lmeta)),
            }
        } else {
            (kind_of(&lmeta), stat_of(&lmeta))
        };
        out.push(Entry {
            name,
            kind,
            symlink,
            stat,
        });
    }
    Ok(out)
}

fn kind_of(m: &Metadata) -> EntryKind {
    if m.is_dir() {
        EntryKind::Dir
    } else if m.is_file() {
        EntryKind::File
    } else {
        EntryKind::Other
    }
}

fn read_file(path: &Path, max_bytes: u64) -> Result<Reply, Error> {
    let file = File::open(path)?;
    let meta = file.metadata()?;
    if meta.is_dir() {
        return Err(Error::new(ErrorKind::IsDirectory, "is a directory"));
    }
    if meta.len() > max_bytes {
        return Err(too_large(meta.len(), max_bytes));
    }
    let mut data = Vec::with_capacity(meta.len() as usize);
    // The file may grow while we read; never take more than the limit.
    file.take(max_bytes + 1).read_to_end(&mut data)?;
    if data.len() as u64 > max_bytes {
        return Err(too_large(data.len() as u64, max_bytes));
    }
    Ok(Reply::File {
        hash: content_hash(&data),
        data,
        stat: stat_of(&meta),
    })
}

fn too_large(size: u64, limit: u64) -> Error {
    Error::new(
        ErrorKind::TooLarge,
        format!("file is {size} bytes; the limit is {limit} bytes"),
    )
}

fn write_file(path: &Path, data: &[u8], expected_hash: Option<u64>) -> Result<Reply, Error> {
    // Replace a symlink's target, not the link itself.
    let target = match fs::canonicalize(path) {
        Ok(t) => t,
        Err(e) if e.kind() == io::ErrorKind::NotFound => path.to_path_buf(),
        Err(e) => return Err(e.into()),
    };
    let existing = match fs::metadata(&target) {
        Ok(m) => Some(m),
        Err(e) if e.kind() == io::ErrorKind::NotFound => None,
        Err(e) => return Err(e.into()),
    };
    if existing.as_ref().is_some_and(|m| m.is_dir()) {
        return Err(Error::new(ErrorKind::IsDirectory, "is a directory"));
    }
    // Run as late as possible: right before the new content replaces the old.
    let unchanged = || -> Result<(), Error> {
        let Some(expected) = expected_hash else {
            return Ok(());
        };
        match file_hash(&target) {
            Ok(h) if h == expected => Ok(()),
            Ok(_) => Err(Error::new(
                ErrorKind::Conflict,
                "the file changed on the remote since it was opened",
            )),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Err(Error::new(
                ErrorKind::Conflict,
                "the file was deleted on the remote since it was opened",
            )),
            Err(e) => Err(e.into()),
        }
    };

    // Write to a temporary file and rename it over the original, so a crash
    // or a full disk never leaves a half-written file. Fall back to writing in
    // place when renaming would break something: hard links, a file owned by
    // someone else (group-shared project directories), or a directory we may
    // not create files in.
    let replaced = match &existing {
        Some(m) if m.nlink() > 1 => false,
        _ => match replace_atomically(&target, data, existing.as_ref(), &unchanged) {
            Ok(done) => done,
            Err(e) if e.kind == ErrorKind::PermissionDenied && existing.is_some() => false,
            Err(e) => return Err(e),
        },
    };
    if !replaced {
        unchanged()?;
        write_in_place(&target, data)?;
    }
    Ok(Reply::Written {
        hash: content_hash(data),
        stat: stat_of(&fs::metadata(&target)?),
    })
}

/// Hash a file without holding it all in memory; it may have grown a lot
/// since it was opened.
fn file_hash(path: &Path) -> io::Result<u64> {
    let mut f = File::open(path)?;
    let mut h = ContentHasher::default();
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        match f.read(&mut buf)? {
            0 => return Ok(h.finish()),
            n => h.update(&buf[..n]),
        }
    }
}

/// Returns `Ok(false)` if the replacement would change the file's owner.
/// `unchanged` is checked just before the rename.
fn replace_atomically(
    target: &Path,
    data: &[u8],
    orig: Option<&Metadata>,
    unchanged: &dyn Fn() -> Result<(), Error>,
) -> Result<bool, Error> {
    let dir = target.parent().unwrap_or(Path::new("/"));
    let name = target
        .file_name()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "path has no file name"))?
        .to_string_lossy();
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    let tmp = dir.join(format!(".{name}.yonder-{}-{nanos}.tmp", std::process::id()));

    let mut f = OpenOptions::new().write(true).create_new(true).open(&tmp)?;
    let result = (|| -> Result<bool, Error> {
        if let Some(m) = orig {
            let t = f.metadata()?;
            if t.uid() != m.uid() || t.gid() != m.gid() {
                return Ok(false);
            }
        }
        f.write_all(data)?;
        // After writing: a write clears setuid and setgid bits.
        if let Some(m) = orig {
            f.set_permissions(m.permissions())?;
        }
        f.sync_all()?;
        unchanged()?;
        fs::rename(&tmp, target)?;
        Ok(true)
    })();
    if !matches!(result, Ok(true)) {
        let _ = fs::remove_file(&tmp);
    }
    result
}

fn write_in_place(target: &Path, data: &[u8]) -> io::Result<()> {
    let mut f = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(target)?;
    f.write_all(data)?;
    f.sync_all()
}

/// Run git without locks, prompts, pagers or colour, keeping at most
/// `max_bytes` of its output.
fn run_git(cwd: &Path, args: &[String], max_bytes: u64) -> Result<Reply, Error> {
    let mut child = std::process::Command::new("git")
        .args(["-c", "core.quotepath=off", "-c", "color.ui=false"])
        .args(args)
        .current_dir(cwd)
        // `git status` would otherwise refresh the index, a write that can
        // collide with the user's own git commands.
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_PAGER", "cat")
        .env("LC_ALL", "C")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| {
            if e.kind() == io::ErrorKind::NotFound {
                Error::new(ErrorKind::NotFound, "git is not installed on the remote")
            } else {
                Error::from(e)
            }
        })?;
    let mut stderr = child.stderr.take().unwrap();
    let errors = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = (&mut stderr).take(64 * 1024).read_to_end(&mut buf);
        String::from_utf8_lossy(&buf).into_owned()
    });
    let mut stdout = Vec::new();
    child
        .stdout
        .take()
        .unwrap()
        .take(max_bytes + 1)
        .read_to_end(&mut stdout)?;
    let truncated = stdout.len() as u64 > max_bytes;
    if truncated {
        stdout.truncate(max_bytes as usize);
        let _ = child.kill();
    }
    let status = child.wait()?;
    Ok(Reply::Output {
        code: status.code(),
        stdout,
        stderr: errors.join().unwrap_or_default(),
        truncated,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::{symlink, PermissionsExt};

    fn written_hash(r: Result<Reply, Error>) -> u64 {
        match r.unwrap() {
            Reply::Written { hash, .. } => hash,
            other => panic!("unexpected reply {other:?}"),
        }
    }

    fn read(path: &Path) -> (Vec<u8>, u64) {
        match read_file(path, 1 << 20).unwrap() {
            Reply::File { data, hash, .. } => (data, hash),
            other => panic!("unexpected reply {other:?}"),
        }
    }

    #[test]
    fn write_then_read() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("a.txt");
        let h = written_hash(write_file(&p, b"hello", None));
        assert_eq!(read(&p), (b"hello".to_vec(), h));
        // No temporary files left behind.
        assert_eq!(fs::read_dir(d.path()).unwrap().count(), 1);
    }

    #[test]
    fn conflict_when_changed_underneath() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("a.txt");
        fs::write(&p, "v1").unwrap();
        let (_, h) = read(&p);
        fs::write(&p, "v2 from a batch job").unwrap();
        let err = write_file(&p, b"mine", Some(h)).unwrap_err();
        assert_eq!(err.kind, ErrorKind::Conflict);
        assert_eq!(fs::read(&p).unwrap(), b"v2 from a batch job");
    }

    #[test]
    fn conflict_when_deleted() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("a.txt");
        let err = write_file(&p, b"x", Some(content_hash(b"old"))).unwrap_err();
        assert_eq!(err.kind, ErrorKind::Conflict);
    }

    #[test]
    fn matching_hash_saves() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("a.txt");
        fs::write(&p, "v1").unwrap();
        let (_, h) = read(&p);
        written_hash(write_file(&p, b"v2", Some(h)));
        assert_eq!(fs::read(&p).unwrap(), b"v2");
    }

    #[test]
    fn keeps_permissions() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("run.sh");
        fs::write(&p, "#!/bin/sh\n").unwrap();
        fs::set_permissions(&p, fs::Permissions::from_mode(0o750)).unwrap();
        write_file(&p, b"#!/bin/sh\necho hi\n", None).unwrap();
        assert_eq!(
            fs::metadata(&p).unwrap().permissions().mode() & 0o777,
            0o750
        );
    }

    #[test]
    fn writes_through_symlink() {
        let d = tempfile::tempdir().unwrap();
        let real = d.path().join("real.txt");
        let link = d.path().join("link.txt");
        fs::write(&real, "old").unwrap();
        symlink(&real, &link).unwrap();
        write_file(&link, b"new", None).unwrap();
        assert!(fs::symlink_metadata(&link)
            .unwrap()
            .file_type()
            .is_symlink());
        assert_eq!(fs::read(&real).unwrap(), b"new");
    }

    #[test]
    fn keeps_hard_links_intact() {
        let d = tempfile::tempdir().unwrap();
        let a = d.path().join("a");
        let b = d.path().join("b");
        fs::write(&a, "old").unwrap();
        fs::hard_link(&a, &b).unwrap();
        write_file(&a, b"new", None).unwrap();
        assert_eq!(fs::read(&b).unwrap(), b"new");
    }

    #[test]
    fn keeps_setgid_bit() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("tool");
        fs::write(&p, "old").unwrap();
        fs::set_permissions(&p, fs::Permissions::from_mode(0o2755)).unwrap();
        write_file(&p, b"new", None).unwrap();
        assert_eq!(
            fs::metadata(&p).unwrap().permissions().mode() & 0o7777,
            0o2755
        );
    }

    #[test]
    fn conflict_leaves_no_temporary_file() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("a.txt");
        fs::write(&p, "theirs").unwrap();
        let err = write_file(&p, b"mine", Some(content_hash(b"old"))).unwrap_err();
        assert_eq!(err.kind, ErrorKind::Conflict);
        assert_eq!(fs::read_dir(d.path()).unwrap().count(), 1);
    }

    #[test]
    // APFS refuses names that are not UTF-8.
    #[cfg(target_os = "linux")]
    fn non_utf8_names_are_listed_as_other() {
        use std::os::unix::ffi::OsStrExt;
        let d = tempfile::tempdir().unwrap();
        fs::write(d.path().join(std::ffi::OsStr::from_bytes(b"bad\xff")), "x").unwrap();
        let entries = list_dir(d.path()).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].kind, EntryKind::Other);
    }

    #[test]
    fn stat_follows_symlinks() {
        let d = tempfile::tempdir().unwrap();
        fs::write(d.path().join("plot.png"), "12345").unwrap();
        symlink(d.path().join("plot.png"), d.path().join("latest.png")).unwrap();
        let op = Op::Stat {
            path: d.path().join("latest.png").to_string_lossy().into(),
        };
        let Ok(Reply::Stat(stat)) = handle(op) else {
            panic!()
        };
        assert_eq!(stat.size, 5);
        let op = Op::Stat {
            path: d.path().join("nope").to_string_lossy().into(),
        };
        assert_eq!(handle(op).unwrap_err().kind, ErrorKind::NotFound);
    }

    #[test]
    fn git_runs_and_caps_output() {
        let d = tempfile::tempdir().unwrap();
        let git = |args: &[&str], max| {
            let args = args.iter().map(|a| a.to_string()).collect::<Vec<_>>();
            match run_git(d.path(), &args, max).unwrap() {
                Reply::Output {
                    code,
                    stdout,
                    stderr,
                    truncated,
                } => (code, stdout, stderr, truncated),
                other => panic!("unexpected reply {other:?}"),
            }
        };
        let (code, _, stderr, _) = git(&["status"], 1 << 20);
        assert_eq!(code, Some(128), "{stderr}");
        assert!(stderr.contains("not a git repository"), "{stderr}");
        assert_eq!(git(&["init", "-q"], 1 << 20).0, Some(0));
        // Truncating kills git, so whether it left an exit code is a race.
        let (_, out, _, truncated) = git(&["--version"], 4);
        assert_eq!((out.as_slice(), truncated), (&b"git "[..], true));
    }

    #[test]
    fn read_link_returns_the_stored_target() {
        let d = tempfile::tempdir().unwrap();
        fs::write(d.path().join("data.txt"), "contents").unwrap();
        symlink("data.txt", d.path().join("link")).unwrap();
        let op = Op::ReadLink {
            path: d.path().join("link").to_string_lossy().into(),
        };
        assert_eq!(
            handle(op).unwrap(),
            Reply::Path {
                path: "data.txt".into(),
                is_dir: false
            }
        );
        let op = Op::ReadLink {
            path: d.path().join("data.txt").to_string_lossy().into(),
        };
        assert_eq!(handle(op).unwrap_err().kind, ErrorKind::Other);
    }

    #[test]
    fn read_limit() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("big");
        fs::write(&p, vec![b'x'; 100]).unwrap();
        let err = read_file(&p, 10).unwrap_err();
        assert_eq!(err.kind, ErrorKind::TooLarge);
    }

    #[test]
    fn list_dir_kinds() {
        let d = tempfile::tempdir().unwrap();
        fs::create_dir(d.path().join("sub")).unwrap();
        fs::write(d.path().join("f"), "x").unwrap();
        symlink(d.path().join("sub"), d.path().join("to_sub")).unwrap();
        symlink(d.path().join("missing"), d.path().join("dangling")).unwrap();
        let mut entries = list_dir(d.path()).unwrap();
        entries.sort_by(|a, b| a.name.cmp(&b.name));
        let got: Vec<_> = entries
            .iter()
            .map(|e| (e.name.as_str(), e.kind, e.symlink))
            .collect();
        assert_eq!(
            got,
            vec![
                ("dangling", EntryKind::BrokenLink, true),
                ("f", EntryKind::File, false),
                ("sub", EntryKind::Dir, false),
                ("to_sub", EntryKind::Dir, true),
            ]
        );
    }

    #[test]
    fn resolve_relative_to_home() {
        let home = home();
        match resolve("~").unwrap() {
            Reply::Path { path, is_dir } => {
                assert!(is_dir);
                assert_eq!(PathBuf::from(path), fs::canonicalize(home).unwrap());
            }
            other => panic!("unexpected reply {other:?}"),
        }
        assert_eq!(
            resolve("/definitely/not/here").unwrap_err().kind,
            ErrorKind::NotFound
        );
    }
}
