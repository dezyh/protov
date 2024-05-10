#![allow(unused)]

use std::{
    env,
    fmt::Debug,
    fs::{self, File, OpenOptions},
    io::{BufReader, BufWriter},
    path::{Path, PathBuf},
    process::Command,
};

use anyhow::{anyhow, Context};
use dirs::cache_dir;
use serde::{Deserialize, Serialize};
use tracing::{debug, error, info, warn};
use uuid::Uuid;

use crate::{CleanArgs, SyncArgs};

const BUILD_OUTPUTS_PATH: &str = "./protov-build-outputs.json";

pub fn parse(path: impl Into<PathBuf>) -> anyhow::Result<Config> {
    let content = std::fs::read_to_string(path.into())?;
    let config = toml::from_str(&content)?;
    Ok(config)
}

#[derive(Deserialize, Debug, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub struct Config {
    #[serde(rename = "source")]
    sources: Vec<Source>,
    #[serde(rename = "template")]
    templates: Vec<Template>,
    #[serde(rename = "target")]
    targets: Vec<CompileTarget>,
}

#[derive(Deserialize, Debug, PartialEq, Eq)]
struct Template {
    in_path: PathBuf,
    out_path: PathBuf,
}

#[derive(Deserialize, Debug, PartialEq, Eq)]
struct CompileTarget {
    /// Path to where to build the output artifacts
    ///
    /// This is what is used when clearing based on the build lock/actions.
    /// Think of it as the identifier for the target... not a great design...
    out_path: PathBuf,
    /// Path to the buf template yaml
    out_gen_path: PathBuf,
    /// The 'name' field of sources which should be built
    sources: Vec<String>,
}

#[derive(Serialize, Deserialize, Debug, PartialEq, Eq, Clone)]
#[serde(rename_all = "lowercase", tag = "type")]
enum Source {
    Local(LocalSource),
    Remote(RemoteSource),
}

impl Source {
    fn name(&self) -> &str {
        match self {
            Source::Local(source) => &source.name,
            Source::Remote(source) => &source.name,
        }
    }
}

#[derive(Serialize, Deserialize, PartialEq, Eq, Debug, Clone)]
struct LocalSource {
    path: String,
    name: String,
}

#[derive(Serialize, Deserialize, PartialEq, Eq, Debug, Clone)]
struct RemoteSource {
    url: String,
    tag: String,
    path: String,
    name: String,
}

/// An index into the cache directory. This index will be written to disk to allow the cache to be
/// used between separate invocations.
#[derive(Serialize, Deserialize, Debug, Clone, Default)]
struct CacheIndex {
    entries: Vec<CacheEntry>,
}

impl CacheIndex {
    fn try_from_file(path: impl Into<PathBuf>) -> anyhow::Result<Self> {
        read_cache_index(path)
    }

    fn lookup_cached_source(&self, source: &Source) -> Option<CacheEntry> {
        self.entries
            .iter()
            .find(|&entry| entry.source == *source)
            .cloned()
    }
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
struct CacheEntry {
    path: PathBuf,
    source: Source,
}

/// Read the cache index from file path.
fn read_cache_index(path: impl Into<PathBuf>) -> anyhow::Result<CacheIndex> {
    let file = OpenOptions::new().read(true).open(path.into())?;
    let reader = BufReader::new(file);
    let index = serde_json::from_reader(reader)?;
    Ok(index)
}

/// Write the updated cache index to the file path.
fn write_cache_index(path: impl Into<PathBuf>, index: &CacheIndex) -> anyhow::Result<()> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .truncate(true)
        .create(true)
        .open(path.into())?;
    serde_json::to_writer_pretty(file, &index)?;
    Ok(())
}

// Function to expand environment variables and the tilde
fn expand_path(path: &str) -> String {
    match path.starts_with("~/") {
        true => match env::var("HOME") {
            Ok(home) => path.replacen('~', &home, 1),
            Err(e) => path.to_string(),
        },
        false => path.to_string(),
    }
}

pub fn sync(args: &SyncArgs, config: &Config) {
    let cache_index = match cache(config) {
        Ok(index) => index,
        Err(err) => {
            error!(?err, "failed to cache, aborting sync");
            return;
        }
    };

    clean_vendor();

    // create_targets_from_templates(config);

    populate_vendor(config, &cache_index);

    // clean_target()
    // let before = stat_dir();
    // let after = stat_dir();
    // let outputs = stat_diff(before, after);
    // write_outputs_to_lockfile()

    build_targets(config).inspect_err(|err| error!(?err, "failed to build targets"));
}

