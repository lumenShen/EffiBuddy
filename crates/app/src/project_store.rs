//! Durable project registry: named containers with ordered folders.
//!
//! `projects.json` is separate from `config.json` (which is rewritten on every
//! layout tweak) and from `sessions/*.json` (one transcript per file). A
//! project owns an ordered root list with exactly one primary folder; sessions
//! group by the project id, never by path, so the same folder may appear in
//! several projects and a session keeps its own immutable cwd.
//!
//! Legacy data (`AppConfig.projects` and the single-path `SessionState.project`
//! tag) is migrated once on startup and reconciled on every launch, so a lost
//! registry never makes conversations disappear from the sidebar.

use std::path::{Component, Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::config::base_dir;

pub const PROJECTS_SCHEMA_VERSION: u32 = 1;

/// One folder inside a project. Order in [`Project::roots`] is display order.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectRoot {
    pub path: PathBuf,
    /// Exactly one root per non-empty project carries this flag. New sessions
    /// run with this folder as their cwd and tool sandbox primary root.
    #[serde(default)]
    pub is_primary: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Project {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub roots: Vec<ProjectRoot>,
    /// Manual order; the sidebar also decorates with recency, but new projects
    /// append after existing ones.
    #[serde(default)]
    pub position: u32,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl Project {
    pub fn primary(&self) -> Option<&Path> {
        self.roots
            .iter()
            .find(|root| root.is_primary)
            .map(|root| root.path.as_path())
            .or_else(|| self.roots.first().map(|root| root.path.as_path()))
    }

    pub fn root_paths(&self) -> Vec<PathBuf> {
        self.roots.iter().map(|root| root.path.clone()).collect()
    }

    /// Roots other than the session cwd, in project order. This is what the
    /// agent receives as additional working directories.
    pub fn additional_roots(&self, cwd: &Path) -> Vec<PathBuf> {
        self.roots
            .iter()
            .map(|root| root.path.clone())
            .filter(|path| path.as_path() != cwd)
            .collect()
    }

    pub fn contains_root(&self, path: &Path) -> bool {
        self.roots.iter().any(|root| root.path == path)
    }

    pub fn is_multi_root(&self) -> bool {
        self.roots.len() > 1
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectFile {
    #[serde(default = "default_schema_version")]
    pub schema_version: u32,
    #[serde(default)]
    pub projects: Vec<Project>,
}

fn default_schema_version() -> u32 {
    PROJECTS_SCHEMA_VERSION
}

pub struct ProjectStore {
    path: PathBuf,
    file: ProjectFile,
    /// Set when the on-disk file could not be parsed or is newer than this
    /// build. The file is left intact; the registry starts empty and legacy
    /// sessions are reconciled into fresh projects.
    load_error: Option<String>,
}

impl ProjectStore {
    pub fn new() -> Self {
        Self::at(Self::path())
    }

    /// Test/preview store rooted at an explicit file path.
    pub fn at(path: impl Into<PathBuf>) -> Self {
        let path = path.into();
        if !path.exists() {
            return Self {
                path,
                file: ProjectFile {
                    schema_version: PROJECTS_SCHEMA_VERSION,
                    projects: Vec::new(),
                },
                load_error: None,
            };
        }
        match std::fs::read_to_string(&path) {
            Ok(contents) => match serde_json::from_str::<ProjectFile>(&contents) {
                Ok(mut file) => {
                    let mut error = None;
                    if file.schema_version > PROJECTS_SCHEMA_VERSION {
                        error = Some(format!(
                            "projects.json 的版本 {} 高于当前支持的 {}；已按会话重建项目列表，文件未被覆盖。",
                            file.schema_version, PROJECTS_SCHEMA_VERSION
                        ));
                        file.projects.clear();
                    }
                    let mut store = Self {
                        path,
                        file,
                        load_error: error,
                    };
                    if store.load_error.is_none() && store.normalize() {
                        let _ = store.save();
                    }
                    store
                }
                Err(error) => Self {
                    path,
                    file: ProjectFile {
                        schema_version: PROJECTS_SCHEMA_VERSION,
                        projects: Vec::new(),
                    },
                    load_error: Some(format!(
                        "projects.json 无法解析（{error}）；已按会话重建项目列表，原文件保留。"
                    )),
                },
            },
            Err(error) => Self {
                path,
                file: ProjectFile {
                    schema_version: PROJECTS_SCHEMA_VERSION,
                    projects: Vec::new(),
                },
                load_error: Some(format!("无法读取 projects.json：{error}")),
            },
        }
    }

    pub fn path() -> PathBuf {
        base_dir().join("projects.json")
    }

    pub fn load_error(&self) -> Option<&str> {
        self.load_error.as_deref()
    }

    pub fn projects(&self) -> &[Project] {
        &self.file.projects
    }

    pub fn is_empty(&self) -> bool {
        self.file.projects.is_empty()
    }

    pub fn by_id(&self, id: &str) -> Option<&Project> {
        self.file.projects.iter().find(|project| project.id == id)
    }

    /// Project owning `path` as one of its roots. Primary matches win so a
    /// legacy session grouping is deterministic when a folder belongs to
    /// several projects.
    pub fn project_for_path(&self, path: &Path) -> Option<&Project> {
        let normalized = normalize_root(path);
        self.file
            .projects
            .iter()
            .find(|project| {
                project
                    .primary()
                    .is_some_and(|primary| primary == normalized.as_path())
            })
            .or_else(|| {
                self.file
                    .projects
                    .iter()
                    .find(|project| project.contains_root(&normalized))
            })
    }

    /// Ordered roots for a session: project id first, legacy project path
    /// second. `None` means the session is a plain task.
    pub fn project_for_session(
        &self,
        project_id: Option<&str>,
        legacy_project: Option<&Path>,
    ) -> Option<&Project> {
        if let Some(id) = project_id
            && let Some(project) = self.by_id(id)
        {
            return Some(project);
        }
        legacy_project.and_then(|path| self.project_for_path(path))
    }

    /// Create a project from 1..n folders. The first folder becomes primary.
    /// Duplicate paths are dropped; duplicate names get a numeric suffix.
    pub fn create(&mut self, folders: &[PathBuf]) -> Result<Project, String> {
        let roots = normalize_roots(folders);
        if roots.is_empty() {
            return Err("项目至少需要一个文件夹。".to_string());
        }
        let now = Utc::now();
        let name = self.unique_name(project_default_name(&roots[0]), None);
        let position = self
            .file
            .projects
            .iter()
            .map(|project| project.position)
            .max()
            .map(|position| position.saturating_add(1))
            .unwrap_or(0);
        let project = Project {
            id: effibuddy_agent_core::ids::new_id(),
            name,
            roots: roots
                .into_iter()
                .enumerate()
                .map(|(index, path)| ProjectRoot {
                    path,
                    is_primary: index == 0,
                })
                .collect(),
            position,
            created_at: now,
            updated_at: now,
        };
        self.file.projects.push(project.clone());
        Ok(project)
    }

    /// Replace name and folders of an existing project. Roots are normalized
    /// and deduplicated; the primary folder is kept when it still exists,
    /// otherwise the first folder is promoted. Existing sessions are never
    /// touched.
    pub fn update(
        &mut self,
        id: &str,
        name: &str,
        folders: &[PathBuf],
        primary: Option<&Path>,
    ) -> Result<(), String> {
        let roots = normalize_roots(folders);
        if roots.is_empty() {
            return Err("项目至少需要一个文件夹。".to_string());
        }
        let name = name.trim();
        let computed_name = if name.is_empty() {
            project_default_name(&roots[0])
        } else {
            self.unique_name(name.to_string(), Some(id))
        };
        let Some(project) = self
            .file
            .projects
            .iter_mut()
            .find(|project| project.id == id)
        else {
            return Err("项目不存在。".to_string());
        };
        let primary_path = primary
            .map(normalize_root)
            .filter(|path| roots.iter().any(|root| root == path))
            .or_else(|| {
                project
                    .primary()
                    .filter(|path| roots.iter().any(|root| root == *path))
                    .map(Path::to_path_buf)
            })
            .unwrap_or_else(|| roots[0].clone());
        project.name = computed_name;
        project.roots = roots
            .into_iter()
            .map(|path| ProjectRoot {
                is_primary: path == primary_path,
                path,
            })
            .collect();
        project.updated_at = Utc::now();
        Ok(())
    }

    pub fn remove(&mut self, id: &str) -> bool {
        let before = self.file.projects.len();
        self.file.projects.retain(|project| project.id != id);
        self.file.projects.len() != before
    }

    /// One-time migration from the legacy registries: `config.projects`
    /// entries (manual, keep their order) followed by project paths observed
    /// in sessions (most recent first). Returns `legacy path → project id` so
    /// the caller can rewrite persisted sidebar collapse keys.
    pub fn migrate_legacy(
        &mut self,
        config_projects: &[PathBuf],
        session_paths: &[PathBuf],
    ) -> Vec<(PathBuf, String)> {
        let mut mapping = Vec::new();
        for path in config_projects.iter().chain(session_paths.iter()) {
            let normalized = normalize_root(path);
            if let Some(project) = self.project_for_path(&normalized) {
                mapping.push((normalized, project.id.clone()));
                continue;
            }
            if let Ok(project) = self.create(std::slice::from_ref(&normalized)) {
                mapping.push((normalized, project.id));
            }
        }
        mapping
    }

    /// Add a single-root project for any session folder the registry does not
    /// know yet. Keeps the sidebar complete when `projects.json` is missing,
    /// older than the sessions, or was edited by hand.
    pub fn reconcile(&mut self, session_paths: &[PathBuf]) -> bool {
        let mut changed = false;
        for path in session_paths {
            let normalized = normalize_root(path);
            if self.project_for_path(&normalized).is_some() {
                continue;
            }
            if self.create(std::slice::from_ref(&normalized)).is_ok() {
                changed = true;
            }
        }
        changed
    }

    pub fn save(&self) -> Result<(), String> {
        let Some(parent) = self.path.parent() else {
            return Err("projects.json 路径无效".to_string());
        };
        std::fs::create_dir_all(parent).map_err(|error| format!("无法创建数据目录：{error}"))?;
        let json = serde_json::to_string_pretty(&self.file)
            .map_err(|error| format!("无法序列化 projects.json：{error}"))?;
        let temporary = self.path.with_extension("json.tmp");
        std::fs::write(&temporary, json)
            .map_err(|error| format!("无法写入 {}：{error}", temporary.display()))?;
        if self.path.exists() {
            let _ = std::fs::remove_file(&self.path);
        }
        std::fs::rename(&temporary, &self.path)
            .map_err(|error| format!("无法完成 {} 写入：{error}", self.path.display()))
    }

    /// Restore invariants after loading (dedupe roots, exactly one primary).
    /// Returns whether anything changed and needs persisting.
    fn normalize(&mut self) -> bool {
        let before = serde_json::to_string(&self.file).unwrap_or_default();
        for project in &mut self.file.projects {
            let paths = normalize_roots(&project.root_paths());
            let primary = project
                .primary()
                .filter(|path| paths.iter().any(|candidate| candidate == path))
                .map(Path::to_path_buf)
                .or_else(|| paths.first().cloned());
            project.roots = paths
                .into_iter()
                .map(|path| ProjectRoot {
                    is_primary: Some(&path) == primary.as_ref(),
                    path,
                })
                .collect();
            if project.name.trim().is_empty() {
                project.name = project
                    .primary()
                    .map(project_default_name)
                    .unwrap_or_else(|| "项目".to_string());
            }
        }
        // Stable ids survive manual edits; drop accidental duplicates.
        let mut seen = std::collections::HashSet::new();
        self.file
            .projects
            .retain(|project| seen.insert(project.id.clone()));
        let after = serde_json::to_string(&self.file).unwrap_or_default();
        before != after
    }

    fn unique_name(&self, base: String, except_id: Option<&str>) -> String {
        let base = base.trim().to_string();
        let base = if base.is_empty() {
            "项目".to_string()
        } else {
            base
        };
        let taken = |candidate: &str| {
            self.file.projects.iter().any(|project| {
                Some(project.id.as_str()) != except_id
                    && project.name.eq_ignore_ascii_case(candidate)
            })
        };
        if !taken(&base) {
            return base;
        }
        for index in 2..1000 {
            let candidate = format!("{base} ({index})");
            if !taken(&candidate) {
                return candidate;
            }
        }
        base
    }
}

impl Default for ProjectStore {
    fn default() -> Self {
        Self::new()
    }
}

/// Stable sidebar collapse key for one project id.
pub fn project_group_key(id: &str) -> String {
    format!("project:{id}")
}

/// Name of a project folder: last path component, falling back to the full
/// path when the folder has no usable name (e.g. `/`).
pub fn project_default_name(path: &Path) -> String {
    path.file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| path.display().to_string())
}

/// Canonicalize when the folder exists, otherwise normalize lexically so a
/// moved or deleted folder keeps a stable identity.
pub fn normalize_root(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| lexical_absolute(path))
}

fn lexical_absolute(path: &Path) -> PathBuf {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join(path)
    };
    let mut out = PathBuf::new();
    for component in absolute.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            Component::Prefix(prefix) => out.push(prefix.as_os_str()),
            Component::RootDir => out.push(component.as_os_str()),
            Component::Normal(part) => out.push(part),
        }
    }
    out
}

