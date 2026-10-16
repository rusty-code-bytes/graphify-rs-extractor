use crate::Options;
use anyhow::{Context, Result, bail};
use globset::{GlobBuilder, GlobSet, GlobSetBuilder};
use std::{
    collections::HashSet,
    fs,
    path::{Component, Path, PathBuf},
};

pub struct Filters {
    globs: GlobSet,
    files: HashSet<PathBuf>,
}

impl Filters {
    pub fn new(root: &Path, options: &Options) -> Result<Self> {
        let mut patterns = options.exclude.clone();
        let mut sources = options.exclude_from.clone();
        let automatic = root.join(".graphifyignore");
        match fs::symlink_metadata(&automatic) {
            Ok(_) => sources.push(automatic),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e).context("inspect .graphifyignore"),
        }
        for source in sources {
            let text = fs::read_to_string(&source)
                .with_context(|| format!("read exclusion file {}", source.display()))?;
            patterns.extend(
                text.lines()
                    .map(str::trim)
                    .filter(|l| !l.is_empty() && !l.starts_with('#'))
                    .map(str::to_owned),
            );
        }
        let mut globs = GlobSetBuilder::new();
        for pattern in patterns {
            if pattern.starts_with('!')
                || pattern.starts_with('/')
                || pattern.split('/').any(|s| s == "..")
            {
                bail!("exclusion must be a root-relative glob without negation: {pattern}");
            }
            let pattern = if pattern.ends_with('/') {
                format!("{pattern}**")
            } else {
                pattern
            };
            globs.add(
                GlobBuilder::new(&pattern)
                    .literal_separator(true)
                    .backslash_escape(true)
                    .build()
                    .with_context(|| format!("invalid exclusion glob {pattern:?}"))?,
            );
        }
        let mut files = HashSet::new();
        for path in &options.exclude_file {
            let mut normalized = PathBuf::new();
            for c in path.components() {
                match c {
                    Component::Normal(p) => normalized.push(p),
                    Component::CurDir => {}
                    _ => bail!(
                        "exclude-file must be root-relative without '..': {}",
                        path.display()
                    ),
                }
            }
            if normalized.as_os_str().is_empty() {
                bail!("empty exclude-file");
            }
            files.insert(normalized);
        }
        Ok(Self {
            globs: globs.build()?,
            files,
        })
    }
    pub fn excludes(&self, path: &Path, root: &Path) -> bool {
        let Ok(rel) = path.strip_prefix(root) else {
            return true;
        };
        self.files.contains(rel) || self.globs.is_match(rel)
            // A trailing /** pattern excludes the directory itself too.
            || self.globs.is_match(format!("{}/", rel.to_string_lossy()))
    }
}
