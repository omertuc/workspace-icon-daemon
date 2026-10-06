//! The job in the foreground of a terminal window.
//!
//! A terminal process may host several windows and tabs (daemon modes such
//! as `footserver` or `alacritty msg create-window`, and single-instance
//! terminals like kitty, Ghostty or Ptyxis), each with a shell of
//! its own on a pseudo-terminal. The window manager only knows the process,
//! so the session shown is told apart by the window's title.

use std::path::PathBuf;
use std::time::SystemTime;

/// How deep below the terminal process to look for sessions; Ptyxis, for
/// one, starts its shells from a helper process.
const SESSION_DEPTH: usize = 2;
const PTS_MAJOR_FIRST: u64 = 136;
const PTS_MAJOR_LAST: u64 = 143;

/// The fields of `/proc/<pid>/stat` used here.
struct Stat {
    session: i64,
    tty: u64,
    foreground: i64,
}

fn stat(pid: i64) -> Option<Stat> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // comm may contain spaces and parentheses; fields resume after the last
    // ')': state, ppid, pgrp, session, tty_nr, tpgid.
    let mut fields = stat.get(stat.rfind(')')? + 2..)?.split_whitespace().skip(3);
    Some(Stat {
        session: fields.next()?.parse().ok()?,
        tty: fields.next()?.parse().ok()?,
        foreground: fields.next()?.parse().ok()?,
    })
}

fn children(pid: i64) -> Vec<i64> {
    let Ok(tasks) = std::fs::read_dir(format!("/proc/{pid}/task")) else {
        return Vec::new();
    };
    let mut tasks: Vec<u64> = tasks
        .flatten()
        .filter_map(|e| e.file_name().to_str()?.parse().ok())
        .collect();
    tasks.sort_unstable();
    tasks
        .iter()
        .flat_map(|task| {
            std::fs::read_to_string(format!("/proc/{pid}/task/{task}/children"))
                .unwrap_or_default()
                .split_whitespace()
                .filter_map(|c| c.parse().ok())
                .collect::<Vec<_>>()
        })
        .collect()
}

/// A shell (or other program) a terminal started on a pseudo-terminal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Session {
    /// Name of the foreground job, or None at the shell's prompt.
    pub job: Option<String>,
    /// The foreground job's command line, its arguments joined by spaces.
    pub command: String,
    /// The foreground job's working directory.
    pub cwd: Option<PathBuf>,
    /// When the terminal last read input from the user.
    pub last_input: Option<SystemTime>,
}

impl Session {
    fn read(leader: i64, stat: &Stat) -> Self {
        let foreground = if stat.foreground > 0 {
            stat.foreground
        } else {
            leader
        };
        let job = (foreground != leader)
            .then(|| std::fs::read_to_string(format!("/proc/{foreground}/comm")).ok())
            .flatten()
            .map(|comm| comm.trim().to_string())
            .filter(|comm| !comm.is_empty());
        let command = std::fs::read(format!("/proc/{foreground}/cmdline"))
            .map(|argv| {
                String::from_utf8_lossy(&argv)
                    .split('\0')
                    .filter(|arg| !arg.is_empty())
                    .collect::<Vec<_>>()
                    .join(" ")
            })
            .unwrap_or_default();
        let cwd = std::fs::read_link(format!("/proc/{foreground}/cwd")).ok();
        let last_input = pts_path(stat.tty)
            .and_then(|path| std::fs::metadata(path).ok())
            .and_then(|meta| meta.accessed().ok());
        Self {
            job,
            command,
            cwd,
            last_input,
        }
    }
}

/// The `/dev/pts` device of a `tty_nr`, if it is a pseudo-terminal.
fn pts_path(tty: u64) -> Option<PathBuf> {
    let major = (tty >> 8) & 0xfff;
    let minor = (tty & 0xff) | ((tty >> 12) & 0xfff00);
    (PTS_MAJOR_FIRST..=PTS_MAJOR_LAST)
        .contains(&major)
        .then(|| {
            PathBuf::from(format!(
                "/dev/pts/{}",
                (major - PTS_MAJOR_FIRST) * 256 + minor
            ))
        })
}

/// Sessions a terminal process started, oldest first.
pub fn sessions(terminal: i64) -> Vec<Session> {
    let mut sessions = Vec::new();
    let mut level = vec![terminal];
    for _ in 0..SESSION_DEPTH {
        let mut next = Vec::new();
        for pid in level.iter().flat_map(|&pid| children(pid)) {
            match stat(pid) {
                Some(stat) if stat.session == pid && stat.tty != 0 => {
                    sessions.push(Session::read(pid, &stat));
                }
                Some(_) => next.push(pid),
                None => {}
            }
        }
        level = next;
    }
    sessions
}

/// Whether `text` contains `word` with no letters or digits joined on.
fn contains_word(text: &str, word: &str) -> bool {
    !word.is_empty()
        && text.match_indices(word).any(|(at, _)| {
            let before = text[..at].chars().next_back();
            let after = text[at + word.len()..].chars().next();
            !before.is_some_and(char::is_alphanumeric) && !after.is_some_and(char::is_alphanumeric)
        })
}

