use crate::config::MoMMIConfig;
use crate::github::data::{PullRequestAction, PullRequestEvent, PushEvent};
use crate::mommi::commloop;
use lazy_static::lazy_static;
use regex::{Regex, RegexBuilder};
use serde::de::{Error, MapAccess, Visitor};
use serde::ser::SerializeMap;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::fmt;
use std::fs::{create_dir_all, read_dir, File, OpenOptions};
use std::io::Write;
use std::path::Path;
use std::process::Command;
use std::sync::Arc;
use std::sync::{Mutex, MutexGuard};
use std::thread;
use std::time::{Duration, Instant};

const LOG_DIR: &str = "logs";
const LOG_FILE: &str = "logs/changelog.log";

fn get_timestamp() -> String {
    Command::new("date")
        .arg("+%Y-%m-%d %H:%M:%S")
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_else(|_| "???".to_string())
}

fn log_to_file(level: &str, message: &str) {
    let timestamp = get_timestamp();
    let line = format!("{} [{}] {}\n", timestamp, level, message);

    match level {
        "ERROR" | "WARNING" => eprint!("{}", line),
        _ => print!("{}", line),
    }

    let _ = create_dir_all(LOG_DIR);
    if let Ok(mut file) = OpenOptions::new()
        .create(true)
        .append(true)
        .open(LOG_FILE)
    {
        let _ = file.write_all(line.as_bytes());
    }
}

fn log_info(message: &str) {
    log_to_file("INFO", message);
}

fn log_warning(message: &str) {
    log_to_file("WARNING", message);
}

fn log_error(message: &str) {
    log_to_file("ERROR", message);
}

pub fn try_handle_changelog_pr(event: &PullRequestEvent, config: &Arc<MoMMIConfig>) {
    log_info(&format!("Received PR event: action={:?}, merged={}, PR#{}",
        event.action, event.pull_request.merged, event.number));

    if event.action != PullRequestAction::Closed
        || !event.pull_request.merged
        || !config.has_changelog_repo_path()
    {
        log_info("PR not a merge or no changelog repo configured, skipping");
        return;
    }

    if let Some(name) = config.get_changelog_repo_name() {
        if name != event.repository.full_name {
            return;
        }
    }

    let additions = parse_body_changelog(&event.pull_request.body);

    if additions.len() == 0 {
        log_info("No changelogs found in PR body");
        return;
    }

    log_info(&format!("Found {} changelogs in PR#{}", additions.len(), event.number));

    let changelog = Changelog {
        author: event.pull_request.user.login.clone(),
        changes: additions,
        delete_after: Some(true),
    };

    let mut changelog_path = config.get_changelog_repo_path().unwrap().to_path_buf();
    changelog_path.push(&format!("html/changelogs/PR-{}-temp.yml", event.number));

    match write_temp_changelog(&changelog_path, changelog) {
        Err(e) => log_error(&format!("Error writing changelog temp file: {:?}", e)),
        _ => log_info(&format!("Wrote temp changelog for PR-{}", event.number)),
    };

    process_changelogs(config);
}