pub fn clean(args: &CleanArgs, config: &Config) {
    clean_targets(config).inspect_err(|err| error!(?err, "failed to clean targets"));
}

#[derive(Serialize, Deserialize, Debug, Default)]
struct BuildActions {
    diffs: Vec<TargetBuildActions>,
}

impl BuildActions {
    fn try_read_from_file(path: impl AsRef<Path>) -> anyhow::Result<Self> {
        let file = OpenOptions::new()
            .read(true)
            .open(path)
            .with_context(|| "failed to open file for reading")?;
        let reader = BufReader::new(file);
        let content = serde_json::from_reader(reader)
            .with_context(|| "failed to read open file contents as expected json struct")?;
        Ok(content)
    }

    fn try_write_to_file(&self, path: impl AsRef<Path>) -> anyhow::Result<()> {
        let file = OpenOptions::new()
            .write(true)
            .truncate(true)
            .create(true)
            .open(path)
            .with_context(|| "failed to create/open file for writing")?;
        let writer = BufWriter::new(file);

        serde_json::to_writer_pretty(writer, &self)
            .with_context(|| "failed to write to created/opened file")
    }

    fn find_target(&self, target: impl AsRef<Path>) -> Option<&TargetBuildActions> {
        self.diffs
            .iter()
            .find(|&curr| curr.target == target.as_ref())
    }

    fn set_target(&mut self, target: &CompileTarget, actions: Stat) {
        let x = self
            .diffs
            .iter()
            .position(|curr| curr.target == target.out_path);

        match x {
            Some(index) => {
                self.diffs[index] = TargetBuildActions {
                    target: target.out_path.clone(),
                    diff: actions,
                };
            }
            None => self.diffs.push(TargetBuildActions {
                target: target.out_path.clone(),
                diff: actions,
            }),
        }
    }
}

#[derive(Serialize, Deserialize, Debug, Default)]
struct TargetBuildActions {
    target: PathBuf,
    diff: Stat,
}

/// Represents the results of stating each file within a directory recursively.
///
/// When two Stat structs are diffed, this instead represents the files and
/// directories that were created between the two stats.
#[derive(Serialize, Deserialize, Debug, Default)]
struct Stat {
    /// List of paths to directories that exist (or were created).
    dirs: Vec<PathBuf>,
    /// List of paths to files that exist (or were created).
    files: Vec<PathBuf>,
}

/// Recursively finds all files and directories that exist at the specific path
/// which should represent a directory.
///
/// PERF: We take a AsRef<Path> but then have to call Into<PathBuf> to push it
/// onto the stack... is there any point taking a impl AsRef<Path> rather than
/// a owned PathBuf?
fn target_stat<D>(dir: D) -> anyhow::Result<Stat>
where
    D: Debug,
    D: AsRef<Path>,
    D: Into<PathBuf>,
{
    let mut stat = Stat {
        dirs: Vec::new(),
        files: Vec::new(),
    };

    let mut stack = vec![dir.into()];

    while let Some(current_dir) = stack.pop() {
        // Read the directory contents
        let entries = fs::read_dir(&current_dir)
            .with_context(|| format!("failed to read directory: {:?}", current_dir))?;

        for entry in entries {
            let entry = entry.with_context(|| "failed to process an entry")?;
            let path = entry.path();

            // Check if it is a directory or a file and push to corresponding list
            if path.is_dir() {
                stat.dirs.push(path.clone());
                stack.push(path.clone());
            } else {
                stat.files.push(path);
            }
        }
    }
    Ok(stat)
}

/// Clean the target directory based on the previously written files as
/// specified via the stat diff.
fn target_clean(actions: &TargetBuildActions) -> anyhow::Result<()> {
    // Remove files
    for file in &actions.diff.files {
        fs::remove_file(file)
            .map(|_| debug!(?file, ?actions.target, "cleaning target: removed file"))
            .with_context(|| format!("failed to delete file: {:?}", file))?;
    }

    // Remove directories. It's important to attempt removing directories
    // after files because directories need to be empty before they can be removed.
    let mut directories = actions.diff.dirs.clone();
    // Sort directories in reverse order to ensure that we attempt to delete
    // subdirectories before their parents
    directories.sort_by(|a, b| b.cmp(a));
    for dir in directories {
        if fs::read_dir(&dir)?.next().is_none() {
            // Check if directory is empty
            fs::remove_dir(&dir)
                .map(|_| debug!(?dir, ?actions.target, "cleaning target: removed dir"))
                .with_context(|| format!("failed to delete directory: {:?}", dir))?;
        } else {
            return Err(anyhow::anyhow!("directory not empty: {:?}", dir));
        }
    }

    Ok(())
}

