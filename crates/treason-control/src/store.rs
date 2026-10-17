//! Saved projects and exported MIDI files: a directory of each, with a file
//! for each name.

use std::ffi::OsStr;
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::SystemTime;

use thiserror::Error;
use treason_core::model::Project;
use treason_devices::DeviceRegistry;

use crate::file::{self, FileError};

/// The most bytes a project file may hold, so that loading one can't
/// exhaust memory. Saving refuses a larger project, so every saved project
/// loads.
pub const MAX_FILE_BYTES: u64 = 16 << 20;

/// The most names [`ProjectStore::list`] returns.
pub const LIST_LIMIT: usize = 1000;

/// The longest project name, in characters.
pub const MAX_NAME_CHARS: usize = 64;

/// What a project file's name ends with.
const EXTENSION: &str = "toml";

/// What an exported MIDI file's name ends with.
const MIDI_EXTENSION: &str = "mid";

/// Numbers the temporary files saves write, so that no two share a name.
static TEMPORARY_FILES: AtomicU64 = AtomicU64::new(0);

/// Why a project couldn't be saved, loaded, listed or exported. A failed
/// save, load or export changes nothing.
#[derive(Debug, Error)]
pub enum ProjectError {
    /// The text can't name a project.
    #[error(
        "`{0}` can't name a project: use 1 to {MAX_NAME_CHARS} letters, digits, spaces, \
         dashes and underscores, beginning with a letter or digit and not ending in a space"
    )]
    InvalidName(String),
    /// Saving without a name, before the project has one.
    #[error("the project has no name yet")]
    Unnamed,
    /// Saving would replace another project.
    #[error("there is already a project `{0}`")]
    Exists(ProjectName),
    /// Exporting would replace another MIDI file.
    #[error("there is already a MIDI file `{0}`")]
    ExportExists(ProjectName),
    /// No project has this name.
    #[error("there is no project `{0}`")]
    NotFound(ProjectName),
    /// Loading would lose changes that haven't been saved.
    #[error("the project has unsaved changes")]
    UnsavedChanges,
    /// The project is too big to save or load.
    #[error("project `{name}` takes {size} bytes, more than the {limit} a project may")]
    TooLarge {
        /// The project.
        name: ProjectName,
        /// Its size.
        size: u64,
        /// The most a project may take.
        limit: u64,
    },
    /// A project file can't be written or read.
    #[error("{}: {error}", path.display())]
    File {
        /// The file.
        path: PathBuf,
        /// What is wrong.
        error: FileError,
    },
    /// The file system refused.
    #[error("{}: {error}", path.display())]
    Io {
        /// The file or directory.
        path: PathBuf,
        /// What happened.
        error: io::Error,
    },
}

/// A saved project's name, which is also its file's name, without `.toml`.
///
/// Letters, digits, spaces, dashes and underscores only, so a name can
/// neither reach outside the project directory nor hide its file.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ProjectName(String);

impl ProjectName {
    /// Checks that `name` can name a project.
    pub fn new(name: impl Into<String>) -> Result<Self, ProjectError> {
        let name = name.into();
        let valid = name.chars().next().is_some_and(char::is_alphanumeric)
            && !name.ends_with(' ')
            && name.chars().count() <= MAX_NAME_CHARS
            && name
                .chars()
                .all(|c| c.is_alphanumeric() || matches!(c, ' ' | '-' | '_'));
        if valid {
            Ok(Self(name))
        } else {
            Err(ProjectError::InvalidName(name))
        }
    }

    /// The name.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ProjectName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl FromStr for ProjectName {
    type Err = ProjectError;

    fn from_str(name: &str) -> Result<Self, Self::Err> {
        Self::new(name)
    }
}

/// What [`ProjectStore::list`] found.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProjectList {
    /// Names, newest first: the most recently saved project leads.
    pub names: Vec<ProjectName>,
    /// How many more projects there are, all older than the last name.
    pub more: usize,
}

/// A directory of saved projects, with a file for each name.
#[derive(Debug, Clone)]
pub struct ProjectStore {
    dir: PathBuf,
    max_bytes: u64,
}