pub fn try_handle_changelog_push(event: &PushEvent, config: &Arc<MoMMIConfig>) {
    if let Some(name) = config.get_changelog_repo_name() {
        if name != event.repository.full_name {
            return;
        }
    }

    lazy_static! {
        static ref IS_CHANGELOG_RE: Regex = Regex::new(r#"^html/changelogs/[^.].*\.yml$"#).unwrap();
    }

    for filename in event
        .commits
        .iter()
        .flat_map(|c| c.added.iter().chain(c.modified.iter()))
    {
        log_info(&format!("Push event file: {}", filename));
        if IS_CHANGELOG_RE.is_match(filename) {
            process_changelogs(config);
            return;
        }
    }
}

fn parse_body_changelog(body: &str) -> Vec<ChangelogEntry> {
    lazy_static! {
        static ref HEADER_RE: Regex = RegexBuilder::new(r#"(?::cl:|🆑) *\r?\n(.+)$"#).dot_matches_new_line(true).build().unwrap();
        static ref ENTRY_RE: Regex = RegexBuilder::new(r#"^ *[*-]? *(bugfix|wip|tweak|soundadd|sounddel|rscdel|rscadd|imageadd|imagedel|spellcheck|experiment|tgs): *(\S[^\n\r]+)\r?$"#).multi_line(true).build().unwrap();
    }

    let content = match HEADER_RE.captures(body) {
        Some(capture) => capture.get(1).unwrap().as_str(),
        _ => return Vec::new(),
    };

    ENTRY_RE
        .captures_iter(content)
        .map(|m| {
            let entry_type = match m.get(1).unwrap().as_str() {
                "bugfix" => ChangelogEntryType::Bugfix,
                "wip" => ChangelogEntryType::Wip,
                "tweak" => ChangelogEntryType::Tweak,
                "soundadd" => ChangelogEntryType::Soundadd,
                "sounddel" => ChangelogEntryType::Sounddel,
                "rscdel" => ChangelogEntryType::Rscdel,
                "rscadd" => ChangelogEntryType::Rscadd,
                "imageadd" => ChangelogEntryType::Imageadd,
                "imagedel" => ChangelogEntryType::Imagedel,
                "spellcheck" => ChangelogEntryType::Spellcheck,
                "experiment" => ChangelogEntryType::Experiment,
                "tgs" => ChangelogEntryType::Tgs,
                _ => unreachable!(),
            };

            ChangelogEntry(entry_type, m.get(2).unwrap().as_str().trim().to_owned())
        })
        .collect()
}

fn write_temp_changelog(path: &Path, changelog: Changelog) -> std::io::Result<()> {
    let mut file = File::create(path)?;
    serde_yaml::to_writer(&file, &changelog).unwrap(); // TODO: Remove unwrap.
    file.flush()?;
    Ok(())
}

lazy_static! {
    pub static ref CHANGELOG_MANAGER: Mutex<ChangelogManager> =
        { Mutex::new(ChangelogManager { last_time: None }) };
}

pub struct ChangelogManager {
    // If None, no thread is currently on it.
    last_time: Option<Instant>,
}

/// Represents a new changelog entry.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub struct Changelog {
    pub author: String,
    pub changes: Vec<ChangelogEntry>,
    pub delete_after: Option<bool>,
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub struct ChangelogEntry(ChangelogEntryType, String);

impl Serialize for ChangelogEntry {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut map = serializer.serialize_map(Some(1))?;
        map.serialize_entry(&self.0, &self.1)?;
        map.end()
    }
}

impl<'de> Deserialize<'de> for ChangelogEntry {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_map(ChangelogEntryVisitor)
    }
}

struct ChangelogEntryVisitor;

impl<'de> Visitor<'de> for ChangelogEntryVisitor {
    type Value = ChangelogEntry;

    fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
        formatter.write_str("A single-element map")
    }

    fn visit_map<M>(self, mut access: M) -> Result<Self::Value, M::Error>
    where
        M: MapAccess<'de>,
    {
        match access.next_entry()? {
            Some((key, value)) => {
                let value = ChangelogEntry(key, value);
                match access.next_key::<ChangelogEntryType>()? {
                    Some(_) => Err(M::Error::invalid_length(2, &"A single-element map.")),
                    _ => Ok(value),
                }
            }
            None => Err(M::Error::invalid_length(0, &"A single-element map.")),
        }
    }
}

#[derive(Debug, Copy, Clone, Hash, Eq, PartialEq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ChangelogEntryType {
    Bugfix,
    Wip,
    Tweak,
    Soundadd,
    Sounddel,
    Rscadd,
    Rscdel,
    Imageadd,
    Imagedel,
    Spellcheck,
    Experiment,
    Tgs,
}

pub fn process_changelogs(config: &Arc<MoMMIConfig>) {
    log_info("process_changelogs called");
    let mut lock = CHANGELOG_MANAGER.lock().unwrap();
    let should_spawn_thread = lock.last_time.is_none();
    lock.last_time = Some(Instant::now());

    if should_spawn_thread {
        // Nobody currently processing.
        log_info("Spawning new changelog thread");
        lock.last_time = Some(Instant::now());
        let config = config.clone();
        thread::Builder::new()
            .name("Changelog thread".into())
            .spawn(move || {
                handle_changelog_thread(config);
            })
            .unwrap();
    } else {
        log_info("Changelog thread already running, updated last_time");
    }
}

fn handle_changelog_thread(config: Arc<MoMMIConfig>) {
    let delay = config.get_changelog_delay();
    log_info(&format!("Changelog thread started, delay={}s", delay));

    loop {
        let time = {
            let lock = CHANGELOG_MANAGER.lock().unwrap();
            let elapsed = lock.last_time.as_ref().unwrap().elapsed();
            if elapsed.as_secs() > delay {
                log_info("Delay elapsed, starting changelog processing");
                return do_changelog(lock, config);
            }

            match Duration::from_secs(delay).checked_sub(elapsed) {
                Some(t) => t,
                None => {
                    log_info("Delay elapsed, starting changelog processing");
                    return do_changelog(lock, config);
                }
            }
        };
        log_info(&format!("Waiting {:?} before processing", time));
        thread::sleep(time);
    }
}

