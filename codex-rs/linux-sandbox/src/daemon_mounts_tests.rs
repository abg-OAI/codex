use super::*;
use pretty_assertions::assert_eq;
use test_case::test_case;

// Most cases have no independently masked subtree.
fn check_mounts(
    directory: &Path,
    device: &str,
    mount_id: Option<&str>,
    mountinfo: &[u8],
) -> io::Result<DaemonSocketMounts> {
    super::check_mounts(
        directory, device, mount_id, mountinfo, /*masked_root*/ None,
    )
}

#[test_case("/tmp", "/host-tmp", "/host-tmp/codex-daemon-1000"; "ancestor alias")]
#[test_case("/tmp/codex-daemon-1000", "/alias", "/alias"; "directory alias")]
#[test_case("/", "/host", "/host/tmp/codex-daemon-1000"; "root alias")]
#[test_case("/tmp", "/host\\040tmp", "/host tmp/codex-daemon-1000"; "escaped alias")]
fn discovers_complete_directory_aliases(root: &str, destination: &str, expected_alias: &str) {
    let mounts =
        format!("1 0 0:1 / / rw - ext4 disk rw\n2 1 0:1 {root} {destination} rw - ext4 disk rw\n");
    for mount_id in [Some("1"), None] {
        assert_eq!(
            check_mounts(
                Path::new("/tmp/codex-daemon-1000"),
                "0:1",
                mount_id,
                mounts.as_bytes()
            )
            .unwrap()
            .directories,
            BTreeSet::from([
                PathBuf::from("/tmp/codex-daemon-1000"),
                PathBuf::from(expected_alias),
            ]),
            "mount_id: {mount_id:?}"
        );
    }
}

#[test_case("/tmp/codex-daemon-1000/rpc.sock", "/alias.sock"; "socket alias")]
#[test_case("/tmp/codex-daemon-1000/private", "/private"; "subdirectory alias")]
#[test_case("/other", "/tmp/codex-daemon-1000/nested"; "nested mount")]
fn rejects_mounts_that_expose_less_than_the_complete_directory(root: &str, destination: &str) {
    let mounts =
        format!("1 0 0:1 / / rw - ext4 disk rw\n2 1 0:1 {root} {destination} rw - ext4 disk rw\n");
    for mount_id in [Some("1"), None] {
        assert!(
            check_mounts(
                Path::new("/tmp/codex-daemon-1000"),
                "0:1",
                mount_id,
                mounts.as_bytes(),
            )
            .is_err(),
            "mount_id: {mount_id:?}",
        );
    }
}

#[test_case("/workspace", "/project"; "unrelated project bind")]
#[test_case("/tmp", "/tmp"; "same location")]
fn ignores_mounts_without_an_additional_directory_alias(root: &str, destination: &str) {
    let mounts =
        format!("1 0 0:1 / / rw - ext4 disk rw\n2 1 0:1 {root} {destination} rw - ext4 disk rw\n");
    let visible_mount = if destination == "/tmp" { "2" } else { "1" };
    for mount_id in [Some(visible_mount), None] {
        assert_eq!(
            check_mounts(
                Path::new("/tmp/codex-daemon-1000"),
                "0:1",
                mount_id,
                mounts.as_bytes(),
            )
            .unwrap()
            .directories,
            BTreeSet::from([PathBuf::from("/tmp/codex-daemon-1000")]),
            "mount_id: {mount_id:?}",
        );
    }
}

#[test_case("0:2", "mnt:[4026532835]", "/run/snapd/ns/example.mnt", Ok(()); "unrelated mount namespace")]
#[test_case("0:2", "net:[4026531840]", "/run/netns/example", Ok(()); "unrelated network namespace")]
#[test_case("0:2", "mnt:[4026532835]", "/tmp/codex-daemon-1000/ns", Err(io::ErrorKind::PermissionDenied); "nested namespace mount")]
#[test_case("0:1", "mnt:[4026532835]", "/run/snapd/ns/example.mnt", Err(io::ErrorKind::Other); "non-path root on socket filesystem")]
#[test_case("0:2", "mnt:[4026532835]", "relative/ns", Err(io::ErrorKind::Other); "relative destination")]
#[test_case("0:2", "mnt:[4026532835]", "/run/snapd/ns/\\invalid", Err(io::ErrorKind::Other); "invalid destination escape")]
fn validates_namespace_mounts_by_device_and_destination(
    device: &str,
    root: &str,
    destination: &str,
    expected: Result<(), io::ErrorKind>,
) {
    let mounts = format!(
        "1 0 0:1 / / rw - ext4 disk rw\n2 1 {device} {root} {destination} rw - nsfs nsfs rw\n"
    );
    for mount_id in [Some("1"), None] {
        assert_eq!(
            check_mounts(
                Path::new("/tmp/codex-daemon-1000"),
                "0:1",
                mount_id,
                mounts.as_bytes(),
            )
            .map(|_| ())
            .map_err(|error| error.kind()),
            expected,
            "mount_id: {mount_id:?}",
        );
    }
}

