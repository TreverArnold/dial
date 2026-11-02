//! Opt-in, project-local component context. Source stories remain authoritative.
use crate::errors::{DialError, Result};
use rusqlite::{Connection, OptionalExtension};
use std::path::{Path, PathBuf};

fn setting(conn: &Connection, key: &str) -> Result<Option<String>> {
    Ok(conn
        .query_row("SELECT value FROM config WHERE key = ?1", [key], |r| {
            r.get(0)
        })
        .optional()?)
}

fn local_path(root: &Path, value: &str) -> Result<PathBuf> {
    let root = root.canonicalize()?;
    let path = root.join(value).canonicalize()?;
    if !path.starts_with(&root) {
        return Err(DialError::UserError(
            "Storybook paths must stay inside the project.".into(),
        ));
    }
    Ok(path)
}

pub(super) fn context(conn: &Connection, root: &Path, task: &str) -> Result<String> {
    let Some(source) = setting(conn, "storybook_dir")?.filter(|s| !s.trim().is_empty()) else {
        return Ok(String::new());
    };
    let source_path = local_path(root, &source)?;
    if !source_path.is_dir() {
        return Err(DialError::UserError(
            "storybook_dir must name a project directory.".into(),
        ));
    }
    let mut text = format!("## Project Storybook\nSource/configuration root: {source}\nFor UI work, search existing implementation, shared components, and stories here before creating a component. Reuse or extend existing components and design tokens. Save new reusable UI in project source with colocated stories following the existing Storybook configuration. Cover relevant loading, empty, error and interactive states. For non-UI tasks, do not change UI or stories. Run configured Storybook and functionality checks; a catalog entry is not proof of acceptance. Do not install or replace the project's framework without task authorization.\n");
    if let Some(index) = setting(conn, "storybook_index")?.filter(|s| !s.trim().is_empty()) {
        let path = local_path(root, &index)?;
        if path.metadata()?.len() > 2_000_000 {
            return Err(DialError::UserError(
                "Storybook index exceeds 2 MB; use source search instead.".into(),
            ));
        }
        let value: serde_json::Value = serde_json::from_slice(&std::fs::read(path)?)
            .map_err(|e| DialError::UserError(format!("Invalid Storybook index: {e}")))?;
        let entries = value
            .get("entries")
            .and_then(|v| v.as_object())
            .ok_or_else(|| {
                DialError::UserError("Storybook index must contain an entries object.".into())
            })?;
        let words: Vec<_> = task
            .split_whitespace()
            .map(str::to_lowercase)
            .filter(|w| w.len() > 2)
            .collect();
        let mut rows = Vec::new();
        for (id, entry) in entries {
            if entry.get("type").and_then(|v| v.as_str()) != Some("story") {
                continue;
            }
            let field = |name| {
                entry
                    .get(name)
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .replace(['\n', '\r'], " ")
                    .chars()
                    .take(180)
                    .collect::<String>()
            };
            let row = format!(
                "- {} | {} / {} | {}\n",
                id.chars().take(180).collect::<String>(),
                field("title"),
                field("name"),
                field("importPath")
            );
            let lower = row.to_lowercase();
            let score = words.iter().filter(|w| lower.contains(w.as_str())).count();
            rows.push((score, row));
        }
        rows.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
        text.push_str(&format!("Catalog: {index} (generated snapshot; verify source before reuse). Showing up to 20 task-ranked entries; search source for the rest.\n"));
        let mut used = 0;
        for (_, row) in rows.into_iter().take(20) {
            if used + row.len() > 6000 {
                break;
            }
            used += row.len();
            text.push_str(&row);
        }
    }
    Ok(text)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn db() -> Connection {
        let c = Connection::open_in_memory().unwrap();
        c.execute_batch("CREATE TABLE config (key TEXT PRIMARY KEY, value TEXT);")
            .unwrap();
        c
    }
    #[test]
    fn disabled_catalog_needs_no_files() {
        assert_eq!(
            context(&db(), Path::new("/does-not-exist"), "backend").unwrap(),
            ""
        );
    }
    #[test]
    fn ranks_and_bounds_catalog_and_rejects_escape() {
        let dir = tempfile::tempdir().unwrap();
        let c = db();
        c.execute("INSERT INTO config VALUES ('storybook_dir', '.')", [])
            .unwrap();
        c.execute(
            "INSERT INTO config VALUES ('storybook_index', 'index.json')",
            [],
        )
        .unwrap();
        let mut entries = serde_json::Map::new();
        for i in 0..100 {
            entries.insert(format!("button-{i}"), serde_json::json!({"type":"story","title":"Button","name":"Default","importPath":"src/Button.stories.ts"}));
        }
        entries.insert("search".into(), serde_json::json!({"type":"story","title":"Search","name":"Empty","importPath":"src/Search.stories.ts"}));
        std::fs::write(
            dir.path().join("index.json"),
            serde_json::json!({"entries":entries}).to_string(),
        )
        .unwrap();
        let text = context(&c, dir.path(), "Search results").unwrap();
        assert_eq!(text.lines().filter(|l| l.starts_with("- ")).count(), 20);
        assert!(text.find("- search").unwrap() < text.find("- button").unwrap());
        c.execute(
            "UPDATE config SET value = '..' WHERE key = 'storybook_dir'",
            [],
        )
        .unwrap();
        assert!(context(&c, dir.path(), "Search").is_err());
    }
}