// Pass the lock directly so we don't risk race conditions.
fn do_changelog(mut lock: MutexGuard<ChangelogManager>, config: Arc<MoMMIConfig>) {
    log_info("Running changelogs!");
    // Get what we need and drop the lock.
    // so we don't hang everything for the time it takes for the git commands and stuff.
    lock.last_time = None;
    drop(lock);

    let path = config.get_changelog_repo_path().unwrap();
    let ssh_config = config
        .get_ssh_key()
        .map(|p| format!("ssh -i {}", p.to_string_lossy()));

    let mut fetch_cmd = Command::new("git");
    fetch_cmd
        .args(["fetch", "origin"])
        .current_dir(&path);
    if let Some(ref ssh_command) = ssh_config {
        fetch_cmd.env("GIT_SSH_COMMAND", &ssh_command);
    }
    let fetch_output = fetch_cmd.output();

    match &fetch_output {
        Ok(o) if o.status.success() => log_info("git fetch successful"),
        Ok(o) => {
            log_error(&format!("git fetch failed: {}", String::from_utf8_lossy(&o.stderr)));
            return;
        }
        Err(e) => {
            log_error(&format!("git fetch error: {:?}", e));
            return;
        }
    }

    // discard changes to changelog.html and .all-changelogs.yml, those are handled by github action now
    let reset_output = Command::new("git")
        .args(["reset", "--hard", "origin/Bleeding-Edge"])
        .current_dir(&path)
        .output();

    match &reset_output {
        Ok(o) if o.status.success() => log_info("git reset successful"),
        Ok(o) => {
            log_error(&format!("git reset failed: {}", String::from_utf8_lossy(&o.stderr)));
            return;
        }
        Err(e) => {
            log_error(&format!("git reset error: {:?}", e));
            return;
        }
    }

    let mut changelog_dir_path = path.to_owned();
    changelog_dir_path.push("html/changelogs");

    // Send changelog files over to MoMMI maybe.
    if let Some((addr, pass)) = config.get_commloop() {
        for entry in read_dir(&changelog_dir_path).unwrap() {
            let entry = entry.unwrap();
            let os_file_name = entry.file_name();
            let file_name = os_file_name.to_str().unwrap();
            if file_name.starts_with(".")
                || !file_name.ends_with(".yml")
                || file_name == "example.yml"
                || file_name.starts_with("AutoChangeLog") //generated by github action, lets not duplicate report these
            {
                continue;
            }

            log_info(&format!("Processing changelog file: {}", file_name));

            let file = match File::open(entry.path()) {
                Ok(f) => f,
                Err(e) => {
                    log_error(&format!("Failed to open {}: {:?}", file_name, e));
                    continue;
                }
            };
            let data: Changelog = match serde_yaml::from_reader(&file) {
                Ok(d) => d,
                Err(e) => {
                    log_error(&format!("Failed to parse {}: {:?}", file_name, e));
                    continue;
                }
            };

            if data.changes.len() == 0 {
                log_warning(&format!("Changelog {} has no changes, skipping", file_name));
                continue;
            }

            match commloop(addr, pass, "changelog", "", &data) {
                Ok(_) => log_info(&format!("Changelog for {} sent to commloop", file_name)),
                Err(e) => log_error(&format!("Failed sending changelog for {}: {:?}", file_name, e)),
            }
        }
    }

    // Run changelog script.
    let status = Command::new("python")
        .arg("tools/changelog/ss13_genchangelog.py")
        .arg("html/changelog.html")
        .arg("html/changelogs")
        .current_dir(&path)
        .status();

    match status {
        Ok(s) if s.success() => log_info("Changelog script successful"),
        Ok(s) => log_error(&format!("Changelog script failed: {:?}", s)),
        Err(e) => log_error(&format!("Changelog script failed badly: {:?}", e)),
    }


    // Job below is handled by a github action now


    // Command::new("git")
    //     .arg("update-index")
    //     .arg("--refresh")
    //     .current_dir(&path)
    //     .status()
    //     .unwrap();

    // // See if repo is dirty.
    // let status = Command::new("git")
    //     .arg("diff-index")
    //     .arg("--exit-code")
    //     .arg("HEAD")
    //     .current_dir(&path)
    //     .status()
    //     .unwrap();

    // if status.code().unwrap_or(0) == 0 {
    //     // No changes, nothing to commit.
    //     return;
    // }

    // let status = Command::new("git")
    //     .arg("add")
    //     .arg(".")
    //     .arg("-A")
    //     .current_dir(&path)
    //     .status()
    //     .unwrap();

    // assert!(status.success());

    // let status = Command::new("git")
    //     .arg("commit")
    //     .arg("-m")
    //     .arg("[ci skip] Automatic changelog update.")
    //     .current_dir(&path)
    //     .status()
    //     .unwrap();

    // assert!(status.success());

    // Git push the repo.
    //let mut command = Command::new("git");
    //command.arg("push").arg("origin").current_dir(&path);
    //if let Some(ref ssh_command) = ssh_config {
    //    command.env("GIT_SSH_COMMAND", &ssh_command);
    //}
    //let status = command.status().unwrap();
//
    //assert!(status.success());

    log_info("Changelog processing complete");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn body_parse_test() {
        // vgstation-coders/vgstation13#21025
        const TEST: &str = "Ghosts can now possess dionae and mushmonkeys via clicking on them, should they have no client controlling them.\r\n\r\n:cl:\r\n * rscadd: Ghosts can now possess inactive diona nymphs and mushrum monkeys by clicking on them.  \r\n * rscadd: Dionae now don't expire after harvesting, should they not be possessed in the given time.  ";

        let entries = parse_body_changelog(TEST);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0], ChangelogEntry(ChangelogEntryType::Rscadd, "Ghosts can now possess inactive diona nymphs and mushrum monkeys by clicking on them.".into()));
        assert_eq!(entries[1], ChangelogEntry(ChangelogEntryType::Rscadd, "Dionae now don't expire after harvesting, should they not be possessed in the given time.".into()));
    }
}
changelog.rs
16 KB