/// Calculate the diff of two stats where one stat is taken before some action
/// and the other is taken after the action. This calculates the files
/// created by the action.
fn target_stat_diff(before: &Stat, after: &Stat) -> Stat {
    let mut diff = Stat {
        dirs: vec![],
        files: vec![],
    };

    // Create sets for O(1) look-up times
    let before_dirs: std::collections::HashSet<_> = before.dirs.iter().collect();
    let before_files: std::collections::HashSet<_> = before.files.iter().collect();

    // Check for new directories in 'after' that weren't in 'before'
    for dir in &after.dirs {
        if !before_dirs.contains(dir) {
            diff.dirs.push(dir.clone());
        }
    }

    // Check for new files in 'after' that weren't in 'before'
    for file in &after.files {
        if !before_files.contains(file) {
            diff.files.push(file.clone());
        }
    }

    diff
}

/// Persist/write the stat diff to a file. This allows for a targetted clean
/// which only clears generated files.
fn persist_build_actions(path: PathBuf, stats: BuildActions) -> anyhow::Result<()> {
    let file =
        File::create(&path).with_context(|| format!("failed to create file: {:?}", &path))?;
    let writer = BufWriter::new(file);

    serde_json::to_writer_pretty(writer, &stats).with_context(|| "failed to write data to file")?;

    Ok(())
}

fn create_targets_from_templates(config: &Config) -> anyhow::Result<()> {
    for template in &config.templates {
        // 1. Wipe the existing target directory
        match fs::metadata(&template.out_path).ok() {
            Some(md) => {
                debug!(?md, "found existing target directory/file");
                match fs::remove_dir_all(&template.out_path) {
                    Ok(()) => debug!(?template, "cleaned existing target directory"),
                    Err(err) => {
                        error!(?err, ?template, "failed to clean existing target directory")
                    }
                };
            }
            None => {
                debug!("target directory/file doesn't exist, no need to clean wipe it")
            }
        }
        // match fs::remove_dir_all(&template.out_path) {
        //     Ok(()) => {
        //         debug!(
        //             ?template,
        //             "cleared target output directory prior to template copy"
        //         )
        //     }
        //     Err(err) => {
        //         error!(
        //             ?err,
        //             ?template,
        //             "failed to clear target output directory before template copy"
        //         );
        //         return Err(err.into());
        //     }
        // }
        // 2. Copy the template into the target directory
        let src = PathBuf::from(".").join(&template.in_path);
        let dst = PathBuf::from(".").join(&template.out_path);
        match copy_dir_recursive(&src, &dst) {
            Ok(_) => info!(?src, ?dst, ?template, "copied template success"),
            Err(err) => error!(?err, ?src, ?dst, ?template, "failed to copy template"),
        };
    }
    Ok(())
}

fn clean_targets(config: &Config) -> anyhow::Result<()> {
    // 1. Clear the target based on the protov.build.outputs.json
    let mut build_actions = BuildActions::try_read_from_file(BUILD_OUTPUTS_PATH)
        .inspect_err(|err| warn!(?err, "failed to read build outputs from file"))
        .unwrap_or_else(|_| BuildActions::default());

    for target in &config.targets {
        let target_stat_before = target_stat(&target.out_path)?;

        // Clean each target based on the previous build actions
        if let Some(target_build_actions) = build_actions.find_target(&target.out_path) {
            info!(
                ?target_build_actions,
                "cleaning target build outputs from previous build"
            );
            target_clean(target_build_actions);
        } else {
            info!("no previous target build outputs found");
        }

        let target_stat_after = target_stat(&target.out_path)?;
        let target_stat_diff = target_stat_diff(&target_stat_before, &target_stat_after);

        build_actions.set_target(target, target_stat_diff);
    }

    build_actions
        .try_write_to_file(BUILD_OUTPUTS_PATH)
        .with_context(|| "failed to write build outputs to file")?;

    Ok(())
}

