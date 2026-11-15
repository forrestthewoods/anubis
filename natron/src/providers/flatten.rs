//! The `flatten` option of the `git` and `url` providers: also
//! place every file whose name matches a pattern into one directory of the
//! install.
//!
//! Some source trees keep each header next to its code, one directory per
//! package, but include them by bare name (`#include <gp_Pnt.hxx>`), and
//! expect the build to gather them into one include directory first
//! (OpenCASCADE's CMake build does). Without that directory a compile needs
//! one `-I` per package: hundreds of failed lookups per `#include`, which is
//! very slow on Windows.
//!
//! ```toml
//! [toolchain.options]
//! flatten = { into = "inc", files = ["*.hxx", "*.lxx", "*.gxx", "*.h"] }
//! ```
//!
//! `files` are patterns for file names (not paths). The originals stay where
//! they are; `into/<name>` is a second name for the same content (the CAS
//! stores it once). Two matching files with the same name, compared without
//! case, are an error.

use anyhow::{Context, Result, anyhow, bail};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Flatten {
    into: String,
    patterns: Vec<String>,
}

impl Flatten {
    /// Parse `options.flatten`. `provider` names the caller for errors.
    pub(crate) fn parse(options: &toml::Table, provider: &str) -> Result<Option<Flatten>> {
        let Some(v) = options.get("flatten") else {
            return Ok(None);
        };
        let t = v.as_table().ok_or_else(|| {
            anyhow!("`{provider}` option 'flatten' must be a table: {{ into = \"dir\", files = [\"*.h\"] }}")
        })?;
        if let Some(k) = t.keys().find(|k| *k != "into" && *k != "files") {
            bail!("`{provider}` option 'flatten' has an unknown key '{k}' (expected: into, files)");
        }
        let into = super::options::required_str(t, "into", provider)
            .context("in option 'flatten'")?
            .trim_matches('/')
            .to_string();
        // `/`-separated components only (see the `git` provider's strip_prefix).
        if into.is_empty()
            || into.contains(['\\', ':'])
            || into.split('/').any(|c| c.is_empty() || c == "." || c == "..")
        {
            bail!("`{provider}` option 'flatten': into must be a relative path, got {into:?}");
        }
        let patterns = super::options::optional_string_list(t, "files", provider)?;
        if patterns.is_empty() {
            bail!("`{provider}` option 'flatten': files must list at least one pattern, e.g. [\"*.h\"]");
        }
        for p in &patterns {
            if p.contains(['/', '\\']) {
                bail!("`{provider}` option 'flatten': files patterns match file names, not paths; got {p:?}");
            }
            glob::Pattern::new(p)
                .with_context(|| format!("`{provider}` option 'flatten': bad pattern {p:?}"))?;
        }
        Ok(Some(Flatten { into, patterns }))
    }

    /// Text for the install's fingerprint: a different `flatten` is a
    /// different install tree.
    pub(crate) fn key(this: &Option<Flatten>) -> String {
        match this {
            None => String::new(),
            Some(f) => format!("|flatten:{}:{:?}", f.into, f.patterns),
        }
    }

    /// Add `into/` to the extracted tree at `root`. Returns the file count.
    pub(crate) fn apply(&self, root: &Path) -> Result<usize> {
        let into = root.join(&self.into);
        if into.symlink_metadata().is_ok() {
            bail!("flatten: '{}' already exists in the installed tree; pick another `into`", self.into);
        }
        // A parent of `into` that the archive ships as a symlink (or a file)
        // would put the folder somewhere else, maybe outside the tree.
        let mut parent = root.to_path_buf();
        let parts: Vec<&str> = self.into.split('/').collect();
        for part in &parts[..parts.len() - 1] {
            parent.push(part);
            if parent.symlink_metadata().is_ok_and(|m| !m.file_type().is_dir()) {
                bail!("flatten: '{}' is not a directory in the installed tree; pick another `into`", part);
            }
        }
        let patterns: Vec<glob::Pattern> =
            self.patterns.iter().map(|p| glob::Pattern::new(p)).collect::<std::result::Result<_, _>>()?;

        // Lowercased name -> (name, source). Sorted, so a rerun links the
        // same files in the same order.
        let mut found: BTreeMap<String, (String, PathBuf)> = BTreeMap::new();
        let mut clashes: Vec<String> = Vec::new();
        for entry in jwalk::WalkDir::new(root).skip_hidden(false).follow_links(false).sort(true) {
            let entry = entry.with_context(|| format!("walking {}", root.display()))?;
            if !entry.file_type().is_file() {
                continue;
            }
            let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            if !patterns.iter().any(|p| p.matches(&name)) {
                continue;
            }
            let src = entry.path();
            if let Some((_, first)) = found.get(&name.to_lowercase()) {
                let rel = |p: &Path| crate::fs_util::slash_str(p.strip_prefix(root).unwrap_or(p));
                clashes.push(format!("{} and {}", rel(first), rel(&src)));
                continue;
            }
            found.insert(name.to_lowercase(), (name, src));
        }
        if !clashes.is_empty() {
            clashes.truncate(10);
            bail!(
                "flatten: files with the same name can't share '{}':\n  {}",
                self.into,
                clashes.join("\n  ")
            );
        }
        if found.is_empty() {
            bail!("flatten: no file matches {:?}", self.patterns);
        }

        std::fs::create_dir_all(&into).with_context(|| format!("creating {}", into.display()))?;
        for (name, src) in found.values() {
            let dst = into.join(name);
            // A plain copy: the install's CAS pass stores equal files once.
            std::fs::copy(src, &dst)
                .with_context(|| format!("copying {} to {}", src.display(), dst.display()))?;
        }
        Ok(found.len())
    }
}

#[cfg(test)]
#[path = "tests/flatten.rs"]
mod tests;