fn normalize_roots(folders: &[PathBuf]) -> Vec<PathBuf> {
    let mut roots: Vec<PathBuf> = Vec::new();
    for folder in folders {
        let normalized = normalize_root(folder);
        if !roots.iter().any(|existing| existing == &normalized) {
            roots.push(normalized);
        }
    }
    roots
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_project_store() -> (tempfile::TempDir, ProjectStore) {
        let directory = tempfile::tempdir().unwrap();
        let store = ProjectStore::at(directory.path().join("projects.json"));
        (directory, store)
    }

    #[test]
    fn create_dedupes_folders_and_marks_first_primary() {
        let (directory, mut store) = temp_project_store();
        let first = directory.path().join("alpha");
        let second = directory.path().join("beta");
        std::fs::create_dir_all(&first).unwrap();
        std::fs::create_dir_all(&second).unwrap();

        let project = store
            .create(&[first.clone(), second.clone(), first.clone()])
            .unwrap();
        assert_eq!(project.roots.len(), 2);
        assert_eq!(project.primary(), Some(first.as_path()));
        assert_eq!(project.name, "alpha");
        assert!(project.is_multi_root());
        assert_eq!(project.additional_roots(&first), vec![second]);
    }

    #[test]
    fn duplicate_names_get_suffixes() {
        let (directory, mut store) = temp_project_store();
        for name in ["one", "two"] {
            let folder = directory.path().join(name);
            std::fs::create_dir_all(&folder).unwrap();
            store.create(&[folder]).unwrap();
        }
        let third = directory.path().join("one");
        let project = store.create(&[third]).unwrap();
        assert_eq!(project.name, "one (2)");
    }

    #[test]
    fn update_promotes_first_root_when_primary_is_removed() {
        let (directory, mut store) = temp_project_store();
        let alpha = directory.path().join("alpha");
        let beta = directory.path().join("beta");
        std::fs::create_dir_all(&alpha).unwrap();
        std::fs::create_dir_all(&beta).unwrap();
        let project = store.create(&[alpha.clone(), beta.clone()]).unwrap();

        // Remove the primary folder; beta must take over.
        store
            .update(&project.id, "renamed", &[beta.clone()], None)
            .unwrap();
        let updated = store.by_id(&project.id).unwrap();
        assert_eq!(updated.name, "renamed");
        assert_eq!(updated.primary(), Some(beta.as_path()));
        assert_eq!(updated.roots.len(), 1);
    }

    #[test]
    fn explicit_primary_is_honored() {
        let (directory, mut store) = temp_project_store();
        let alpha = directory.path().join("alpha");
        let beta = directory.path().join("beta");
        std::fs::create_dir_all(&alpha).unwrap();
        std::fs::create_dir_all(&beta).unwrap();
        let project = store.create(&[alpha.clone(), beta.clone()]).unwrap();

        store
            .update(&project.id, "", &[alpha.clone(), beta.clone()], Some(&beta))
            .unwrap();
        let updated = store.by_id(&project.id).unwrap();
        assert_eq!(updated.primary(), Some(beta.as_path()));
        // Empty name falls back to the first root's name.
        assert_eq!(updated.name, "alpha");
    }

    #[test]
    fn migration_reuses_existing_root_and_maps_paths() {
        let (directory, mut store) = temp_project_store();
        let alpha = directory.path().join("alpha");
        let beta = directory.path().join("beta");
        std::fs::create_dir_all(&alpha).unwrap();
        std::fs::create_dir_all(&beta).unwrap();
        let existing = store.create(&[alpha.clone(), beta.clone()]).unwrap();

        let mapping = store.migrate_legacy(&[alpha.clone()], &[beta.clone(), alpha.clone()]);
        assert_eq!(store.projects().len(), 1);
        for (_, id) in &mapping {
            assert_eq!(id, &existing.id);
        }
        assert_eq!(mapping.len(), 3);
    }

    #[test]
    fn reconcile_adds_missing_session_folders() {
        let (directory, mut store) = temp_project_store();
        let alpha = directory.path().join("alpha");
        std::fs::create_dir_all(&alpha).unwrap();
        assert!(store.reconcile(&[alpha.clone()]));
        assert!(!store.reconcile(&[alpha.clone()]));
        assert_eq!(store.projects().len(), 1);
        assert_eq!(store.projects()[0].primary(), Some(alpha.as_path()));
    }

    #[test]
    fn save_and_reload_roundtrip() {
        let (directory, mut store) = temp_project_store();
        let alpha = directory.path().join("alpha");
        std::fs::create_dir_all(&alpha).unwrap();
        let created = store.create(&[alpha.clone()]).unwrap();
        store.save().unwrap();

        let reloaded = ProjectStore::at(directory.path().join("projects.json"));
        assert!(reloaded.load_error().is_none());
        assert_eq!(reloaded.projects().len(), 1);
        assert_eq!(reloaded.projects()[0].id, created.id);
        assert_eq!(reloaded.projects()[0].primary(), Some(alpha.as_path()));
    }

    #[test]
    fn newer_schema_is_not_overwritten() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("projects.json");
        std::fs::write(
            &path,
            r#"{"schema_version": 99, "projects": [{"id": "x", "name": "x", "roots": [], "created_at": "2026-01-01T00:00:00Z", "updated_at": "2026-01-01T00:00:00Z"}]}"#,
        )
        .unwrap();
        let store = ProjectStore::at(&path);
        assert!(store.load_error().is_some());
        assert!(store.projects().is_empty());
        // The original file is preserved verbatim.
        let contents = std::fs::read_to_string(&path).unwrap();
        assert!(contents.contains("\"schema_version\": 99"));
    }

    #[test]
    fn project_for_session_prefers_id_then_legacy_path() {
        let (directory, mut store) = temp_project_store();
        let alpha = directory.path().join("alpha");
        let beta = directory.path().join("beta");
        let gamma = directory.path().join("gamma");
        for folder in [&alpha, &beta, &gamma] {
            std::fs::create_dir_all(folder).unwrap();
        }
        let both = store.create(&[alpha.clone(), beta.clone()]).unwrap();
        let other = store.create(&[gamma.clone(), alpha.clone()]).unwrap();

        // A session that knows its project id is never re-routed by path.
        assert_eq!(
            store
                .project_for_session(Some(&other.id), Some(&alpha))
                .unwrap()
                .id,
            other.id
        );
        // Legacy sessions resolve through the project roots.
        assert_eq!(
            store.project_for_session(None, Some(&beta)).unwrap().id,
            both.id
        );
        assert!(store.project_for_session(None, None).is_none());
    }

    #[test]
    fn missing_folders_keep_lexical_identity() {
        let (directory, mut store) = temp_project_store();
        let gone = directory.path().join("gone");
        let project = store.create(&[gone.clone()]).unwrap();
        assert_eq!(project.primary(), Some(lexical_absolute(&gone).as_path()));
    }
}