/// TODO: Unify this with clean_targets()
fn build_targets(config: &Config) -> anyhow::Result<()> {
    // 1. Clear the target based on the protov.build.lock.json
    let mut build_actions = BuildActions::try_read_from_file(BUILD_OUTPUTS_PATH)
        .inspect_err(|err| warn!(?err, "failed to read build outputs from file"))
        .unwrap_or_else(|_| BuildActions::default());

    for target in &config.targets {
        info!("building target");

        if let Some(target_build_actions) = build_actions.find_target(&target.out_path) {
            info!(
                ?target_build_actions,
                "cleaning target build outputs from previous build"
            );
            target_clean(target_build_actions);
        } else {
            info!("no previous target build outputs found");
        }

        let target_stat_before = target_stat(&target.out_path)?;

        // debug!(?target.out_path, "cleaning existing target directory");
        // match fs::metadata(&target.out_path).ok() {
        //     Some(md) => {
        //         debug!(?md, "found existing target directory/file");
        //         match fs::remove_dir_all(&target.out_path) {
        //             Ok(()) => debug!("cleaned existing target directory"),
        //             Err(err) => error!(?err, "failed to clean existing target directory"),
        //         };
        //     }
        //     None => {
        //         debug!("target directory/file doesn't exist, no need to clean wipe it")
        //     }
        // }

        for source_name in &target.sources {
            info!("building target from source");
            // buf generate ../protos --template buf.gen.yaml
            let mut cmd = Command::new("buf");
            cmd.arg("generate")
                .arg(PathBuf::from("./vendor").join(source_name))
                .arg("--template")
                .arg(&target.out_gen_path)
                .arg("--output")
                .arg(&target.out_path)
                .current_dir(".");
            let cmd_res = cmd.output().context("failed to generate source")?;
            debug!(?cmd, ?cmd_res, "executed command");
        }

        let target_stat_after = target_stat(&target.out_path)?;
        let target_stat_diff = target_stat_diff(&target_stat_before, &target_stat_after);

        build_actions.set_target(target, target_stat_diff);
    }

    build_actions
        .try_write_to_file(BUILD_OUTPUTS_PATH)
        .with_context(|| "failed to write build outputs to file")?;

    Ok(())
}

fn clean_vendor() -> anyhow::Result<()> {
    fs::remove_dir_all(PathBuf::from(".").join("vendor"))?;
    fs::create_dir(PathBuf::from(".").join("vendor"));
    Ok(())
}

/// Populate the vendor directory from the system cache.
///
/// TODO: Using cache doesn't really make sense since we need to check for the
/// latest commit in the remote, or we should just copy again for local to get
/// the latest changes. The only time it could be useful is if the developer
/// knows there's no changes in the remote in which case a git command (~2 sec)
/// can be avoided.
fn populate_vendor(config: &Config, cache_index: &CacheIndex) {
    for source in &config.sources {
        match &cache_index.lookup_cached_source(source) {
            Some(cached_source) => {
                let src = match &cached_source.source {
                    Source::Local(local) => cached_source.path.clone(),
                    // TODO: This will copy .git if we're into the root like stellarstation-api is.
                    // Should we instead filter for only .proto and .buf.yaml files in this copy?
                    Source::Remote(remote) => cached_source.path.join(&remote.path),
                };

                let dst = match &cached_source.source {
                    Source::Local(local) => PathBuf::from(".").join("vendor").join(&local.name),
                    Source::Remote(remote) => PathBuf::from(".").join("vendor").join(&remote.name),
                };

                info!(
                    ?source,
                    ?cached_source,
                    ?src,
                    ?dst,
                    "populating source (dst) from cache (src)"
                );
                let log = copy_dir_recursive(src, dst).expect("failed to remote tree to vendor");
                info!(?log, "log of remote tree copy to vendor")
            }
            None => match &source {
                Source::Local(local) => {
                    let src = expand_path(&local.path);
                    let dst = PathBuf::from(".").join("vendor").join(&local.name);
                    debug!(?src, ?dst, "attempting to copy local directory");
                    let log =
                        copy_dir_recursive(src, dst).expect("failed to copy local tree to vendor");
                    info!(?log, "log of local tree copy to vendor")
                }
                Source::Remote(remote) => {
                    error!(?remote, "failed to populate remote source from cache during sync, should already exist");
                }
            },
        }
    }
}

#[derive(Debug)]
struct ActionLog {
    actions: Vec<Action>,
}