impl ProjectStore {
    /// The projects in `dir`. Saving creates it if need be.
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self {
            dir: dir.into(),
            max_bytes: MAX_FILE_BYTES,
        }
    }

    /// The directory.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// The file that holds the project `name`.
    pub fn path(&self, name: &ProjectName) -> PathBuf {
        self.dir.join(format!("{name}.{EXTENSION}"))
    }

    /// Whether a project `name` is saved.
    pub fn exists(&self, name: &ProjectName) -> Result<bool, ProjectError> {
        let path = self.path(name);
        path.try_exists()
            .map_err(|error| ProjectError::Io { path, error })
    }

    /// Saves `project` as `name`. Unless `overwrite`, atomically refuses an
    /// existing name with [`ProjectError::Exists`]. `ports` names the output
    /// ports, in `PortId` order.
    ///
    /// The file is written and flushed to disk under a temporary name, then
    /// linked into place for a new name, or renamed for a replacement, so a
    /// failed save leaves the old file as it was.
    pub fn save(
        &self,
        name: &ProjectName,
        project: &Project,
        ports: &[String],
        overwrite: bool,
    ) -> Result<(), ProjectError> {
        let path = self.path(name);
        let text = file::write(project, ports).map_err(|error| ProjectError::File {
            path: path.clone(),
            error,
        })?;
        let size = text.len() as u64;
        if size > self.max_bytes {
            return Err(ProjectError::TooLarge {
                name: name.clone(),
                size,
                limit: self.max_bytes,
            });
        }
        match publish(&self.dir, &path, text.as_bytes(), overwrite)? {
            Published::Written => Ok(()),
            Published::Exists => Err(ProjectError::Exists(name.clone())),
        }
    }

    /// Loads the project saved as `name`. `ports` names the output ports,
    /// in `PortId` order, and every device a track uses must have a profile
    /// in `devices`.
    pub fn load(
        &self,
        name: &ProjectName,
        ports: &[String],
        devices: &DeviceRegistry,
    ) -> Result<Project, ProjectError> {
        let path = self.path(name);
        let text = self.read(name, &path)?;
        file::read(&text, ports, devices).map_err(|error| ProjectError::File { path, error })
    }

    /// The saved projects, newest first, at most [`LIST_LIMIT`] of them.
    /// Files that aren't projects are left out. If the directory doesn't
    /// exist yet, there are none.
    pub fn list(&self) -> Result<ProjectList, ProjectError> {
        let io = |error| ProjectError::Io {
            path: self.dir.clone(),
            error,
        };
        let entries = match fs::read_dir(&self.dir) {
            Ok(entries) => entries,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Ok(ProjectList::default());
            }
            Err(error) => return Err(io(error)),
        };
        let mut projects = Vec::new();
        for entry in entries {
            let path = entry.map_err(io)?.path();
            if path.extension() != Some(OsStr::new(EXTENSION)) {
                continue;
            }
            let Some(name) = path
                .file_stem()
                .and_then(OsStr::to_str)
                .and_then(|stem| ProjectName::new(stem).ok())
            else {
                continue;
            };
            // Following links, as loading does. An entry that vanished or
            // can't be read is no project to load.
            match fs::metadata(&path) {
                Ok(metadata) if metadata.is_file() => {
                    let modified = metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH);
                    projects.push((modified, name));
                }
                _ => {}
            }
        }
        projects.sort_by(|(a_time, a_name), (b_time, b_name)| {
            b_time.cmp(a_time).then_with(|| a_name.cmp(b_name))
        });
        let more = projects.len().saturating_sub(LIST_LIMIT);
        projects.truncate(LIST_LIMIT);
        Ok(ProjectList {
            names: projects.into_iter().map(|(_, name)| name).collect(),
            more,
        })
    }

    /// The text of the file at `path`, which holds the project `name`.
    fn read(&self, name: &ProjectName, path: &Path) -> Result<String, ProjectError> {
        let io = |error| ProjectError::Io {
            path: path.to_owned(),
            error,
        };
        let file = File::open(path).map_err(|error| match error.kind() {
            io::ErrorKind::NotFound => ProjectError::NotFound(name.clone()),
            _ => io(error),
        })?;
        let too_large = |size| ProjectError::TooLarge {
            name: name.clone(),
            size,
            limit: self.max_bytes,
        };
        let size = file.metadata().map_err(io)?.len();
        if size > self.max_bytes {
            return Err(too_large(size));
        }
        // The file can grow after the check, so read at most one byte more
        // than a project may take.
        let mut text = String::new();
        file.take(self.max_bytes + 1)
            .read_to_string(&mut text)
            .map_err(io)?;
        let size = text.len() as u64;
        if size > self.max_bytes {
            return Err(too_large(size));
        }
        Ok(text)
    }
}