#[test]
fn unrelated_namespace_mount_does_not_hide_directory_alias() {
    let mounts = b"1 0 0:1 / / rw - ext4 disk rw\n\
                   2 1 0:2 net:[4026531840] /run/netns/example rw - nsfs nsfs rw\n\
                   3 1 0:1 /tmp/codex-daemon-1000 /alias rw - ext4 disk rw\n";
    for mount_id in [Some("1"), None] {
        assert_eq!(
            check_mounts(Path::new("/tmp/codex-daemon-1000"), "0:1", mount_id, mounts,)
                .unwrap()
                .directories,
            BTreeSet::from([
                PathBuf::from("/alias"),
                PathBuf::from("/tmp/codex-daemon-1000"),
            ]),
            "mount_id: {mount_id:?}",
        );
    }
}

#[test]
fn discovers_source_alias_when_tmp_is_itself_a_bind_mount() {
    let mounts = b"1 0 0:1 / / rw - ext4 disk rw\n2 1 0:1 /backing/tmp /tmp rw - ext4 disk rw\n";
    assert_eq!(
        check_mounts(
            Path::new("/tmp/codex-daemon-1000"),
            "0:1",
            Some("2"),
            mounts
        )
        .unwrap()
        .directories,
        BTreeSet::from([
            PathBuf::from("/backing/tmp/codex-daemon-1000"),
            PathBuf::from("/tmp/codex-daemon-1000"),
        ]),
    );
    // A hidden deeper mount must not override the actual /tmp backing location.
    let hidden = [
        mounts.as_slice(),
        b"3 1 0:1 /tmp/codex-daemon-1000 /tmp/codex-daemon-1000 rw - ext4 disk rw\n",
    ]
    .concat();
    assert_eq!(
        check_mounts(
            Path::new("/tmp/codex-daemon-1000"),
            "0:1",
            Some("2"),
            &hidden
        )
        .unwrap()
        .directories,
        BTreeSet::from([
            PathBuf::from("/backing/tmp/codex-daemon-1000"),
            PathBuf::from("/tmp/codex-daemon-1000"),
        ]),
    );
}

#[test]
fn accepts_private_tmp_filesystem_and_resolves_stacked_mounts() {
    let mounts = "1 0 0:1 / / rw - ext4 disk rw\n2 1 0:2 / /tmp rw - tmpfs tmpfs rw\n";
    let directory = Path::new("/tmp/codex-daemon-1000");
    assert!(check_mounts(directory, "0:2", Some("2"), mounts.as_bytes()).is_ok());
    assert!(check_mounts(directory, "0:2", /*mount_id*/ None, mounts.as_bytes()).is_ok());
    // Missing or inconsistent precise IDs must not fall back to the otherwise
    // acceptable conservative interpretation.
    assert!(check_mounts(directory, "0:2", Some("missing"), mounts.as_bytes()).is_err());
    assert!(check_mounts(directory, "0:2", Some("1"), mounts.as_bytes()).is_err());
    assert!(check_mounts(directory, "0:3", /*mount_id*/ None, mounts.as_bytes()).is_err());
    let stacked = format!("{mounts}3 2 0:2 /other /tmp rw - tmpfs tmpfs rw\n");
    assert!(check_mounts(directory, "0:2", Some("3"), stacked.as_bytes()).is_ok());
    assert!(check_mounts(directory, "0:2", /*mount_id*/ None, stacked.as_bytes()).is_err());
}

#[test]
fn rejects_open_mount_that_has_been_covered() {
    let directory = Path::new("/tmp/codex-daemon-1000");
    let mounts = "1 0 0:1 / / rw - ext4 disk rw\n\
                  2 1 0:1 /tmp/private-old/tmp /tmp rw - ext4 disk rw\n\
                  3 2 0:1 /tmp/private-new/tmp /tmp rw - ext4 disk rw\n";
    assert!(check_mounts(directory, "0:1", Some("2"), mounts.as_bytes()).is_err());
    assert!(check_mounts(directory, "0:1", Some("3"), mounts.as_bytes()).is_ok());
    let exposed = format!(
        "{mounts}4 1 0:1 /tmp/private-new/tmp/codex-daemon-1000 /outside rw - ext4 disk rw\n"
    );
    assert!(check_mounts(directory, "0:1", Some("2"), exposed.as_bytes()).is_err());
    assert_eq!(
        check_mounts(directory, "0:1", Some("3"), exposed.as_bytes())
            .unwrap()
            .directories,
        BTreeSet::from([directory.to_path_buf(), PathBuf::from("/outside")]),
    );
    assert!(check_mounts(directory, "0:1", None, exposed.as_bytes()).is_err());
}

