//! The three config layers (chapter 1) as typed structs, and the `config` event (10.3).
//!
//! Every struct refuses unknown keys. The JSON Schemas are `schemas/config-*.schema.json`;
//! the test suite checks every example parses here.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::canonical::hash_value;
use crate::event::Kind;
use crate::ledger::{Dedupe, Recorded, Tx};
use crate::{Error, Result};

pub type Labels = Vec<String>;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TypeLabels {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bug: Option<Labels>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub todo: Option<Labels>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decision: Option<Labels>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub docs: Option<Labels>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PriorityLabels {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub p0: Option<Labels>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub p1: Option<Labels>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub p2: Option<Labels>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub p3: Option<Labels>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModifierLabels {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub needs_decision: Option<Labels>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub in_progress: Option<Labels>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub self_contained: Option<Labels>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub partial: Option<Labels>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub umbrella: Option<Labels>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub good_first_issue: Option<Labels>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub help_wanted: Option<Labels>,
}

/// A label translation (1.3): canonical value → the repo's label names, first preferred.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Translate {
    #[serde(default, rename = "type", skip_serializing_if = "Option::is_none")]
    pub type_: Option<TypeLabels>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub priority: Option<PriorityLabels>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub modifier: Option<ModifierLabels>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub area: Option<BTreeMap<String, Labels>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Gate {
    pub run: String,
    pub minutes: u32,
}

/// A launch recipe (7.6).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Recipe {
    /// The size of the recipe's pool of launch slots (10.7).
    pub capacity: u32,
    pub workspace: String,
    pub release: String,
    pub gate: BTreeMap<String, Gate>,
    pub land: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub process: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Defaults {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub translate: Option<Translate>,
    /// The recipe a repo's launches use when its entry names none (7.6).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub launch: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RepoEntry {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub local_path: Option<String>,
    /// The recipe this repo's launches use.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub launch: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub translate: Option<Translate>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub docs: Option<Vec<String>>,
    /// The repo's own way to triage and batch closures, relative to `local_path`; a
    /// recommender applies it (5.8). Without one it follows the generic procedure.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub campaign_skill: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Forks {
    pub owners: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ForkEntry {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upstream: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fixes: Option<Vec<String>>,
}

/// `<root>/repos.toml`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Registry {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schema: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub defaults: Option<Defaults>,
    pub repo: BTreeMap<String, RepoEntry>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub groups: BTreeMap<String, Vec<String>>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub launch: BTreeMap<String, Recipe>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub forks: Option<Forks>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub fork: BTreeMap<String, ForkEntry>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Writes {
    #[serde(rename = "labels")]
    Labels,
    #[serde(rename = "labels-and-comments")]
    LabelsAndComments,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Member {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exclude: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filter: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub also: Option<Vec<u64>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub read_only: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub writes: Option<Writes>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allow_public_comments: Option<bool>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TimeLimits {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub p0: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub p1: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub p2: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub p3: Option<u32>,
}

/// `<root>/projects/<project>.toml`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Project {
    pub schema: u32,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dashboard: Option<String>,
    /// The page user id whose decisions count (10.5).
    pub owner_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub maintainers: Option<Vec<String>>,
    pub member: Vec<Member>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub groups: BTreeMap<String, Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_view: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub history_days: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sync_interval_minutes: Option<u32>,
    /// How many days the page's Recently closed list looks back (5.8).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recently_closed_days: Option<u32>,
    /// The id of the app's scheduled task that runs this project's sync job, which `sync-now`
    /// prints for the calling session to run now (7.2).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sync_task: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_writes_per_run: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_batch: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub time_limits_days: Option<TimeLimits>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub routing: Option<BTreeMap<String, crate::docs::Routing>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub voice: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disclose_drafting: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub demand_threshold: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gate_minutes_per_point: Option<u32>,
    /// How many days a closure answer holds before triage asks it again (triage spec 2.2).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub closure_recheck_days: Option<u32>,
}

/// `.issue-dashboard.toml` at the root of a repo you own.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RepoFile {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schema: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub areas: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub launch: BTreeMap<String, Recipe>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub docs: Option<Vec<String>>,
}

/// The configuration in force: all three layers as read from the root and the checkouts.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConfigSet {
    pub registry: Registry,
    pub projects: BTreeMap<String, Project>,
    /// Repo files found in the registry's local checkouts, by owner/name.
    pub repo_files: BTreeMap<String, RepoFile>,
}

/// Parses a TOML file into `T`, naming the file on failure.
pub fn parse_toml<T: serde::de::DeserializeOwned>(path: &Path) -> Result<T> {
    let text = fs::read_to_string(path).map_err(|e| Error::Config {
        file: path.to_path_buf(),
        reason: e.to_string(),
    })?;
    toml::from_str(&text).map_err(|e| Error::Config {
        file: path.to_path_buf(),
        reason: e.to_string(),
    })
}

/// Parses a config layer, checking it against its schema first (`schemas/<stem>.schema.json`),
/// so what serde's types cannot express (a capacity of at least 1, `exclude` only with `group`,
/// a filter without the tracker's own qualifiers) is refused, naming the file.
fn parse_layer<T: serde::de::DeserializeOwned>(path: &Path, stem: &str) -> Result<T> {
    let table: toml::Table = parse_toml(path)?;
    let json = serde_json::to_value(&table)?;
    let errors = crate::schema::errors(stem, &json);
    if !errors.is_empty() {
        return Err(Error::Config {
            file: path.to_path_buf(),
            reason: errors.join("; "),
        });
    }
    serde_json::from_value(json).map_err(|e| Error::Config {
        file: path.to_path_buf(),
        reason: e.to_string(),
    })
}

impl ConfigSet {
    /// Reads `<root>/repos.toml`, `<root>/projects/*.toml`, and the repo file of every
    /// registry entry with a `local_path` (a missing repo file is fine; 1.1).
    pub fn load(root: &Path) -> Result<ConfigSet> {
        let registry_path = root.join("repos.toml");
        if !registry_path.exists() {
            return Err(Error::Config {
                file: registry_path,
                reason: "the repo registry is missing".into(),
            });
        }
        let registry: Registry = parse_layer(&registry_path, "config-registry")?;
        if let Some((repo, _)) = registry
            .repo
            .iter()
            .find(|(_, e)| e.campaign_skill.is_some() && e.local_path.is_none())
        {
            return Err(Error::Config {
                file: registry_path,
                reason: format!("{repo}: campaign_skill is a path in the repo's checkout, so it needs local_path"),
            });
        }

        let mut projects = BTreeMap::new();
        let dir = root.join("projects");
        if dir.exists() {
            for entry in fs::read_dir(&dir)? {
                let path = entry?.path();
                if path.extension().and_then(|e| e.to_str()) != Some("toml") {
                    continue;
                }
                let name = path.file_stem().unwrap().to_string_lossy().into_owned();
                projects.insert(name, parse_layer::<Project>(&path, "config-project")?);
            }
        }

        let mut repo_files = BTreeMap::new();
        for (repo, entry) in &registry.repo {
            if let Some(local) = &entry.local_path {
                let path = expand_home(local).join(".issue-dashboard.toml");
                if path.exists() {
                    repo_files.insert(repo.clone(), parse_layer::<RepoFile>(&path, "config-repo-file")?);
                }
            }
        }
        Ok(ConfigSet {
            registry,
            projects,
            repo_files,
        })
    }

    /// The configuration's content hash, over its canonical JSON.
    pub fn content_hash(&self) -> String {
        hash_value(&serde_json::to_value(self).expect("config serialises"))
    }

    /// Records a `config` event if this configuration differs from the last one recorded.
    pub fn record(&self, tx: &mut Tx<'_>) -> Result<Recorded> {
        let content = serde_json::to_value(self)?;
        let hash = hash_value(&content);
        tx.record_valid(
            Kind::Config,
            "config",
            json!({"content_hash": hash}),
            json!({"content_hash": hash, "content": content}),
            Dedupe::Latest,
        )
    }

    /// The size of the pool of the recipe named `name`. The built-in read-only pool
    /// ([`READ_ONLY_RECIPE`]) has one slot unless a recipe of that name says otherwise.
    pub fn capacity(&self, name: &str) -> Result<u32> {
        match self.recipe(name) {
            Ok(r) => Ok(r.capacity),
            Err(Error::Refused(_)) if name == READ_ONLY_RECIPE && !self.defines_recipe(name) => Ok(1),
            Err(e) => Err(e),
        }
    }

    fn defines_recipe(&self, name: &str) -> bool {
        self.registry.launch.contains_key(name) || self.repo_files.values().any(|f| f.launch.contains_key(name))
    }

    /// The recipe a repo's launches use: its entry's, else the registry default.
    pub fn recipe_name_for(&self, repo: &str) -> Option<&str> {
        self.registry
            .repo
            .get(repo)
            .and_then(|e| e.launch.as_deref())
            .or_else(|| self.registry.defaults.as_ref().and_then(|d| d.launch.as_deref()))
    }

    /// The docs to name in a brief for `repo` (7.5): its repo file's, else its entry's.
    pub fn docs_for(&self, repo: &str) -> Vec<String> {
        self.repo_files
            .get(repo)
            .and_then(|f| f.docs.clone())
            .or_else(|| self.registry.repo.get(repo).and_then(|e| e.docs.clone()))
            .unwrap_or_default()
    }

    /// The repo's local checkout, if the registry names one.
    pub fn local_path(&self, repo: &str) -> Option<PathBuf> {
        self.registry
            .repo
            .get(repo)
            .and_then(|e| e.local_path.as_deref())
            .map(expand_home)
    }

    /// The repo's campaign skill as a full path in its checkout, if one is configured.
    pub fn campaign_skill(&self, repo: &str) -> Option<PathBuf> {
        let skill = self.registry.repo.get(repo)?.campaign_skill.as_deref()?;
        Some(self.local_path(repo)?.join(skill))
    }

    /// The recipe named `name`: from the registry or a repo file. Two different definitions
    /// under one name are refused, since a pool must have one capacity.
    pub fn recipe(&self, name: &str) -> Result<&Recipe> {
        let mut found: Vec<(String, &Recipe)> = Vec::new();
        if let Some(r) = self.registry.launch.get(name) {
            found.push(("repos.toml".into(), r));
        }
        for (repo, file) in &self.repo_files {
            if let Some(r) = file.launch.get(name) {
                found.push((format!("{repo}'s .issue-dashboard.toml"), r));
            }
        }
        match found.as_slice() {
            [] => Err(Error::refused(format!("no launch recipe named {name}"))),
            [(_, first), rest @ ..] => {
                if let Some((where_, _)) = rest.iter().find(|(_, r)| r != first) {
                    return Err(Error::refused(format!(
                        "launch recipe {name} is defined differently in {} and {where_}",
                        found[0].0
                    )));
                }
                Ok(first)
            }
        }
    }
}

/// One repo as a project sees it (1.4), once groups, exclusions and own entries are resolved.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct View {
    pub repo: String,
    pub filter: Option<String>,
    pub also: Vec<u64>,
    pub read_only: bool,
}

impl View {
    fn of(repo: &str, m: &Member) -> View {
        let mut also = m.also.clone().unwrap_or_default();
        also.sort_unstable();
        also.dedup();
        View {
            repo: repo.to_string(),
            filter: m.filter.clone().filter(|f| !f.trim().is_empty()),
            also,
            read_only: m.read_only.unwrap_or(false),
        }
    }
}

impl ConfigSet {
    /// The project named by its config file stem.
    pub fn project(&self, name: &str) -> Result<&Project> {
        self.projects
            .get(name)
            .ok_or_else(|| Error::refused(format!("no project {name} (projects/{name}.toml)")))
    }

    /// Every repo `project` tracks, with its view (1.4): a repo's own entry wins over a group;
    /// a repo two groups add with different settings, two `repo =` entries for one repo, an
    /// unknown group and a repo without a registry entry are refused, naming the file.
    pub fn views(&self, project: &str) -> Result<BTreeMap<String, View>> {
        let p = self.project(project)?;
        let file = PathBuf::from(format!("projects/{project}.toml"));
        let refuse = |reason: String| Error::Config {
            file: file.clone(),
            reason,
        };
        let mut out: BTreeMap<String, View> = BTreeMap::new();
        for m in p.member.iter().filter(|m| m.repo.is_some()) {
            let repo = m.repo.as_deref().expect("filtered");
            if out.insert(repo.to_string(), View::of(repo, m)).is_some() {
                return Err(refuse(format!("two members name {repo}")));
            }
        }
        let own: Vec<String> = out.keys().cloned().collect();
        let mut from_group: BTreeMap<String, (String, View)> = BTreeMap::new();
        for m in p.member.iter().filter(|m| m.group.is_some()) {
            let group = m.group.as_deref().expect("filtered");
            let repos = p
                .groups
                .get(group)
                .or_else(|| self.registry.groups.get(group))
                .ok_or_else(|| refuse(format!("member group {group} is not defined")))?;
            let exclude = m.exclude.clone().unwrap_or_default();
            for repo in repos.iter().filter(|r| !exclude.contains(r) && !own.contains(r)) {
                let view = View::of(repo, m);
                match from_group.get(repo) {
                    Some((other, v)) if v != &view => {
                        return Err(refuse(format!(
                            "{repo} arrives through groups {other} and {group} with different settings"
                        )));
                    }
                    Some(_) => {}
                    None => {
                        from_group.insert(repo.clone(), (group.to_string(), view));
                    }
                }
            }
        }
        out.extend(from_group.into_iter().map(|(r, (_, v))| (r, v)));
        if let Some(repo) = out.keys().find(|r| !self.registry.repo.contains_key(*r)) {
            return Err(refuse(format!("member {repo} has no entry in repos.toml")));
        }
        Ok(out)
    }
}

/// The pool a read-only launch (a recommender) takes its slot from (10.7).
pub const READ_ONLY_RECIPE: &str = "read-only";

/// Expands a leading `~/` to the home directory.
pub fn expand_home(path: &str) -> PathBuf {
    match (path.strip_prefix("~/"), std::env::var_os("HOME")) {
        (Some(rest), Some(home)) => PathBuf::from(home).join(rest),
        _ => PathBuf::from(path),
    }
}