#[derive(Debug)]
enum Action {
    CopyFile {
        src: PathBuf,
        dst: PathBuf,
    },
    CreateDir {
        dir: PathBuf,
    },
    /// Some path that was ignored.
    ///
    /// The current filters are:
    /// - include: *.proto, buf.yaml, buf.lock
    /// - ignore: .git/*
    IgnorePath {
        src: PathBuf,
    },
}

fn copy_dir_recursive<U: AsRef<Path>, V: AsRef<Path>>(
    src: U,
    dst: V,
) -> Result<ActionLog, std::io::Error> {
    let src_path = src.as_ref();
    let dst_path = dst.as_ref();
    if !src_path.is_dir() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "Source is not a directory",
        ));
    }

    let mut log = ActionLog {
        actions: Vec::new(),
    };

    let mut stack = Vec::new();
    stack.push(PathBuf::from(src_path));

    // This is the number of components in the src path to strip to create relative paths
    let input_root_length = src_path.components().count();

    while let Some(working_path) = stack.pop() {
        debug!("walking directory: {:?}", &working_path);

        // Generate a relative path based on the src directory structure
        let relative_path: PathBuf = working_path.components().skip(input_root_length).collect();

        // Create a corresponding path in the dst directory
        let target_path = dst_path.join(&relative_path);
        if !target_path.exists() {
            debug!("creating directory: {:?}", target_path);
            fs::create_dir_all(&target_path)?;
            log.actions.push(Action::CreateDir {
                dir: target_path.clone(),
            });
        }

        // Iterate over the contents of the current directory
        for entry in fs::read_dir(&working_path)? {
            let entry = entry?;
            let path = entry.path();

            match path.is_dir() {
                // In general, we want to recurse into directories by appending to the stack
                true => {
                    // We want to filter out the .git directories to avoid:
                    //
                    // hint: You've added another git repository inside your current repository.
                    // hint: Clones of the outer repository will not contain the contents of
                    // hint: the embedded repository and will not know how to obtain it.
                    if path.file_name().unwrap_or_default().to_os_string() != ".git" {
                        stack.push(path);
                    } else {
                        debug!("ignoring dir due to filters: {:?}", &path);
                    }
                }
                // But for files, we want to copy the file as they're like a leaf node
                false => match path.file_name() {
                    Some(filename) => {
                        let proto = path.extension().unwrap_or_default() == "proto";
                        let buf_yaml = path.file_name().unwrap_or_default() == "buf.yaml";
                        let buf_lock = path.file_name().unwrap_or_default() == "buf.lock";

                        match proto || buf_yaml || buf_lock {
                            true => {
                                let dest_path = target_path.join(filename);
                                debug!("copying file: {:?} -> {:?}", &path, &dest_path);

                                // let exists = fs::metadata(&dest_path).is_ok();
                                fs::copy(&path, &dest_path)?;

                                log.actions.push(Action::CopyFile {
                                    src: path.to_path_buf(),
                                    dst: dest_path.to_path_buf(),
                                });
                            }
                            false => {
                                debug!("ignoring file due to filters: {:?}", &path);
                                log.actions.push(Action::IgnorePath { src: path })
                            }
                        }
                    }
                    None => {
                        warn!("failed to copy file: {:?}", path);
                    }
                },
            }
        }
    }

    Ok(log)
}

fn cache(config: &Config) -> anyhow::Result<CacheIndex> {
    let cachedir = cache_dir().context("no cache dir exists")?.join("protov");
    info!(?cachedir, "using cachedir");

    match std::fs::create_dir_all(&cachedir) {
        Ok(()) => {
            println!("created cache directory: {:?}", &cachedir);
        }
        Err(e) => {
            println!("failed to cache directory: {:?}: {:?}", cachedir, e);
        }
    }

    let mut cache_index = CacheIndex::try_from_file(cachedir.join("cache.json"))
        .map(|index| {
            info!("read cache index file");
            index
        })
        .unwrap_or_else(|err| {
            warn!(?err, "failed to read cache index, creating new index");
            CacheIndex::default()
        });

    for source in &config.sources {
        match cache_index.lookup_cached_source(source) {
            Some(cached) => info!(?cached, "found cached entry"),
            None => match source {
                Source::Remote(source) => match cache_remote_source(source, &cachedir) {
                    Ok(entry) => cache_index.entries.push(entry),
                    Err(err) => error!(?err, "failed to cache entry"),
                },
                Source::Local(source) => {
                    let res = Command::new("ls")
                        .current_dir(expand_path(&source.path))
                        .output()
                        .unwrap();
                }
            },
        }
    }

    info!(?cache_index, "finished caching, showing index");
    match write_cache_index(cachedir.join("cache.json"), &cache_index) {
        Ok(()) => info!("wrote updated cache index file"),
        Err(err) => error!(?err, "failed to write updated cache index file"),
    };

    Ok(cache_index)
}