/// A directory of MIDI files exported from projects, with a file for each
/// name.
#[derive(Debug, Clone)]
pub struct ExportStore {
    dir: PathBuf,
}

impl ExportStore {
    /// The MIDI files in `dir`. Exporting creates it if need be.
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    /// The directory.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// The file that holds the export `name`.
    pub fn path(&self, name: &ProjectName) -> PathBuf {
        self.dir.join(format!("{name}.{MIDI_EXTENSION}"))
    }

    /// Writes `bytes`, a MIDI file, as the export `name` and returns its
    /// path. Unless `overwrite`, atomically refuses an existing file with
    /// [`ProjectError::ExportExists`]. Written like a saved project, so a
    /// failed export leaves the old file as it was.
    pub fn save(
        &self,
        name: &ProjectName,
        bytes: &[u8],
        overwrite: bool,
    ) -> Result<PathBuf, ProjectError> {
        let path = self.path(name);
        match publish(&self.dir, &path, bytes, overwrite)? {
            Published::Written => Ok(path),
            Published::Exists => Err(ProjectError::ExportExists(name.clone())),
        }
    }

    /// Writes `bytes`, a MIDI file, to an explicit path, atomically
    /// replacing any earlier file. The parent directory must exist. A
    /// failed write or flush leaves the old file as it was.
    pub fn save_to(path: &Path, bytes: &[u8]) -> Result<(), ProjectError> {
        publish_file(path, true, |temporary| write_new(temporary, bytes))?;
        Ok(())
    }
}

/// What [`publish`] did.
enum Published {
    /// The file is in place.
    Written,
    /// A file of that name exists, and replacing it wasn't allowed.
    Exists,
}

/// Puts `bytes` at `path` in `dir`, creating `dir` if need be, without a
/// moment where `path` holds only some of them. They are written and flushed
/// to disk under a temporary name, then linked into place for a new file or,
/// with `overwrite`, renamed over the old one. So a failed write leaves the
/// old file as it was, and without `overwrite`, a file that appeared in the
/// meantime, even from another process, isn't replaced.
fn publish(
    dir: &Path,
    path: &Path,
    bytes: &[u8],
    overwrite: bool,
) -> Result<Published, ProjectError> {
    fs::create_dir_all(dir).map_err(|error| ProjectError::Io {
        path: dir.to_owned(),
        error,
    })?;
    publish_file(path, overwrite, |temporary| write_new(temporary, bytes))
}

/// Writes and flushes a temporary sibling of `path` with `write`, then
/// publishes it. A write failure removes the temporary file and leaves
/// `path` as it was.
fn publish_file(
    path: &Path,
    overwrite: bool,
    write: impl FnOnce(&Path) -> io::Result<()>,
) -> Result<Published, ProjectError> {
    let temporary = path.with_file_name(format!(
        ".{}.{}-{}.tmp",
        path.file_name().unwrap_or_default().display(),
        std::process::id(),
        TEMPORARY_FILES.fetch_add(1, Ordering::Relaxed)
    ));
    let io = |error| ProjectError::Io {
        path: path.to_owned(),
        error,
    };
    let published = write(&temporary).map_err(io).and_then(|()| {
        let placed = if overwrite {
            fs::rename(&temporary, path)
        } else {
            // Both names are in the same directory. A hard link publishes
            // the complete file without replacing a name that another
            // process may have taken in the meantime.
            fs::hard_link(&temporary, path)
        };
        match placed {
            Ok(()) => Ok(Published::Written),
            Err(error) if !overwrite && error.kind() == io::ErrorKind::AlreadyExists => {
                Ok(Published::Exists)
            }
            Err(error) => Err(io(error)),
        }
    });
    // A successful link leaves the temporary name too. Cleanup mustn't turn
    // an already published file into a reported failure.
    let _ = fs::remove_file(&temporary);
    published
}