#[test]
fn rejects_mount_hidden_by_an_ancestor_overmount() {
    let directory = Path::new("/tmp/private/codex-daemon-1000");
    let mounts = "1 0 0:1 / / rw - ext4 disk rw\n\
                  2 1 0:2 / /tmp rw - tmpfs tmpfs rw\n\
                  3 2 0:3 / /tmp/private rw - tmpfs tmpfs rw\n";
    assert!(check_mounts(directory, "0:3", Some("3"), mounts.as_bytes()).is_ok());
    let covered = format!("{mounts}4 2 0:2 /other /tmp rw - tmpfs tmpfs rw\n");
    assert!(check_mounts(directory, "0:3", Some("3"), covered.as_bytes()).is_err());
    assert!(check_mounts(directory, "0:2", Some("4"), covered.as_bytes()).is_ok());
}

#[test_case("/tmp/systemd-private-service/tmp", "/"; "tmp on root filesystem")]
#[test_case("/systemd-private-service/tmp", "/tmp"; "tmp on separate filesystem")]
fn discovers_complete_aliases_for_private_tmp_bind(root: &str, parent: &str) {
    let directory = Path::new("/tmp/codex-daemon-1000");
    let mounts =
        format!("1 0 0:1 / {parent} rw - ext4 disk rw\n2 1 0:1 {root} /tmp rw - ext4 disk rw\n");
    assert_eq!(
        check_mounts(directory, "0:1", Some("2"), mounts.as_bytes())
            .unwrap()
            .directories,
        BTreeSet::from([directory.to_path_buf()]),
    );
    // PrivateTmp remains ambiguous when neither fdinfo nor statx supplies an ID.
    assert!(check_mounts(directory, "0:1", /*mount_id*/ None, mounts.as_bytes()).is_err());

    let directory_alias = format!("{mounts}3 2 0:1 {root} /tmp/exposed rw - ext4 disk rw\n");
    assert_eq!(
        check_mounts(directory, "0:1", Some("2"), directory_alias.as_bytes())
            .unwrap()
            .directories,
        BTreeSet::from([
            directory.to_path_buf(),
            PathBuf::from("/tmp/exposed/codex-daemon-1000"),
        ]),
    );

    let root_alias = format!("{mounts}3 2 0:1 / /host rw - ext4 disk rw\n");
    assert!(check_mounts(directory, "0:1", Some("2"), root_alias.as_bytes()).is_ok());

    let socket_alias = format!(
        "{mounts}3 2 0:1 {root}/codex-daemon-1000/rpc.sock /tmp/alias.sock rw - ext4 disk rw\n"
    );
    assert!(check_mounts(directory, "0:1", Some("2"), socket_alias.as_bytes()).is_err());
}

#[test]
fn masked_wslg_alias_is_omitted_without_hiding_other_aliases() {
    let mounts = "1 0 0:1 / / rw - ext4 disk rw\n2 1 0:1 / /mnt/wslg/distro rw - ext4 disk rw\n";
    let directory = Path::new("/tmp/codex-daemon-1000");
    let mask = Some(Path::new(crate::bwrap::WSLG_DISTRO_ROOT));
    let exposed = format!("{mounts}3 1 0:1 /tmp /host-tmp rw - ext4 disk rw\n");
    let socket_alias =
        format!("{mounts}3 1 0:1 /tmp/codex-daemon-1000/rpc.sock /alias.sock rw - ext4 disk rw\n");
    for mount_id in [Some("1"), None] {
        assert!(check_mounts(directory, "0:1", mount_id, mounts.as_bytes()).is_ok());
        assert_eq!(
            super::check_mounts(directory, "0:1", mount_id, mounts.as_bytes(), mask)
                .unwrap()
                .directories,
            BTreeSet::from([directory.to_path_buf()]),
        );
        assert_eq!(
            super::check_mounts(directory, "0:1", mount_id, exposed.as_bytes(), mask)
                .unwrap()
                .directories,
            BTreeSet::from([
                PathBuf::from("/host-tmp/codex-daemon-1000"),
                directory.to_path_buf(),
            ]),
        );
        assert!(
            super::check_mounts(directory, "0:1", mount_id, socket_alias.as_bytes(), mask).is_err()
        );
    }
}
