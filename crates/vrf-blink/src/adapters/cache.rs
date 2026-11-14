use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, SystemTime};

const USED: &str = ".used";
const KEEP: usize = 3;
const RECENT: Duration = Duration::from_secs(3600);

pub(crate) fn keep() -> usize {
    std::env::var("VRF_CACHE_KEEP")
        .ok()
        .and_then(|value| value.trim().parse().ok())
        .unwrap_or(KEEP)
}

pub(crate) fn touch(entry: &Path) {
    let _ = fs::write(entry.join(USED), b"");
}

pub(crate) fn last_used(entry: &Path) -> SystemTime {
    fs::metadata(entry.join(USED))
        .or_else(|_| fs::symlink_metadata(entry))
        .and_then(|metadata| metadata.modified())
        .unwrap_or(SystemTime::UNIX_EPOCH)
}

fn is_link(metadata: &fs::Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
        if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return true;
        }
    }
    metadata.file_type().is_symlink()
}

fn remove_link(path: &Path, metadata: &fs::Metadata) -> io::Result<()> {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x10;
        if metadata.file_attributes() & FILE_ATTRIBUTE_DIRECTORY != 0 {
            return fs::remove_dir(path);
        }
    }
    #[cfg(not(windows))]
    let _ = metadata;
    fs::remove_file(path)
}

fn unlink_all(dir: &Path) -> io::Result<()> {
    for entry in fs::read_dir(dir)? {
        let path = entry?.path();
        let metadata = fs::symlink_metadata(&path)?;
        if is_link(&metadata) {
            remove_link(&path, &metadata)?;
        } else if metadata.is_dir() {
            unlink_all(&path)?;
        }
    }
    Ok(())
}

/// Deletes `path`. A link, at `path` or anywhere under it, is removed itself and the directory
/// it points at is never entered.
pub(crate) fn remove(path: &Path) -> io::Result<()> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    if is_link(&metadata) {
        return remove_link(path, &metadata);
    }
    if !metadata.is_dir() {
        return fs::remove_file(path);
    }
    unlink_all(path)?;
    fs::remove_dir_all(path)
}

#[cfg(not(windows))]
pub(crate) fn link_dir(target: &Path, link: &Path) -> io::Result<()> {
    std::os::unix::fs::symlink(target, link)
}

#[cfg(windows)]
pub(crate) fn link_dir(target: &Path, link: &Path) -> io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows::Win32::Foundation::{CloseHandle, GENERIC_WRITE};
    use windows::Win32::Storage::FileSystem::{
        CreateFileW, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_MODE,
        OPEN_EXISTING,
    };
    use windows::Win32::System::IO::DeviceIoControl;
    use windows::Win32::System::Ioctl::FSCTL_SET_REPARSE_POINT;
    use windows::core::PCWSTR;

    const IO_REPARSE_TAG_MOUNT_POINT: u32 = 0xA000_0003;
    let target = crate::canonical(target)?;
    let print: Vec<u16> = target.as_os_str().encode_wide().collect();
    if print.starts_with(&[92u16, 92]) {
        return Err(io::Error::other(format!(
            "a junction cannot point at {}",
            target.display()
        )));
    }
    let substitute: Vec<u16> = r"\??\"
        .encode_utf16()
        .chain(print.iter().copied())
        .collect();
    let bytes = |name: &[u16]| (name.len() * 2) as u16;
    let mut data = Vec::new();
    data.extend(0u16.to_le_bytes());
    data.extend(bytes(&substitute).to_le_bytes());
    data.extend((bytes(&substitute) + 2).to_le_bytes());
    data.extend(bytes(&print).to_le_bytes());
    for unit in substitute
        .iter()
        .chain(&[0u16])
        .chain(&print)
        .chain(&[0u16])
    {
        data.extend(unit.to_le_bytes());
    }
    let mut buffer = Vec::with_capacity(8 + data.len());
    buffer.extend(IO_REPARSE_TAG_MOUNT_POINT.to_le_bytes());
    buffer.extend((data.len() as u16).to_le_bytes());
    buffer.extend(0u16.to_le_bytes());
    buffer.extend(data);

    fs::create_dir(link)?;
    let wide: Vec<u16> = link.as_os_str().encode_wide().chain([0]).collect();
    let set = unsafe {
        CreateFileW(
            PCWSTR(wide.as_ptr()),
            GENERIC_WRITE.0,
            FILE_SHARE_MODE(0),
            None,
            OPEN_EXISTING,
            FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_BACKUP_SEMANTICS,
            None,
        )
        .and_then(|handle| {
            let result = DeviceIoControl(
                handle,
                FSCTL_SET_REPARSE_POINT,
                Some(buffer.as_ptr().cast()),
                buffer.len() as u32,
                None,
                0,
                None,
                None,
            );
            let _ = CloseHandle(handle);
            result
        })
    };
    set.map_err(|error| {
        let _ = fs::remove_dir(link);
        io::Error::other(error)
    })
}