/// Creates the file at `path`, which mustn't exist, writes `bytes` and
/// flushes them to disk.
fn write_new(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let mut file = OpenOptions::new().write(true).create_new(true).open(path)?;
    file.write_all(bytes)?;
    file.sync_all()
}

/// A directory for one test, removed when the test ends.
#[cfg(test)]
pub(crate) struct TestDir(PathBuf);

#[cfg(test)]
impl TestDir {
    /// An empty directory named after `test`.
    pub(crate) fn new(test: &str) -> Self {
        let dir =
            std::env::temp_dir().join(format!("treason-control-{}-{test}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }

    pub(crate) fn path(&self) -> &Path {
        &self.0
    }
}

#[cfg(test)]
impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use treason_core::midi::Channel;
    use treason_core::model::{Clip, OutputTarget, PortId, Track, TrackId};
    use treason_core::time::Bpm;

    use super::*;

    fn name(name: &str) -> ProjectName {
        ProjectName::new(name).unwrap()
    }

    fn ports() -> Vec<String> {
        vec!["out".into()]
    }

    fn project(bpm: f64) -> Project {
        Project {
            tempo: Bpm::new(bpm).unwrap(),
            tracks: vec![Arc::new(Track {
                id: TrackId(1),
                name: "drums".into(),
                device: None,
                target: OutputTarget {
                    port: PortId(0),
                    channel: Channel::DRUMS,
                },
                muted: false,
                program: None,
                bend_range: None,
                clip: Clip::default(),
            })],
            ..Project::default()
        }
    }

    /// The names of the files in `dir`.
    fn files(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect();
        names.sort();
        names
    }

    #[test]
    fn names_hold_only_what_a_file_name_can_safely_hold() {
        for valid in [
            "house",
            "Acid Jam 2",
            "dub-techno_v2",
            "Café",
            "x",
            &"y".repeat(64),
        ] {
            assert_eq!(ProjectName::new(valid).unwrap().as_str(), valid);
        }
        for invalid in [
            "",
            " lead",
            "lead ",
            "-flag",
            "_draft",
            "../escape",
            "a/b",
            "a\\b",
            ".hidden",
            "song.toml",
            "tab\there",
            "line\nbreak",
            "nul\0",
            &"z".repeat(65),
        ] {
            let error = ProjectName::new(invalid).unwrap_err();
            assert!(
                matches!(&error, ProjectError::InvalidName(text) if text == invalid),
                "{invalid:?} was accepted"
            );
            assert!(error.to_string().contains("letters, digits"), "{error}");
        }
        assert_eq!("house".parse::<ProjectName>().unwrap(), name("house"));
    }

    #[test]
    fn saving_requires_overwrite_to_replace_an_existing_project() {
        let dir = TestDir::new("save-load");
        let store = ProjectStore::new(dir.path().join("projects"));
        let devices = DeviceRegistry::new();
        assert!(!store.exists(&name("jam")).unwrap());

        store
            .save(&name("jam"), &project(100.0), &ports(), false)
            .unwrap();
        assert!(store.exists(&name("jam")).unwrap());
        assert_eq!(
            store.load(&name("jam"), &ports(), &devices).unwrap(),
            project(100.0)
        );

        let before = fs::read(store.path(&name("jam"))).unwrap();
        let error = store
            .save(&name("jam"), &project(140.0), &ports(), false)
            .unwrap_err();
        assert!(
            matches!(&error, ProjectError::Exists(taken) if *taken == name("jam")),
            "{error}"
        );
        assert_eq!(fs::read(store.path(&name("jam"))).unwrap(), before);
        assert_eq!(files(store.dir()), ["jam.toml"]);

        store
            .save(&name("jam"), &project(140.0), &ports(), true)
            .unwrap();
        assert_eq!(
            store.load(&name("jam"), &ports(), &devices).unwrap(),
            project(140.0)
        );
        // No temporary file is left behind.
        assert_eq!(files(store.dir()), ["jam.toml"]);
    }

    #[test]
    fn a_failed_save_leaves_the_old_file_as_it_was() {
        let dir = TestDir::new("failed-save");
        let store = ProjectStore::new(dir.path());
        store
            .save(&name("jam"), &project(100.0), &ports(), false)
            .unwrap();
        let before = fs::read_to_string(store.path(&name("jam"))).unwrap();

        // A track on a port with no name can't be written.
        let error = store
            .save(&name("jam"), &project(140.0), &[], true)
            .unwrap_err();
        assert!(matches!(
            error,
            ProjectError::File {
                error: FileError::UnnamedPort { .. },
                ..
            }
        ));
        // A directory where the file should go can't be replaced.
        fs::create_dir(store.path(&name("blocked"))).unwrap();
        let error = store
            .save(&name("blocked"), &project(140.0), &ports(), true)
            .unwrap_err();
        assert!(matches!(error, ProjectError::Io { .. }), "{error}");

        assert_eq!(
            fs::read_to_string(store.path(&name("jam"))).unwrap(),
            before
        );
        assert_eq!(files(store.dir()), ["blocked.toml", "jam.toml"]);
    }

    #[test]
    fn loading_says_why_it_failed() {
        let dir = TestDir::new("load-errors");
        let store = ProjectStore::new(dir.path());
        let devices = DeviceRegistry::new();
        let error = store
            .load(&name("nothing"), &ports(), &devices)
            .unwrap_err();
        assert!(
            matches!(&error, ProjectError::NotFound(missing) if *missing == name("nothing")),
            "{error}"
        );

        fs::write(store.path(&name("broken")), "format = 1\ntempo =\n").unwrap();
        let error = store.load(&name("broken"), &ports(), &devices).unwrap_err();
        assert!(matches!(error, ProjectError::File { .. }), "{error}");
        let message = error.to_string();
        assert!(
            message.contains("broken.toml") && message.contains("line 2"),
            "{message}"
        );
    }

    #[test]
    fn projects_too_big_to_load_are_neither_saved_nor_loaded() {
        let dir = TestDir::new("too-large");
        let store = ProjectStore::new(dir.path());
        store
            .save(&name("small"), &project(100.0), &ports(), false)
            .unwrap();
        let size = fs::metadata(store.path(&name("small"))).unwrap().len();
        let limited = ProjectStore {
            max_bytes: size - 1,
            ..store.clone()
        };

        let error = limited
            .load(&name("small"), &ports(), &DeviceRegistry::new())
            .unwrap_err();
        assert!(
            matches!(error, ProjectError::TooLarge { size: s, limit, .. } if s == size && limit == size - 1),
            "{error}"
        );
        let error = limited
            .save(&name("other"), &project(100.0), &ports(), false)
            .unwrap_err();
        assert!(matches!(error, ProjectError::TooLarge { .. }), "{error}");
        assert!(!store.exists(&name("other")).unwrap());

        // A file much larger than the limit isn't read at all.
        let huge = File::create(store.path(&name("huge"))).unwrap();
        huge.set_len(MAX_FILE_BYTES + 1).unwrap();
        let error = store
            .load(&name("huge"), &ports(), &DeviceRegistry::new())
            .unwrap_err();
        assert!(matches!(error, ProjectError::TooLarge { .. }), "{error}");
    }

    #[test]
    fn listing_shows_the_newest_projects_first() {
        let dir = TestDir::new("list");
        let store = ProjectStore::new(dir.path().join("projects"));
        assert_eq!(store.list().unwrap(), ProjectList::default());

        let start = SystemTime::UNIX_EPOCH + Duration::from_secs(1_800_000_000);
        for (offset, saved) in [(1, "old"), (3, "newest"), (2, "tied b"), (2, "tied a")] {
            store
                .save(&name(saved), &project(120.0), &ports(), false)
                .unwrap();
            File::options()
                .write(true)
                .open(store.path(&name(saved)))
                .unwrap()
                .set_modified(start + Duration::from_secs(offset))
                .unwrap();
        }
        // Not projects: another extension, a name a project can't have, a
        // temporary file and a directory.
        fs::write(store.dir().join("notes.txt"), "").unwrap();
        fs::write(store.dir().join("bad name .toml"), "").unwrap();
        fs::write(store.dir().join(".old.toml.1-1.tmp"), "").unwrap();
        fs::create_dir(store.dir().join("folder.toml")).unwrap();

        let list = store.list().unwrap();
        let names: Vec<&str> = list.names.iter().map(ProjectName::as_str).collect();
        assert_eq!(names, ["newest", "tied a", "tied b", "old"]);
        assert_eq!(list.more, 0);
    }

    #[test]
    fn listing_stops_at_its_limit() {
        let dir = TestDir::new("list-limit");
        let store = ProjectStore::new(dir.path());
        for index in 0..LIST_LIMIT + 2 {
            fs::write(store.path(&name(&format!("p{index:04}"))), "").unwrap();
        }
        let list = store.list().unwrap();
        assert_eq!((list.names.len(), list.more), (LIST_LIMIT, 2));
    }

    #[test]
    fn exports_replace_a_file_only_when_told_to() {
        let dir = TestDir::new("exports");
        let exports = ExportStore::new(dir.path().join("exports"));
        let path = exports.save(&name("jam"), b"first", false).unwrap();
        assert_eq!(path, dir.path().join("exports").join("jam.mid"));
        assert_eq!(fs::read(&path).unwrap(), b"first");

        let error = exports.save(&name("jam"), b"second", false).unwrap_err();
        assert!(
            matches!(&error, ProjectError::ExportExists(taken) if *taken == name("jam")),
            "{error}"
        );
        assert_eq!(fs::read(&path).unwrap(), b"first");
        exports.save(&name("jam"), b"second", true).unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"second");

        // A directory in the way can't be replaced, and no temporary file
        // is left behind.
        fs::create_dir(exports.path(&name("blocked"))).unwrap();
        let error = exports.save(&name("blocked"), b"third", true).unwrap_err();
        assert!(matches!(error, ProjectError::Io { .. }), "{error}");
        assert_eq!(files(exports.dir()), ["blocked.mid", "jam.mid"]);
    }

    #[test]
    fn explicit_exports_replace_the_requested_file() {
        let dir = TestDir::new("explicit-export");
        let path = dir.path().join("a.custom-midi-file");
        ExportStore::save_to(&path, b"first").unwrap();
        ExportStore::save_to(&path, b"second").unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"second");
        assert_eq!(files(dir.path()), ["a.custom-midi-file"]);

        // An explicit path keeps requiring an existing parent directory.
        let missing = dir.path().join("missing").join("jam.mid");
        let error = ExportStore::save_to(&missing, b"third").unwrap_err();
        assert!(matches!(error, ProjectError::Io { .. }), "{error}");
        assert!(!missing.parent().unwrap().exists());
    }

    #[test]
    fn a_failed_replacement_preserves_the_previous_file() {
        let dir = TestDir::new("failed-replacement");
        let path = dir.path().join("jam.mid");
        ExportStore::save_to(&path, b"previous complete MIDI file").unwrap();

        // Fail after writing part of the replacement, as a full disk can.
        let result = publish_file(&path, true, |temporary| {
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(temporary)?;
            file.write_all(b"partial replacement")?;
            Err(io::Error::other("simulated write failure"))
        });
        assert!(matches!(result, Err(ProjectError::Io { .. })));
        assert_eq!(fs::read(&path).unwrap(), b"previous complete MIDI file");
        assert_eq!(files(dir.path()), ["jam.mid"]);
    }
}
