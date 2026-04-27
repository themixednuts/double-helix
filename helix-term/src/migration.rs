use anyhow::{Context, Result};
use std::{
    fs,
    io::{self, Write},
    path::Path,
};

#[derive(Debug, Default, Eq, PartialEq)]
struct MigrationReport {
    copied: usize,
    skipped: usize,
}

pub fn migrate_from_helix() -> Result<()> {
    let source = helix_loader::legacy_config_dir();
    let destination = helix_loader::config_dir();
    let mut stdout = io::stdout().lock();
    migrate_config_dir(&source, &destination, &mut stdout)
}

fn migrate_config_dir(source: &Path, destination: &Path, writer: &mut dyn Write) -> Result<()> {
    writeln!(writer, "Migrating Helix config to Double Helix")?;
    writeln!(writer, "source: {}", source.display())?;
    writeln!(writer, "destination: {}", destination.display())?;

    if !source.exists() {
        writeln!(writer, "source does not exist; nothing to migrate")?;
        return Ok(());
    }

    fs::create_dir_all(destination).with_context(|| {
        format!(
            "failed to create Double Helix config directory {}",
            destination.display()
        )
    })?;

    let report = copy_missing_children(source, destination, writer)?;
    writeln!(
        writer,
        "migration complete: {} copied, {} skipped",
        report.copied, report.skipped
    )?;

    Ok(())
}

fn copy_missing_children(
    source: &Path,
    destination: &Path,
    writer: &mut dyn Write,
) -> Result<MigrationReport> {
    let mut report = MigrationReport::default();

    for entry in fs::read_dir(source)
        .with_context(|| format!("failed to read Helix config directory {}", source.display()))?
    {
        let entry = entry?;
        let target = destination.join(entry.file_name());
        if target.exists() {
            report.skipped += 1;
            writeln!(writer, "skip existing {}", target.display())?;
            continue;
        }

        copy_path(&entry.path(), &target)?;
        report.copied += 1;
        writeln!(writer, "copy {}", target.display())?;
    }

    Ok(report)
}

fn copy_path(source: &Path, destination: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(source)
        .with_context(|| format!("failed to read metadata for {}", source.display()))?;
    let file_type = metadata.file_type();

    if file_type.is_dir() {
        copy_dir_recursive(source, destination)
    } else if file_type.is_file() {
        copy_file(source, destination)
    } else if file_type.is_symlink() {
        copy_symlink_target(source, destination)
    } else {
        Ok(())
    }
}

fn copy_symlink_target(source: &Path, destination: &Path) -> Result<()> {
    let target = fs::canonicalize(source)
        .with_context(|| format!("failed to resolve symlink {}", source.display()))?;
    if target.is_dir() {
        copy_dir_recursive(&target, destination)
    } else {
        copy_file(&target, destination)
    }
}

fn copy_dir_recursive(source: &Path, destination: &Path) -> Result<()> {
    fs::create_dir_all(destination)
        .with_context(|| format!("failed to create directory {}", destination.display()))?;

    for entry in fs::read_dir(source)
        .with_context(|| format!("failed to read directory {}", source.display()))?
    {
        let entry = entry?;
        copy_path(&entry.path(), &destination.join(entry.file_name()))?;
    }

    Ok(())
}

fn copy_file(source: &Path, destination: &Path) -> Result<()> {
    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create directory {}", parent.display()))?;
    }

    fs::copy(source, destination).with_context(|| {
        format!(
            "failed to copy {} to {}",
            source.display(),
            destination.display()
        )
    })?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::migrate_config_dir;
    use std::fs;

    #[test]
    fn migration_copies_missing_config_children() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("helix");
        let destination = temp.path().join("double-helix");
        fs::create_dir_all(source.join("themes")).unwrap();
        fs::write(source.join("config.toml"), "theme = \"base16_default\"").unwrap();
        fs::write(
            source.join("themes").join("mine.toml"),
            "inherits = \"base16_default\"",
        )
        .unwrap();

        let mut output = Vec::new();
        migrate_config_dir(&source, &destination, &mut output).unwrap();

        assert_eq!(
            fs::read_to_string(destination.join("config.toml")).unwrap(),
            "theme = \"base16_default\""
        );
        assert_eq!(
            fs::read_to_string(destination.join("themes").join("mine.toml")).unwrap(),
            "inherits = \"base16_default\""
        );
        assert!(String::from_utf8(output)
            .unwrap()
            .contains("migration complete: 2 copied, 0 skipped"));
    }

    #[test]
    fn migration_does_not_overwrite_existing_children() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("helix");
        let destination = temp.path().join("double-helix");
        fs::create_dir_all(&source).unwrap();
        fs::create_dir_all(&destination).unwrap();
        fs::write(source.join("config.toml"), "theme = \"source\"").unwrap();
        fs::write(destination.join("config.toml"), "theme = \"destination\"").unwrap();

        let mut output = Vec::new();
        migrate_config_dir(&source, &destination, &mut output).unwrap();

        assert_eq!(
            fs::read_to_string(destination.join("config.toml")).unwrap(),
            "theme = \"destination\""
        );
        assert!(String::from_utf8(output)
            .unwrap()
            .contains("migration complete: 0 copied, 1 skipped"));
    }
}