/// How well a window title fits a session. Titles name the running command,
/// or the directory at a shell's prompt (and some shells keep showing it
/// while a job runs). `marks` are title prefixes a job uses instead of its
/// name, like a spinner.
fn score(session: &Session, title: &str, home: Option<&str>, marks: &[char]) -> u32 {
    let title = title.to_lowercase();
    let at_prompt = session.job.is_none();
    let mut score = 0;
    if title.chars().next().is_some_and(|c| marks.contains(&c)) {
        score += 8;
    }
    if contains_word(&title, &session.command.to_lowercase()) {
        score += 6;
    }
    if let Some(job) = &session.job
        && contains_word(&title, &job.to_lowercase())
    {
        score += 4;
    }
    if let Some(cwd) = session.cwd.as_ref().and_then(|cwd| cwd.to_str()) {
        let short = home
            .and_then(|home| cwd.strip_prefix(home))
            .filter(|rest| rest.is_empty() || rest.starts_with('/'))
            .map(|rest| format!("~{rest}"));
        let base = cwd.rsplit('/').next().unwrap_or_default();
        if [Some(cwd), short.as_deref()]
            .into_iter()
            .flatten()
            .any(|path| contains_word(&title, &path.to_lowercase()))
        {
            score += if at_prompt { 4 } else { 2 };
        } else if base.len() > 1 && contains_word(&title, &base.to_lowercase()) {
            score += u32::from(at_prompt) + 1;
        }
    }
    score
}

/// The session a window with this title shows: the best fit for the title,
/// or else the one most recently typed in.
pub fn shown_session<'a>(
    sessions: &'a [Session],
    title: &str,
    marks: impl Fn(&str) -> &'static [char],
) -> Option<&'a Session> {
    if let [only] = sessions {
        return Some(only);
    }
    let home = std::env::var("HOME").ok();
    sessions.iter().max_by_key(|session| {
        let marks = session.job.as_deref().map_or(&[][..], &marks);
        (
            score(session, title, home.as_deref(), marks),
            session.last_input,
        )
    })
}

/// Name of the job in the foreground of a terminal window, if any.
pub fn foreground_job(
    terminal: i64,
    title: &str,
    marks: impl Fn(&str) -> &'static [char],
) -> Option<String> {
    shown_session(&sessions(terminal), title, marks)?
        .job
        .clone()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session(job: Option<&str>, command: &str, cwd: &str, input: u64) -> Session {
        Session {
            job: job.map(str::to_string),
            command: command.to_string(),
            cwd: Some(PathBuf::from(cwd)),
            last_input: Some(SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(input)),
        }
    }

    fn marks(job: &str) -> &'static [char] {
        match job {
            "claude" => &['✳', '◐'],
            _ => &[],
        }
    }

    fn shown(sessions: &[Session], title: &str) -> Option<String> {
        shown_session(sessions, title, marks)?.job.clone()
    }

    #[test]
    fn picks_the_session_the_title_names() {
        let home = std::env::var("HOME").unwrap();
        let sessions = [
            session(None, "zsh", &format!("{home}/repos/a"), 3),
            session(
                Some("nvim"),
                "nvim README.md",
                &format!("{home}/repos/b"),
                2,
            ),
            session(Some("claude"), "claude", &format!("{home}/repos/c"), 1),
            session(None, "zsh", &format!("{home}/repos/d"), 0),
        ];
        assert_eq!(shown(&sessions, "nvim README.md").as_deref(), Some("nvim"));
        assert_eq!(shown(&sessions, "◐ Fix the bug").as_deref(), Some("claude"));
        assert_eq!(shown(&sessions, "✳ Claude Code").as_deref(), Some("claude"));
        assert_eq!(shown(&sessions, "omer@host:~/repos/a"), None);
        // The prompt in ~/repos/d, not the one typed in last.
        assert_eq!(
            shown_session(&sessions, "omer@host:~/repos/d", marks).map(|s| s.last_input),
            Some(sessions[3].last_input)
        );
    }

    #[test]
    fn a_prompt_wins_over_a_job_in_the_same_directory() {
        let sessions = [
            session(None, "zsh", "/srv/app", 0),
            session(Some("less"), "less notes", "/srv/app", 9),
        ];
        assert_eq!(shown(&sessions, "user@host:/srv/app"), None);
        // Titles that keep the prompt's directory while a job runs.
        let sessions = [
            session(None, "zsh", "/srv/app", 9),
            session(Some("htop"), "htop", "/tmp", 0),
        ];
        assert_eq!(shown(&sessions, "user@host:/tmp").as_deref(), Some("htop"));
    }

    #[test]
    fn names_match_whole_words() {
        let sessions = [
            session(Some("top"), "top", "/", 9),
            session(Some("htop"), "htop", "/", 0),
        ];
        assert_eq!(shown(&sessions, "htop").as_deref(), Some("htop"));
    }

    #[test]
    fn falls_back_to_the_session_typed_in_last() {
        let sessions = [
            session(Some("htop"), "htop", "/", 1),
            session(Some("nvim"), "nvim", "/", 5),
        ];
        assert_eq!(shown(&sessions, "Terminal").as_deref(), Some("nvim"));
    }

    #[test]
    fn a_lone_session_needs_no_title() {
        let sessions = [session(Some("nvim"), "nvim", "/", 0)];
        assert_eq!(shown(&sessions, "").as_deref(), Some("nvim"));
    }

    #[test]
    fn pseudo_terminal_paths() {
        assert_eq!(pts_path(136 << 8 | 3), Some(PathBuf::from("/dev/pts/3")));
        assert_eq!(
            pts_path((137 << 8) | 4),
            Some(PathBuf::from("/dev/pts/260"))
        );
        assert_eq!(pts_path(4 << 8 | 1), None); // tty1
    }
}
