#![cfg(feature = "runtime")]

#[cfg(unix)]
use std::fs;
#[cfg(unix)]
use std::time::{SystemTime, UNIX_EPOCH};

#[cfg(unix)]
use vm::{
    ConfinedDirectory, ConfinedFileType, ConfinedFsError, ConfinedFsLimits, EnumerationBudget,
    MAX_COMPONENT_BYTES, MAX_PATH_BYTES,
};
use vm::{ConfinedFsErrorKind, ConfinedFsRoot};

#[cfg(unix)]
mod unix_tests {
    use super::*;
    use std::collections::HashSet;
    use std::ffi::OsString;
    use std::os::unix::ffi::{OsStrExt, OsStringExt};
    use std::os::unix::fs::symlink;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use std::sync::{Arc, Barrier};
    use std::thread;

    static TEMP_ID: AtomicU64 = AtomicU64::new(0);

    fn unique_temp_dir(label: &str) -> PathBuf {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock should be after the epoch")
            .as_nanos();
        let id = TEMP_ID.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "rustscript-confined-fs-{label}-{}-{stamp}-{id}",
            std::process::id()
        ));
        fs::create_dir(&path).expect("temporary test directory should be created");
        path
    }

    fn remove_any(path: &Path) {
        let Ok(metadata) = fs::symlink_metadata(path) else {
            return;
        };
        if metadata.file_type().is_dir() {
            fs::remove_dir_all(path).expect("temporary directory should be removed");
        } else {
            fs::remove_file(path).expect("temporary entry should be removed");
        }
    }

    fn assert_kind(error: ConfinedFsError, expected: ConfinedFsErrorKind) {
        assert_eq!(error.kind(), expected, "unexpected error: {error}");
    }

    #[test]
    fn reads_a_file_through_the_open_root_capability() {
        let root_path = unique_temp_dir("read");
        fs::write(root_path.join("hello.txt"), b"hello").expect("fixture should be written");

        let root = ConfinedFsRoot::new(&root_path).expect("root directory should open");
        assert_eq!(
            root.read_file("hello.txt")
                .expect("file should be readable"),
            b"hello"
        );

        remove_any(&root_path);
    }

    #[test]
    fn rejects_empty_traversal_absolute_separator_prefix_and_overlong_paths() {
        let root_path = unique_temp_dir("validation");
        let root = ConfinedFsRoot::new(&root_path).expect("root directory should open");

        for path in [
            "",
            "/etc/passwd",
            "../secret",
            "a/../secret",
            "a//b",
            "a/",
            "a\\b",
        ] {
            assert_kind(
                root.read_file(path).expect_err("path should be rejected"),
                match path {
                    "" => ConfinedFsErrorKind::EmptyPath,
                    "/etc/passwd" | "a/" => ConfinedFsErrorKind::AbsolutePath,
                    "../secret" | "a/../secret" => ConfinedFsErrorKind::ParentTraversal,
                    "a\\b" => ConfinedFsErrorKind::InvalidSeparator,
                    _ => ConfinedFsErrorKind::InvalidPath,
                },
            );
        }
        assert_kind(
            root.read_file("C:secret")
                .expect_err("drive prefix should be rejected"),
            ConfinedFsErrorKind::PathPrefix,
        );
        assert_kind(
            root.read_file("a\0b").expect_err("NUL should be rejected"),
            ConfinedFsErrorKind::NulByte,
        );
        assert_kind(
            root.read_file(&"a".repeat(MAX_PATH_BYTES + 1))
                .expect_err("overlong path should be rejected"),
            ConfinedFsErrorKind::PathTooLong,
        );
        assert_kind(
            root.read_file(&"a".repeat(MAX_COMPONENT_BYTES + 1))
                .expect_err("overlong component should be rejected"),
            ConfinedFsErrorKind::ComponentTooLong,
        );

        remove_any(&root_path);
    }

    #[test]
    fn rejects_root_parent_and_leaf_symlinks_without_following_them() {
        let root_path = unique_temp_dir("symlinks");
        let outside_path = unique_temp_dir("outside");
        fs::write(outside_path.join("secret.txt"), b"outside")
            .expect("outside fixture should be written");
        fs::create_dir(root_path.join("real")).expect("real directory should be created");
        fs::write(root_path.join("real/inside.txt"), b"inside")
            .expect("inside fixture should be written");
        symlink(&outside_path, root_path.join("parent-link")).expect("parent symlink should work");
        symlink(outside_path.join("secret.txt"), root_path.join("leaf-link"))
            .expect("leaf symlink should work");

        let root = ConfinedFsRoot::new(&root_path).expect("root directory should open");
        assert_kind(
            root.read_file("parent-link/secret.txt")
                .expect_err("parent symlink must not be followed"),
            ConfinedFsErrorKind::SymlinkDenied,
        );
        assert_kind(
            root.read_file("leaf-link")
                .expect_err("leaf symlink must not be followed"),
            ConfinedFsErrorKind::SymlinkDenied,
        );
        assert_eq!(
            root.metadata("leaf-link")
                .expect("leaf metadata should be observable without following")
                .file_type(),
            ConfinedFileType::Symlink
        );
        #[cfg(target_os = "linux")]
        assert_kind(
            root.create_temp("parent-link", "temp")
                .expect_err("temporary creation must not enter a symlink parent"),
            ConfinedFsErrorKind::WrongType,
        );
        #[cfg(not(target_os = "linux"))]
        assert_kind(
            root.create_temp("parent-link", "temp")
                .expect_err("temporary creation must fail closed without Linux publication"),
            ConfinedFsErrorKind::UnsupportedPlatform,
        );

        remove_any(&root_path);
        remove_any(&outside_path);

        let root_link = unique_temp_dir("root-link-target");
        let root_link_name = unique_temp_dir("root-link-name");
        remove_any(&root_link_name);
        symlink(&root_link, &root_link_name).expect("root symlink should work");
        let error = ConfinedFsRoot::new(&root_link_name)
            .expect_err("a symlink cannot become the retained root capability");
        assert!(matches!(
            error.kind(),
            ConfinedFsErrorKind::SymlinkDenied | ConfinedFsErrorKind::WrongType
        ));
        assert!(
            !error
                .to_string()
                .contains(root_link.to_string_lossy().as_ref()),
            "root path must not appear in the default error"
        );
        remove_any(&root_link_name);
        remove_any(&root_link);
    }

    #[test]
    fn root_construction_walk_rejects_intermediate_symlinks_and_trailing_dots() {
        let base_path = unique_temp_dir("root-walk");
        let outside_path = unique_temp_dir("root-walk-outside");
        fs::create_dir(outside_path.join("target")).expect("outside target should be created");
        symlink(&outside_path, base_path.join("link")).expect("intermediate symlink should work");

        assert_kind(
            ConfinedFsRoot::new(base_path.join("link/target"))
                .expect_err("root construction must not follow an intermediate symlink"),
            ConfinedFsErrorKind::SymlinkDenied,
        );

        fs::create_dir(base_path.join("real")).expect("real directory should be created");
        assert_kind(
            ConfinedFsRoot::new(base_path.join("real/."))
                .expect_err("trailing dot components must be rejected"),
            ConfinedFsErrorKind::ParentTraversal,
        );
        fs::create_dir(base_path.join("real.")).expect("trailing-dot fixture should be created");
        assert_kind(
            ConfinedFsRoot::new(base_path.join("real."))
                .expect_err("trailing-dot aliases must be rejected"),
            ConfinedFsErrorKind::InvalidPath,
        );

        remove_any(&base_path);
        remove_any(&outside_path);
    }

    #[test]
    fn root_binding_rejects_a_renamed_traversed_parent() {
        let base_path = unique_temp_dir("root-parent-rename");
        let parent_path = base_path.join("parent");
        let root_path = parent_path.join("root");
        let moved_parent = base_path.join("parent-moved");
        fs::create_dir(&parent_path).expect("parent directory should be created");
        fs::create_dir(&root_path).expect("root directory should be created");
        fs::write(root_path.join("data"), b"original").expect("fixture should be written");
        let root = ConfinedFsRoot::new(&root_path).expect("root capability should open");

        fs::rename(&parent_path, &moved_parent).expect("root parent should be renamed");
        assert_kind(
            root.read_file("data")
                .expect_err("renamed root parent must invalidate the capability"),
            ConfinedFsErrorKind::RaceDetected,
        );
        assert_eq!(
            fs::read(moved_parent.join("root/data")).expect("moved root should remain intact"),
            b"original"
        );

        drop(root);
        remove_any(&base_path);
    }

    #[test]
    fn metadata_and_enumeration_are_bounded_and_do_not_follow_leaf_links() {
        let root_path = unique_temp_dir("enumeration");
        fs::create_dir(root_path.join("dir")).expect("directory should be created");
        fs::write(root_path.join("dir/one"), b"1").expect("fixture should be written");
        fs::write(root_path.join("dir/two"), b"2").expect("fixture should be written");
        let root = ConfinedFsRoot::new(&root_path).expect("root directory should open");

        let mut entries = root
            .enumerate("dir")
            .expect("directory should be enumerated")
            .into_iter()
            .map(|entry| (entry.name().to_owned(), entry.metadata()))
            .collect::<Vec<_>>();
        entries.sort_by(|left, right| left.0.cmp(&right.0));
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].0, "one");
        assert!(entries[0].1.is_file());
        assert_eq!(entries[0].1.link_count(), 1);

        let limited = ConfinedFsRoot::with_limits(
            &root_path,
            ConfinedFsLimits {
                max_entries: 1,
                ..ConfinedFsLimits::default()
            },
        )
        .expect("bounded root should open");
        assert_kind(
            limited
                .enumerate("dir")
                .expect_err("enumeration should report an entry budget violation"),
            ConfinedFsErrorKind::BudgetExceeded,
        );
        let examined = root
            .enumerate_with_budget(
                "dir",
                EnumerationBudget {
                    max_entries: 2,
                    max_name_bytes: 255,
                },
            )
            .expect_err("examined directory records must count toward the budget");
        assert_eq!(examined.kind(), ConfinedFsErrorKind::BudgetExceeded);
        assert!(examined.value().unwrap_or(0) > 2);
        assert_kind(
            root.enumerate_with_budget(
                "dir",
                EnumerationBudget {
                    max_entries: 8,
                    max_name_bytes: 1,
                },
            )
            .expect_err("name budget should be enforced"),
            ConfinedFsErrorKind::BudgetExceeded,
        );

        remove_any(&root_path);
    }

    #[test]
    fn enumeration_retains_non_utf8_names_without_aborting() {
        let root_path = unique_temp_dir("non-utf8");
        let raw_name = b"entry-\xff";
        let name = OsString::from_vec(raw_name.to_vec());
        fs::write(root_path.join(&name), b"bytes").expect("non-UTF-8 fixture should be written");
        let root = ConfinedFsRoot::new(&root_path).expect("root directory should open");

        let entry = root
            .enumerate("")
            .expect("enumeration should accept non-UTF-8 names")
            .into_iter()
            .find(|entry| entry.name_bytes() == raw_name)
            .expect("non-UTF-8 entry should be retained");
        assert_eq!(entry.name_os().as_bytes(), raw_name);
        assert_eq!(entry.metadata().file_type(), ConfinedFileType::File);

        remove_any(&root_path);
    }

    #[test]
    fn opening_a_fifo_is_nonblocking_and_rejected_before_use() {
        let root_path = unique_temp_dir("fifo");
        let fifo_path = root_path.join("stream");
        let fifo_name = std::ffi::CString::new(fifo_path.as_os_str().as_bytes())
            .expect("fixture path should contain no NUL");
        let result = unsafe { libc::mkfifo(fifo_name.as_ptr(), 0o600) };
        assert_eq!(result, 0, "FIFO fixture should be created");
        let root = ConfinedFsRoot::new(&root_path).expect("root directory should open");

        assert_kind(
            root.open_read("stream")
                .expect_err("FIFO must be rejected without waiting for a writer"),
            ConfinedFsErrorKind::WrongType,
        );

        remove_any(&root_path);
    }

    #[test]
    fn read_and_write_budgets_bound_allocations_and_output() {
        let root_path = unique_temp_dir("budgets");
        fs::write(root_path.join("input"), b"four").expect("fixture should be written");
        let root = ConfinedFsRoot::with_limits(
            &root_path,
            ConfinedFsLimits {
                max_read_bytes: 3,
                max_write_bytes: 3,
                ..ConfinedFsLimits::default()
            },
        )
        .expect("bounded root should open");

        let error = root
            .read_file("input")
            .expect_err("read should exceed its configured budget");
        assert_kind(error.clone(), ConfinedFsErrorKind::BudgetExceeded);
        assert_eq!(error.limit(), Some(3));
        assert_eq!(error.value(), Some(4));

        #[cfg(target_os = "linux")]
        {
            let mut temp = root
                .create_temp("", "budget")
                .expect("temporary file should be created");
            let error = temp
                .write_all(b"four")
                .expect_err("write should exceed its configured budget");
            assert_kind(error, ConfinedFsErrorKind::BudgetExceeded);
            assert_eq!(temp.bytes_written(), 0);
            temp.cleanup().expect("temporary file should be cleaned");
        }

        remove_any(&root_path);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn atomic_replace_is_same_directory_and_destination_symlinks_are_rejected() {
        let root_path = unique_temp_dir("replace");
        let outside_path = unique_temp_dir("replace-outside");
        fs::create_dir(root_path.join("parent")).expect("parent should be created");
        fs::write(root_path.join("parent/destination"), b"old")
            .expect("destination should be written");
        fs::write(outside_path.join("secret"), b"outside")
            .expect("outside fixture should be written");
        symlink(
            outside_path.join("secret"),
            root_path.join("parent/link-destination"),
        )
        .expect("destination symlink should work");
        let root = ConfinedFsRoot::new(&root_path).expect("root directory should open");

        let mut temp = root
            .create_temp("parent", "atomic")
            .expect("temporary file should be created");
        let temp_name = temp.name().to_owned();
        temp.write_all(b"new").expect("temporary write should work");
        temp.flush().expect("temporary flush should work");
        temp.sync_all().expect("temporary sync should work");
        let publication = temp
            .replace("destination")
            .expect("same-directory replacement should work");
        assert!(publication.is_published());
        assert!(publication.is_durable());
        assert!(publication.staging_cleaned());
        assert_eq!(
            fs::read(root_path.join("parent/destination")).expect("destination should be readable"),
            b"new"
        );
        assert!(!root_path.join("parent").join(temp_name).exists());
        assert_kind(
            temp.write_all(b"must-not-mutate-published-destination")
                .expect_err("a completed temporary must not remain writable"),
            ConfinedFsErrorKind::TempCompleted,
        );
        assert_kind(
            temp.flush()
                .expect_err("a completed temporary must not remain flushable"),
            ConfinedFsErrorKind::TempCompleted,
        );
        assert_kind(
            temp.sync_all()
                .expect_err("a completed temporary must not remain syncable"),
            ConfinedFsErrorKind::TempCompleted,
        );
        assert_kind(
            temp.replace("destination")
                .expect_err("a completed temporary must not publish again"),
            ConfinedFsErrorKind::TempCompleted,
        );

        let mut symlink_temp = root
            .create_temp("parent", "atomic-link")
            .expect("temporary file should be created");
        symlink_temp
            .write_all(b"must-not-escape")
            .expect("temporary write should work");
        let error = symlink_temp
            .replace("link-destination")
            .expect_err("symlink destination should be refused");
        assert_kind(error, ConfinedFsErrorKind::SymlinkDenied);
        assert_eq!(
            fs::read(outside_path.join("secret")).expect("outside file should remain readable"),
            b"outside"
        );
        symlink_temp
            .cleanup()
            .expect("temporary file should be cleaned");

        let mut invalid_parent_temp = root
            .create_temp("parent", "same-dir")
            .expect("temporary file should be created");
        let error = invalid_parent_temp
            .replace("other/destination")
            .expect_err("replacement must reject a second path");
        assert_kind(error, ConfinedFsErrorKind::InvalidSeparator);
        invalid_parent_temp
            .cleanup()
            .expect("temporary file should be cleaned");

        root.write_file("parent/destination", b"convenience")
            .expect("write_file should use confined atomic replacement");
        assert_eq!(
            fs::read(root_path.join("parent/destination")).expect("destination should be readable"),
            b"convenience"
        );
        assert_kind(
            root.write_file("parent/link-destination", b"no-follow")
                .expect_err("write_file must reject a symlink destination"),
            ConfinedFsErrorKind::SymlinkDenied,
        );

        remove_any(&root_path);
        remove_any(&outside_path);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn atomic_replace_rejects_a_temporary_from_another_root() {
        let first_path = unique_temp_dir("replace-root-a");
        let second_path = unique_temp_dir("replace-root-b");
        fs::write(first_path.join("destination"), b"first")
            .expect("first destination should be written");
        let first = ConfinedFsRoot::new(&first_path).expect("first root should open");
        let second = ConfinedFsRoot::new(&second_path).expect("second root should open");
        let temp = second
            .create_temp("", "foreign")
            .expect("second temporary should be created");

        let error = first
            .atomic_replace(temp, "destination")
            .expect_err("a root must not replace with another root's temporary");
        assert_kind(error, ConfinedFsErrorKind::CapabilityMismatch);
        assert_eq!(
            fs::read(first_path.join("destination")).expect("first destination should remain"),
            b"first"
        );

        remove_any(&first_path);
        remove_any(&second_path);
    }

    #[test]
    fn hardlinked_regular_files_are_denied_by_policy() {
        let root_path = unique_temp_dir("hardlink");
        let outside_path = unique_temp_dir("hardlink-outside");
        fs::write(outside_path.join("shared"), b"outside inode")
            .expect("outside fixture should be written");
        fs::hard_link(outside_path.join("shared"), root_path.join("linked"))
            .expect("hard link should be created");
        let root = ConfinedFsRoot::new(&root_path).expect("root directory should open");

        assert_kind(
            root.open_read("linked")
                .expect_err("hardlinked reads should be denied"),
            ConfinedFsErrorKind::HardlinkDenied,
        );
        assert_kind(
            root.metadata("linked")
                .expect_err("hardlinked metadata should be denied"),
            ConfinedFsErrorKind::HardlinkDenied,
        );

        remove_any(&root_path);
        remove_any(&outside_path);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn concurrent_temporary_creation_uses_exclusive_unique_names() {
        let root_path = unique_temp_dir("temp-collision");
        let root = Arc::new(ConfinedFsRoot::new(&root_path).expect("root directory should open"));
        let barrier = Arc::new(Barrier::new(24));
        let mut workers = Vec::new();
        for _ in 0..24 {
            let root = Arc::clone(&root);
            let barrier = Arc::clone(&barrier);
            workers.push(thread::spawn(move || {
                barrier.wait();
                let temp = root
                    .create_temp("", "collision")
                    .expect("exclusive temporary creation should succeed");
                temp.name().to_owned()
            }));
        }
        let names = workers
            .into_iter()
            .map(|worker| worker.join().expect("temporary worker should finish"))
            .collect::<HashSet<_>>();
        assert_eq!(names.len(), 24, "temporary basenames must not collide");

        drop(root);
        remove_any(&root_path);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn detects_a_deterministic_temporary_source_swap_before_rename() {
        let root_path = unique_temp_dir("source-swap");
        fs::write(root_path.join("destination"), b"old").expect("destination should be written");
        let root = ConfinedFsRoot::new(&root_path).expect("root directory should open");
        let mut temp = root
            .create_temp("", "swap")
            .expect("temporary file should be created");
        temp.write_all(b"original")
            .expect("temporary write should work");
        let temp_name = temp.name().to_owned();
        fs::rename(root_path.join(&temp_name), root_path.join("moved"))
            .expect("temporary source should be renamed");
        fs::write(root_path.join(&temp_name), b"attacker")
            .expect("replacement source should be written");

        let error = temp
            .replace("destination")
            .expect_err("source swap should be detected");
        assert_kind(error, ConfinedFsErrorKind::RaceDetected);
        assert_eq!(
            fs::read(root_path.join("moved")).expect("original source should remain"),
            b"original"
        );
        assert_eq!(
            fs::read(root_path.join(&temp_name)).expect("replacement source should remain"),
            b"attacker"
        );
        let cleanup_error = temp
            .cleanup()
            .expect_err("cleanup must report a swapped source entry");
        assert_kind(cleanup_error, ConfinedFsErrorKind::RaceDetected);
        assert_eq!(
            fs::read(root_path.join(&temp_name)).expect("replacement source must remain"),
            b"attacker"
        );
        assert_eq!(
            fs::read(root_path.join("destination")).expect("destination should remain"),
            b"old"
        );

        remove_any(&root_path);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn reports_a_temporary_source_disappearance_as_a_race() {
        let root_path = unique_temp_dir("source-disappearance");
        let root = ConfinedFsRoot::new(&root_path).expect("root directory should open");
        let mut temp = root
            .create_temp("", "gone")
            .expect("temporary file should be created");
        temp.write_all(b"original")
            .expect("temporary write should work");
        let temp_name = temp.name().to_owned();
        fs::remove_file(root_path.join(&temp_name)).expect("temporary source should be removed");

        assert_kind(
            temp.replace("destination")
                .expect_err("source disappearance should be detected"),
            ConfinedFsErrorKind::RaceDetected,
        );
        temp.cleanup()
            .expect("missing temporary source should clean up");

        remove_any(&root_path);
    }

    #[test]
    fn retained_root_descriptor_does_not_follow_root_path_replacement() {
        let root_path = unique_temp_dir("root-replacement");
        let outside_path = unique_temp_dir("root-replacement-outside");
        fs::write(outside_path.join("secret"), b"outside")
            .expect("outside fixture should be written");
        let root = ConfinedFsRoot::new(&root_path).expect("root directory should open");

        fs::remove_dir(&root_path).expect("empty root should be removable");
        symlink(&outside_path, &root_path).expect("replacement root symlink should work");
        let error = root
            .read_file("secret")
            .expect_err("retained descriptor must not follow replacement path");
        assert!(matches!(
            error.kind(),
            ConfinedFsErrorKind::RaceDetected | ConfinedFsErrorKind::SymlinkDenied
        ));
        assert_eq!(
            fs::read(outside_path.join("secret")).expect("outside file should remain"),
            b"outside"
        );

        remove_any(&root_path);
        remove_any(&outside_path);
    }

    #[test]
    fn concurrent_parent_swaps_never_return_outside_content() {
        let root_path = unique_temp_dir("concurrent-swap");
        let outside_path = unique_temp_dir("concurrent-swap-outside");
        fs::write(outside_path.join("data"), b"outside")
            .expect("outside fixture should be written");
        fs::create_dir(root_path.join("entry")).expect("initial entry should be created");
        fs::write(root_path.join("entry/data"), b"inside")
            .expect("initial inside fixture should be written");

        let root = Arc::new(ConfinedFsRoot::new(&root_path).expect("root directory should open"));
        let stop = Arc::new(AtomicBool::new(false));
        let ready = Arc::new(Barrier::new(2));
        let attacker_root = root_path.clone();
        let attacker_outside = outside_path.clone();
        let attacker_stop = Arc::clone(&stop);
        let attacker_ready = Arc::clone(&ready);
        let attacker = thread::spawn(move || {
            attacker_ready.wait();
            for index in 0..2000 {
                let entry = attacker_root.join("entry");
                let _ = fs::remove_file(&entry);
                let _ = fs::remove_dir_all(&entry);
                if index % 2 == 0 {
                    let _ = symlink(&attacker_outside, &entry);
                } else if fs::create_dir(&entry).is_ok() {
                    let _ = fs::write(entry.join("data"), b"inside");
                }
            }
            attacker_stop.store(true, Ordering::Release);
        });

        ready.wait();
        let mut attempts = 0;
        while !stop.load(Ordering::Acquire) || attempts < 2000 {
            attempts += 1;
            match root.read_file("entry/data") {
                Ok(bytes) => assert!(
                    bytes.is_empty() || bytes == b"inside",
                    "outside bytes escaped the root"
                ),
                Err(error) => assert!(
                    matches!(
                        error.kind(),
                        ConfinedFsErrorKind::NotFound
                            | ConfinedFsErrorKind::SymlinkDenied
                            | ConfinedFsErrorKind::WrongType
                            | ConfinedFsErrorKind::Io
                    ),
                    "unexpected concurrent swap error: {error}"
                ),
            }
            if attempts > 20_000 {
                break;
            }
        }
        attacker.join().expect("swap worker should finish");
        assert!(attempts >= 2000);
        assert_eq!(
            fs::read(outside_path.join("data")).expect("outside fixture should remain"),
            b"outside"
        );

        remove_any(&root_path);
        remove_any(&outside_path);
    }

    #[test]
    fn open_directory_selects_nested_and_empty_root_paths() {
        let root_path = unique_temp_dir("open-directory");
        fs::create_dir(root_path.join("nested")).expect("nested directory should be created");
        fs::create_dir(root_path.join("nested/leaf")).expect("leaf directory should be created");
        let root = ConfinedFsRoot::new(&root_path).expect("root directory should open");

        let nested = root
            .open_directory("nested/leaf")
            .expect("nested directory should open");
        let selected_root = root
            .open_directory("")
            .expect("empty path should select the retained root");
        let cloned = nested.clone();

        let debug = format!("{nested:?}{selected_root:?}{cloned:?}");
        assert!(
            !debug.contains(root_path.to_string_lossy().as_ref()),
            "directory debug must not leak the root path: {debug}"
        );
        assert!(
            !debug.contains("nested"),
            "directory debug must not leak the relative path: {debug}"
        );
        assert!(
            !debug.contains("fd"),
            "directory debug must not leak a descriptor: {debug}"
        );
        let _ = ConfinedDirectory::clone(&cloned);

        remove_any(&root_path);
    }

    #[test]
    fn open_directory_denies_symlink_components() {
        let root_path = unique_temp_dir("open-directory-symlink");
        let outside_path = unique_temp_dir("open-directory-symlink-outside");
        fs::create_dir(outside_path.join("leaf")).expect("outside leaf should be created");
        symlink(&outside_path, root_path.join("link")).expect("parent symlink should work");
        let root = ConfinedFsRoot::new(&root_path).expect("root directory should open");

        assert_kind(
            root.open_directory("link/leaf")
                .expect_err("symlink component must not be followed"),
            ConfinedFsErrorKind::SymlinkDenied,
        );

        remove_any(&root_path);
        remove_any(&outside_path);
    }
}

#[cfg(all(unix, not(target_os = "linux")))]
#[test]
fn unix_without_renameat2_does_not_create_unpublished_temps() {
    let error = match ConfinedFsRoot::new(".") {
        Ok(root) => {
            assert!(
                !root.supports_atomic_publication(),
                "non-Linux Unix must not advertise atomic publication"
            );
            root.create_temp("", "tmp")
                .expect_err("create_temp must fail closed without Linux publication")
        }
        Err(error) => error,
    };
    assert_eq!(error.kind(), ConfinedFsErrorKind::UnsupportedPlatform);
}

#[cfg(not(unix))]
#[test]
fn unsupported_targets_fail_closed_without_path_based_fallbacks() {
    let error = match ConfinedFsRoot::new(".") {
        Ok(_) => panic!("unsupported target unexpectedly created a capability"),
        Err(error) => error,
    };
    assert_eq!(error.kind(), ConfinedFsErrorKind::UnsupportedPlatform);
}
