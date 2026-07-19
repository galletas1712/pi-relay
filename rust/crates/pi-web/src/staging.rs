use std::ffi::OsStr;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::num::NonZeroU64;
use std::path::{Component, Path};

use cap_fs_ext::{ambient_authority, DirExt, FollowSymlinks, OpenOptionsFollowExt};
use cap_std::fs::{Dir, OpenOptions};

#[derive(Clone, Copy)]
pub(crate) struct CopyLimits {
    pub(crate) max_entries: usize,
    pub(crate) max_bytes: u64,
    pub(crate) max_depth: usize,
}

pub(crate) struct CopyBudget {
    limits: CopyLimits,
    entries: usize,
    bytes: u64,
}

impl CopyBudget {
    pub(crate) fn new(limits: CopyLimits) -> Self {
        Self {
            limits,
            entries: 0,
            bytes: 0,
        }
    }

    fn add_entry(&mut self, bytes: u64) -> io::Result<()> {
        self.entries = self.entries.checked_add(1).ok_or_else(limit_error)?;
        self.bytes = self.bytes.checked_add(bytes).ok_or_else(limit_error)?;
        if self.entries > self.limits.max_entries || self.bytes > self.limits.max_bytes {
            return Err(limit_error());
        }
        Ok(())
    }
}

/// Open every component through a stable no-follow directory handle.
///
/// This deliberately rejects symlinked ancestors as well as a symlink at the
/// leaf. On platforms where cap-std cannot provide the no-follow operation,
/// opening fails instead of falling back to ambient pathname traversal.
pub(crate) fn open_absolute_dir_nofollow(path: &Path) -> io::Result<Dir> {
    if !path.is_absolute() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "path must be absolute",
        ));
    }
    let anchor = path
        .ancestors()
        .last()
        .filter(|ancestor| !ancestor.as_os_str().is_empty())
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "path has no root"))?;
    let relative = path
        .strip_prefix(anchor)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path has no stable root"))?;
    let mut directory = Dir::open_ambient_dir(anchor, ambient_authority())?;
    for component in relative.components() {
        let Component::Normal(name) = component else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "path contains an unsupported component",
            ));
        };
        directory = directory.open_dir_nofollow(name)?;
    }
    Ok(directory)
}

pub(crate) fn copy_tree(
    source: &Dir,
    destination: &Dir,
    budget: &mut CopyBudget,
    include: &mut dyn FnMut(&Path, bool) -> bool,
) -> io::Result<()> {
    copy_tree_at(source, destination, Path::new(""), 0, budget, include)
}

pub(crate) fn copy_named(
    source: &Dir,
    destination: &Dir,
    name: &OsStr,
    required: bool,
    budget: &mut CopyBudget,
    include: &mut dyn FnMut(&Path, bool) -> bool,
) -> io::Result<bool> {
    let metadata = match source.symlink_metadata(name) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound && !required => return Ok(false),
        Err(error) => return Err(error),
    };
    let relative = Path::new(name);
    if !include(relative, metadata.is_dir()) {
        return Ok(false);
    }
    copy_entry(source, destination, name, relative, 0, budget, include)?;
    Ok(true)
}

fn copy_tree_at(
    source: &Dir,
    destination: &Dir,
    relative: &Path,
    depth: usize,
    budget: &mut CopyBudget,
    include: &mut dyn FnMut(&Path, bool) -> bool,
) -> io::Result<()> {
    if depth > budget.limits.max_depth {
        return Err(limit_error());
    }
    let mut entries = source.entries()?.collect::<io::Result<Vec<_>>>()?;
    entries.sort_by_key(|entry| entry.file_name());
    for entry in entries {
        let name = entry.file_name();
        let metadata = source.symlink_metadata(&name)?;
        let child_relative = relative.join(&name);
        if !include(&child_relative, metadata.is_dir()) {
            continue;
        }
        copy_entry(
            source,
            destination,
            &name,
            &child_relative,
            depth,
            budget,
            include,
        )?;
    }
    Ok(())
}

fn copy_entry(
    source: &Dir,
    destination: &Dir,
    name: &OsStr,
    relative: &Path,
    depth: usize,
    budget: &mut CopyBudget,
    include: &mut dyn FnMut(&Path, bool) -> bool,
) -> io::Result<()> {
    let metadata = source.symlink_metadata(name)?;
    if metadata.file_type().is_symlink() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "symbolic links are not allowed",
        ));
    }
    if metadata.is_dir() {
        budget.add_entry(0)?;
        let source_child = source.open_dir_nofollow(name)?;
        if !source_child.dir_metadata()?.is_dir() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "directory changed type while staging",
            ));
        }
        destination.create_dir(name)?;
        let destination_child = destination.open_dir_nofollow(name)?;
        return copy_tree_at(
            &source_child,
            &destination_child,
            relative,
            depth + 1,
            budget,
            include,
        );
    }
    if !metadata.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "only regular files are allowed",
        ));
    }

    let mut source_options = OpenOptions::new();
    source_options.read(true).follow(FollowSymlinks::No);
    let source_file = source.open_with(name, &source_options)?;
    let opened_metadata = source_file.metadata()?;
    if !opened_metadata.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "file changed type while staging",
        ));
    }
    let length = opened_metadata.len();
    budget.add_entry(length)?;

    let mut destination_options = OpenOptions::new();
    destination_options
        .write(true)
        .create_new(true)
        .follow(FollowSymlinks::No);
    let destination_file = destination.open_with(name, &destination_options)?;
    copy_open_file(source_file.into_std(), destination_file.into_std(), length)
}

fn copy_open_file(
    mut source: std::fs::File,
    mut destination: std::fs::File,
    length: u64,
) -> io::Result<()> {
    if let Some(nonzero_length) = NonZeroU64::new(length) {
        destination.set_len(length)?;
        if reflink_copy::ReflinkBlockBuilder::new(&source, &destination, nonzero_length)
            .reflink_block()
            .is_ok()
        {
            return Ok(());
        }
        destination.set_len(0)?;
        destination.seek(SeekFrom::Start(0))?;
    }

    source.seek(SeekFrom::Start(0))?;
    let copied = io::copy(&mut source.take(length.saturating_add(1)), &mut destination)?;
    if copied != length {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "file changed length while staging",
        ));
    }
    destination.flush()
}

fn limit_error() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        "staged tree exceeds its safety limit",
    )
}