// TODO: Check commit using: git ls-remote origin <tag>
// This will slow down the sync since we can't use the cache anymore without first checking that
// the latest commit still matches.
fn cache_remote_source(source: &RemoteSource, cache: &PathBuf) -> anyhow::Result<CacheEntry> {
    let uuid = Uuid::new_v4().to_string();

    match source.path.is_empty() {
        true => {
            let cmd = Command::new("git")
                .arg("clone")
                .arg("-n")
                .arg("--depth=1")
                .arg("--branch")
                .arg(&source.tag)
                .arg(&source.url)
                .arg(&uuid) // The output directory
                .current_dir(cache)
                .output()
                .context("failed to clone")?;
            if !cmd.status.success() {
                return Err(anyhow!("(1) invalid exit status: {:?}", cmd));
            }

            let c3 = Command::new("git")
                .arg("checkout")
                .arg(&source.tag)
                .current_dir(cache.join(&uuid))
                .output()
                .context("failed to checkout")?;
            if !cmd.status.success() {
                return Err(anyhow!("(3) invalid exit status: {:?}", cmd));
            }
        }
        false => {
            let cmd = Command::new("git")
                .arg("clone")
                .arg("-n")
                .arg("--depth=1")
                .arg("--filter=blob:none") // tree:0
                .arg("--branch")
                .arg(&source.tag)
                .arg(&source.url)
                .arg(&uuid) // The output directory
                .current_dir(cache)
                .output()
                .context("failed to clone")?;
            if !cmd.status.success() {
                return Err(anyhow!("(1) invalid exit status: {:?}", cmd));
            }

            let cmd = Command::new("git")
                .arg("sparse-checkout")
                .arg("set")
                .arg("--no-cone")
                .arg(&source.path)
                .current_dir(cache.join(&uuid))
                .output()
                .context("failed to set sparse-checkout")?;
            if !cmd.status.success() {
                return Err(anyhow!("(2) invalid exit status: {:?}", cmd));
            }

            let c3 = Command::new("git")
                .arg("checkout")
                .arg(&source.tag)
                .current_dir(cache.join(&uuid))
                .output()
                .context("failed to checkout")?;
            if !cmd.status.success() {
                return Err(anyhow!("(3) invalid exit status: {:?}", cmd));
            }
        }
    };

    Ok(CacheEntry {
        path: cache.join(uuid),
        source: Source::Remote(source.clone()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use indoc::indoc;
    use tempdir::TempDir;
    use test_log::test;

    #[test]
    fn test() -> anyhow::Result<()> {
        let tmpfs = TempDir::new("protov")?;

        let config = std::fs::write(
            tmpfs.path().join("protov.toml"),
            indoc! { r#"
                [[source]]
                type = "remote"
                url = "https://github.com/org/public"
                tag = "v1.2.3"
                path = "protos"
                name = "public"

                [[source]]
                type = "local"
                path = "~/org/monorepo/protos"
                name = "monorepo"

                [[target]]
                out_path = "./go"
                out_gen_path = "./go/buf.go.yaml"
                sources = ["support-1", "support-2"]
            "#},
        );

        let config = parse(tmpfs.path().join("protov.toml"));

        assert_eq!(
            config.unwrap(),
            Config {
                sources: vec![
                    Source::Remote(RemoteSource {
                        url: "https://github.com/org/public".into(),
                        tag: "v1.2.3".into(),
                        path: "protos".into(),
                        name: "public".into(),
                    }),
                    Source::Local(LocalSource {
                        path: "~/org/monorepo/protos".into(),
                        name: "monorepo".into(),
                    }),
                ],
                targets: vec![CompileTarget {
                    out_path: PathBuf::from("./go"),
                    out_gen_path: PathBuf::from("./go/buf.go.yaml"),
                    sources: vec!["support-1".into(), "support-2".into()]
                },],
                templates: vec![],
            }
        );

        Ok(())
    }
}