/// Whether a link can be made under `root`. exFAT and FAT volumes have none. The answer is kept
/// for the life of the process.
pub(crate) fn links_work(root: &Path) -> bool {
    static KNOWN: Mutex<Vec<(PathBuf, bool)>> = Mutex::new(Vec::new());
    let mut known = KNOWN.lock().unwrap();
    if let Some((_, works)) = known.iter().find(|(dir, _)| dir == root) {
        return *works;
    }
    let works = probe(root).is_ok();
    known.push((root.to_owned(), works));
    works
}

fn probe(root: &Path) -> io::Result<()> {
    fs::create_dir_all(root)?;
    let link = root.join(format!("link-probe.partial-{}", std::process::id()));
    remove(&link)?;
    let made = link_dir(root, &link);
    let removed = remove(&link);
    made.and(removed)
}

/// Removes the bundles under `root` beyond the `keep` used most recently, and partial bundles
/// older than an hour, which a killed or crashed run left behind. A bundle in `held`, or one
/// used within the last hour, stays. Returns what was removed.
pub(crate) fn prune(
    root: &Path,
    keep: usize,
    held: &[PathBuf],
    is_entry: impl Fn(&str) -> bool,
) -> io::Result<Vec<PathBuf>> {
    let now = SystemTime::now();
    let old = |time: SystemTime| now.duration_since(time).is_ok_and(|age| age > RECENT);
    let mut entries = Vec::new();
    let mut removed = Vec::new();
    for item in fs::read_dir(root)? {
        let item = item?;
        let name = item.file_name().to_string_lossy().into_owned();
        let path = item.path();
        if name.contains(".partial-") {
            let modified = fs::symlink_metadata(&path).and_then(|metadata| metadata.modified());
            if modified.is_ok_and(old) && !held.contains(&path) && remove(&path).is_ok() {
                removed.push(path);
            }
        } else if is_entry(&name) && fs::symlink_metadata(&path).is_ok_and(|m| m.is_dir()) {
            entries.push((last_used(&path), path));
        }
    }
    entries.sort_by(|a, b| b.0.cmp(&a.0));
    for (used, path) in entries.into_iter().skip(keep) {
        if held.contains(&path) || !old(used) {
            continue;
        }
        if remove(&path).is_ok() {
            removed.push(path);
        }
    }
    Ok(removed)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Scratch(PathBuf);

    impl Scratch {
        fn new(name: &str) -> Scratch {
            let dir = std::env::temp_dir().join(format!("vrf-cache-{name}-{}", std::process::id()));
            let _ = remove(&dir);
            fs::create_dir_all(&dir).unwrap();
            Scratch(dir)
        }

        fn file(&self, relative: &str) -> PathBuf {
            let path = self.0.join(relative);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(&path, relative).unwrap();
            path
        }

        fn entry(&self, name: &str, age: Duration) -> PathBuf {
            let dir = self.0.join("cache").join(name);
            fs::create_dir_all(&dir).unwrap();
            fs::write(dir.join("index.html"), name).unwrap();
            touch(&dir);
            set_age(&dir.join(USED), age).unwrap();
            dir
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = remove(&self.0);
        }
    }

    const HOURS: Duration = Duration::from_secs(7200);

    fn set_age(path: &Path, age: Duration) -> io::Result<()> {
        let mut options = fs::OpenOptions::new();
        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt;
            const FILE_WRITE_ATTRIBUTES: u32 = 0x100;
            const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
            options
                .access_mode(FILE_WRITE_ATTRIBUTES)
                .custom_flags(FILE_FLAG_BACKUP_SEMANTICS);
        }
        #[cfg(not(windows))]
        options.read(true);
        options.open(path)?.set_modified(SystemTime::now() - age)
    }

    fn every(_: &str) -> bool {
        true
    }

    #[test]
    fn a_linked_directory_shows_its_target() -> io::Result<()> {
        let scratch = Scratch::new("link");
        scratch.file("assets/v/clip.mp4");
        fs::create_dir_all(scratch.0.join("bundle"))?;
        link_dir(&scratch.0.join("assets"), &scratch.0.join("bundle/public"))?;
        assert_eq!(
            fs::read_to_string(scratch.0.join("bundle/public/v/clip.mp4"))?,
            "assets/v/clip.mp4"
        );
        assert!(is_link(&fs::symlink_metadata(
            scratch.0.join("bundle/public")
        )?));
        Ok(())
    }

    #[test]
    fn removing_a_bundle_keeps_everything_its_links_point_at() -> io::Result<()> {
        let scratch = Scratch::new("remove");
        let clip = scratch.file("assets/v/clip.mp4");
        let nested = scratch.file("assets/deep/a/b.png");
        let bundle = scratch.0.join("bundle");
        fs::create_dir_all(bundle.join("static/deep"))?;
        fs::write(bundle.join("index.html"), "x")?;
        link_dir(&scratch.0.join("assets"), &bundle.join("public"))?;
        link_dir(
            &scratch.0.join("assets/deep"),
            &bundle.join("static/deep/more"),
        )?;
        remove(&bundle)?;
        assert!(!bundle.exists());
        assert_eq!(fs::read_to_string(&clip)?, "assets/v/clip.mp4");
        assert_eq!(fs::read_to_string(&nested)?, "assets/deep/a/b.png");
        Ok(())
    }

    #[test]
    fn the_standard_library_does_not_follow_a_link_when_it_removes_a_tree() -> io::Result<()> {
        let scratch = Scratch::new("std");
        let clip = scratch.file("assets/v/clip.mp4");
        let bundle = scratch.0.join("bundle");
        fs::create_dir_all(bundle.join("nested"))?;
        link_dir(&scratch.0.join("assets"), &bundle.join("nested/public"))?;
        fs::remove_dir_all(&bundle)?;
        assert!(!bundle.exists());
        assert_eq!(fs::read_to_string(&clip)?, "assets/v/clip.mp4");
        Ok(())
    }

    #[test]
    fn links_work_in_the_temp_dir_and_the_probe_leaves_nothing() -> io::Result<()> {
        let scratch = Scratch::new("probe");
        assert!(links_work(&scratch.0));
        assert_eq!(fs::read_dir(&scratch.0)?.count(), 0);
        Ok(())
    }

    #[test]
    fn threads_that_ask_about_one_root_at_once_all_hear_that_links_work() -> io::Result<()> {
        let scratch = Scratch::new("race");
        let root = scratch.0.join("cache");
        let start = std::sync::Barrier::new(8);
        let answers: Vec<bool> = std::thread::scope(|scope| {
            let asks: Vec<_> = (0..8)
                .map(|_| {
                    scope.spawn(|| {
                        start.wait();
                        links_work(&root)
                    })
                })
                .collect();
            asks.into_iter().map(|ask| ask.join().unwrap()).collect()
        });
        assert_eq!(answers, vec![true; 8]);
        assert_eq!(fs::read_dir(&root)?.count(), 0);
        Ok(())
    }

    #[test]
    fn removing_a_link_itself_leaves_its_target() -> io::Result<()> {
        let scratch = Scratch::new("unlink");
        let clip = scratch.file("assets/clip.mp4");
        let link = scratch.0.join("public");
        link_dir(&scratch.0.join("assets"), &link)?;
        remove(&link)?;
        assert!(fs::symlink_metadata(&link).is_err());
        assert!(clip.is_file());
        Ok(())
    }

    #[test]
    fn prune_keeps_the_newest_and_removes_the_rest_without_touching_link_targets() -> io::Result<()>
    {
        let scratch = Scratch::new("prune");
        let clip = scratch.file("assets/v/clip.mp4");
        let names = ["a", "b", "c", "d", "e"];
        let dirs: Vec<PathBuf> = names
            .iter()
            .enumerate()
            .map(|(age, name)| scratch.entry(name, HOURS * (age as u32 + 1)))
            .collect();
        for dir in &dirs {
            link_dir(&scratch.0.join("assets"), &dir.join("public"))?;
        }
        let root = scratch.0.join("cache");
        let removed = prune(&root, 3, &[], every)?;
        assert_eq!(removed.len(), 2);
        for (dir, kept) in dirs.iter().zip([true, true, true, false, false]) {
            assert_eq!(dir.exists(), kept, "{}", dir.display());
        }
        assert_eq!(fs::read_to_string(&clip)?, "assets/v/clip.mp4");
        Ok(())
    }

    #[test]
    fn prune_keeps_held_and_recently_used_bundles() -> io::Result<()> {
        let scratch = Scratch::new("held");
        let newest = scratch.entry("a", HOURS);
        let recent = scratch.entry("b", Duration::from_secs(60));
        let held = scratch.entry("c", HOURS * 3);
        let old = scratch.entry("d", HOURS * 4);
        let removed = prune(
            &scratch.0.join("cache"),
            2,
            std::slice::from_ref(&held),
            every,
        )?;
        assert_eq!(removed, vec![old.clone()]);
        assert!(newest.exists() && recent.exists() && held.exists());
        assert!(!old.exists());
        Ok(())
    }

    #[test]
    fn prune_removes_old_partial_bundles_and_leaves_fresh_ones() -> io::Result<()> {
        let scratch = Scratch::new("partial");
        let clip = scratch.file("assets/clip.mp4");
        let root = scratch.0.join("cache");
        let stale = root.join("0123456789abcdef.partial-99-0");
        let fresh = root.join("0123456789abcdef.partial-98-0");
        for dir in [&stale, &fresh] {
            fs::create_dir_all(dir.join("static"))?;
            link_dir(&scratch.0.join("assets"), &dir.join("public"))?;
        }
        set_age(&stale, HOURS)?;
        prune(&root, 3, &[], every)?;
        assert!(!stale.exists());
        assert!(fresh.exists());
        assert!(clip.is_file());
        Ok(())
    }

    #[test]
    fn prune_leaves_names_that_are_not_bundles() -> io::Result<()> {
        let scratch = Scratch::new("names");
        let other = scratch.entry("empty-public", HOURS * 9);
        prune(&scratch.0.join("cache"), 0, &[], |name| {
            name != "empty-public"
        })?;
        assert!(other.exists());
        Ok(())
    }
}
