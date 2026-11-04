//! Real filesystem space for node hardware collection.
//!
//! It lists the host's real filesystems, those on a disk rather than virtual, network, removable
//! or container-runtime mounts, once per snapshot, and reads their used, available and total space
//! from filesystem statistics on every sample, never by walking directories. A filesystem mounted
//! at several places, or several volumes sharing one storage pool such as an APFS container or a
//! ZFS pool, are reported once, under the shortest mount point, so a disk never counts twice. A
//! filesystem unmounted since the listing is left out rather than reported with the numbers of the
//! one beneath it.

use std::{
    collections::{BTreeMap, HashMap},
    ffi::CString,
    mem::MaybeUninit,
    os::unix::fs::MetadataExt,
    sync::Mutex,
};

use dotcore_domain::hardware_observation::FilesystemSample;
use dotcore_proto::dotcore::observation::v1::FilesystemFacts;
use tracing::debug;

/// The host's real filesystems, listed on the snapshot cadence and read on every sample.
pub(crate) struct Filesystems {
    /// Filesystems of the last listing; `None` until the first one.
    known: Mutex<Option<Vec<KnownFilesystem>>>,
}

impl Filesystems {
    pub(crate) fn new() -> Self {
        Self {
            known: Mutex::new(None),
        }
    }

    /// Lists the host's real filesystems again and returns the facts of those whose statistics
    /// could be read; later samples read exactly these.
    pub(crate) fn discover(&self) -> Vec<FilesystemFacts> {
        let samples: Vec<_> = real_filesystems(system_mounts())
            .into_iter()
            .filter_map(|filesystem| {
                let known = KnownFilesystem::identify(filesystem)?;
                let space = known.read_space()?;
                Some((known, space))
            })
            .collect();
        let facts = samples
            .iter()
            .map(|(_, space)| FilesystemFacts {
                mountpoint: space.mountpoint.clone(),
                fstype: space.fstype.clone(),
                size_bytes: space.size_bytes,
            })
            .collect();
        *self.lock() = Some(samples.into_iter().map(|(known, _)| known).collect());
        facts
    }

    /// Reads the space of every listed filesystem, listing them first when no snapshot did yet;
    /// a filesystem whose statistics fail this time, or no longer mounted at its mount point, is
    /// left out.
    pub(crate) fn sample(&self) -> Vec<FilesystemSample> {
        let known = self.lock().clone();
        let filesystems = match known {
            Some(filesystems) => filesystems,
            None => {
                self.discover();
                self.lock().clone().unwrap_or_default()
            }
        };
        filesystems
            .iter()
            .filter_map(KnownFilesystem::read_space)
            .collect()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Option<Vec<KnownFilesystem>>> {
        self.known
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

/// One mounted filesystem as the host lists it.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Mount {
    mountpoint: String,
    fstype: String,

    /// What is mounted: a device path, a dataset name, or a pseudo source such as `tmpfs`.
    device: String,

    /// Whether the host shows the mount to users; macOS hides internal images, such as simulator
    /// runtimes and cryptexes. Always true on Linux.
    browsable: bool,
}

/// One real filesystem: the mount it is reported under, and the mounts its space is read from. Only
/// a ZFS pool reads several, since each dataset knows only its own used space.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Filesystem {
    mount: Mount,
    members: Vec<Mount>,
}

impl Filesystem {
    /// The filesystem of one storage pool's `mounts`, or `None` when there are none.
    fn of_pool(mut mounts: Vec<Mount>) -> Option<Self> {
        mounts.sort_by(|a, b| mount_point_rank(a).cmp(&mount_point_rank(b)));
        let mount = mounts.first()?.clone();
        if mount.fstype != ZFS {
            mounts.truncate(1);
        }
        Some(Self {
            mount,
            members: mounts,
        })
    }
}

/// A listed filesystem with the mounts its space is read from.
#[derive(Clone, Debug, PartialEq, Eq)]
struct KnownFilesystem {
    mount: Mount,
    members: Vec<KnownMount>,
}

impl KnownFilesystem {
    /// Records the device ids of `filesystem`'s members, or `None` when none can be read.
    fn identify(filesystem: Filesystem) -> Option<Self> {
        let members: Vec<_> = filesystem
            .members
            .into_iter()
            .filter_map(KnownMount::identify)
            .collect();
        (!members.is_empty()).then_some(Self {
            mount: filesystem.mount,
            members,
        })
    }

    /// Reads the space of every member and combines it, or `None` when no member can be read.
    fn read_space(&self) -> Option<FilesystemSample> {
        let spaces = self.members.iter().filter_map(KnownMount::read_space);
        combined_space(&self.mount, spaces)
    }
}

/// A mounted filesystem with the device id its mount point had when listed.
#[derive(Clone, Debug, PartialEq, Eq)]
struct KnownMount {
    mount: Mount,
    device_id: u64,
}

impl KnownMount {
    /// Records the device id now at the mount point, or `None` when it cannot be read.
    fn identify(mount: Mount) -> Option<Self> {
        let device_id = mount_point_device_id(&mount.mountpoint)?;
        Some(Self { mount, device_id })
    }

    /// Reads the space of the filesystem, or `None` when its mount point now holds another
    /// device, as after an unmount exposes the parent filesystem.
    fn read_space(&self) -> Option<FilesystemSample> {
        if mount_point_device_id(&self.mount.mountpoint) != Some(self.device_id) {
            debug!(
                mountpoint = %self.mount.mountpoint,
                "filesystem no longer mounted at its mount point"
            );
            return None;
        }
        read_space(&self.mount)
    }
}

const ZFS: &str = "zfs";

/// Filesystem types on a disk even when their source is not a device path, as for zfs datasets.
const DISK_FILESYSTEM_TYPES: &[&str] = &[
    "apfs", "bcachefs", "btrfs", "exfat", "ext2", "ext3", "ext4", "f2fs", "hfs", "jfs", "ntfs",
    "ntfs3", "vfat", "xfs", "zfs",
];

/// Virtual, network and container filesystem types; network types are also excluded because
/// their statistics can block on an unreachable server.
const EXCLUDED_FILESYSTEM_TYPES: &[&str] = &[
    "9p",
    "afpfs",
    "autofs",
    "binfmt_misc",
    "bpf",
    "ceph",
    "cgroup",
    "cgroup2",
    "cifs",
    "configfs",
    "debugfs",
    "devfs",
    "devpts",
    "devtmpfs",
    "efivarfs",
    "glusterfs",
    "hugetlbfs",
    "iso9660",
    "mqueue",
    "nfs",
    "nfs4",
    "nsfs",
    "nullfs",
    "overlay",
    "proc",
    "pstore",
    "ramfs",
    "rpc_pipefs",
    "securityfs",
    "smb3",
    "smbfs",
    "squashfs",
    "sysfs",
    "tmpfs",
    "tracefs",
    "virtiofs",
    "webdav",
];

/// Mount point trees of boot partitions, of container runtime storage, which mounts one
/// filesystem per image layer, and of removable or attached media.
const EXCLUDED_MOUNT_TREES: &[&str] = &[
    "/boot",
    "/media",
    "/run/containerd",
    "/run/media",
    "/var/lib/containers",
    "/var/lib/docker",
    "/var/lib/rancher",
    "/Volumes",
];

/// Path components of container runtime storage moved elsewhere, such as a relocated Docker data
/// root.
const CONTAINER_RUNTIME_COMPONENTS: &[&str] = &["/docker/", "/containerd/"];

/// macOS system volumes, which either share the root volume's container or hold internal firmware
/// data.
const MACOS_SYSTEM_VOLUMES: &str = "/System/Volumes";

/// Keeps the real filesystems of `mounts`, given in mount order, one per storage pool under its
/// shortest mount point, sorted by mount point. A mount hidden by a later mount at the same mount
/// point is dropped first, since only the later one is visible there, and a device mounted at
/// several places then keeps its shortest mount point.
fn real_filesystems(mounts: Vec<Mount>) -> Vec<Filesystem> {
    let last_at_mount_point: HashMap<String, usize> = mounts
        .iter()
        .enumerate()
        .map(|(index, mount)| (mount.mountpoint.clone(), index))
        .collect();
    let visible = mounts
        .into_iter()
        .enumerate()
        .filter(|(index, mount)| last_at_mount_point.get(&mount.mountpoint) == Some(index))
        .map(|(_, mount)| mount);

    let mut by_device: BTreeMap<String, Mount> = BTreeMap::new();
    for mount in visible.filter(is_real_filesystem) {
        match by_device.get(&mount.device) {
            Some(kept) if mount_point_rank(kept) <= mount_point_rank(&mount) => {}
            _ => {
                by_device.insert(mount.device.clone(), mount);
            }
        }
    }

    let mut by_pool: BTreeMap<String, Vec<Mount>> = BTreeMap::new();
    for mount in by_device.into_values() {
        by_pool
            .entry(storage_pool(&mount).to_owned())
            .or_default()
            .push(mount);
    }
    let mut filesystems: Vec<_> = by_pool
        .into_values()
        .filter_map(Filesystem::of_pool)
        .collect();
    filesystems.sort_by(|a, b| a.mount.mountpoint.cmp(&b.mount.mountpoint));
    filesystems
}

/// Orders mounts shortest mount point first, then alphabetically.
fn mount_point_rank(mount: &Mount) -> (usize, &str) {
    (mount.mountpoint.len(), &mount.mountpoint)
}

fn is_real_filesystem(mount: &Mount) -> bool {
    let fstype = mount.fstype.as_str();
    if EXCLUDED_FILESYSTEM_TYPES.contains(&fstype) || fstype.starts_with("fuse") {
        return false;
    }
    let mountpoint = mount.mountpoint.as_str();
    if EXCLUDED_MOUNT_TREES
        .iter()
        .any(|tree| is_within(mountpoint, tree))
        || CONTAINER_RUNTIME_COMPONENTS
            .iter()
            .any(|component| mountpoint.contains(component))
        || is_within(mountpoint, MACOS_SYSTEM_VOLUMES)
        || !mount.browsable
        || (fstype == ZFS && is_container_layer_dataset(&mount.device))
    {
        return false;
    }
    DISK_FILESYSTEM_TYPES.contains(&fstype) || mount.device.starts_with("/dev/")
}

/// What holds the space of `mount`: mounts of one storage pool share their free space, so they
/// count once. Every other filesystem has its device to itself.
fn storage_pool(mount: &Mount) -> &str {
    let device = mount.device.as_str();
    match mount.fstype.as_str() {
        "apfs" => apfs_container(device).unwrap_or(device),
        ZFS => device.split_once('/').map_or(device, |(pool, _)| pool),
        _ => device,
    }
}

/// The container disk of an APFS volume device, which names its volumes and their snapshots as
/// slices of that disk.
fn apfs_container(device: &str) -> Option<&str> {
    const DISK_PREFIX: &str = "/dev/disk";

    let unit = device.strip_prefix(DISK_PREFIX)?;
    let digits = unit.find(|c: char| !c.is_ascii_digit())?;
    (digits > 0 && unit[digits..].starts_with('s')).then(|| &device[..DISK_PREFIX.len() + digits])
}

/// Whether a zfs dataset is a container image layer, which container runtimes name by a 64-digit
/// hexadecimal layer id, with an `-init` suffix for Docker's initial layer.
fn is_container_layer_dataset(dataset: &str) -> bool {
    let name = dataset.rsplit('/').next().unwrap_or(dataset);
    let id = name.strip_suffix("-init").unwrap_or(name);
    id.len() == 64 && id.bytes().all(|byte| byte.is_ascii_hexdigit())
}

/// Whether `path` is `tree` or below it.
fn is_within(path: &str, tree: &str) -> bool {
    path.strip_prefix(tree)
        .is_some_and(|rest| rest.is_empty() || rest.starts_with('/'))
}

/// The space of `mount` from the spaces of its members, or `None` when there are none. Members
/// share their free space, so it counts once, and a quota can only lower a member's view of it.
fn combined_space(
    mount: &Mount,
    spaces: impl Iterator<Item = FilesystemSample>,
) -> Option<FilesystemSample> {
    let reported = |space: FilesystemSample| FilesystemSample {
        mountpoint: mount.mountpoint.clone(),
        ..space
    };
    spaces.map(reported).reduce(|total, member| {
        let used_bytes = total.used_bytes.saturating_add(member.used_bytes);
        let available_bytes = total.available_bytes.max(member.available_bytes);
        FilesystemSample {
            size_bytes: used_bytes.saturating_add(available_bytes),
            used_bytes,
            available_bytes,
            ..total
        }
    })
}

/// Reads the space of `mount` from one filesystem statistics call, or `None` when it fails.
fn read_space(mount: &Mount) -> Option<FilesystemSample> {
    let path = CString::new(mount.mountpoint.as_str()).ok()?;
    let space = match block_counts(&path) {
        Ok(space) => space,
        Err(error) => {
            debug!(
                mountpoint = %mount.mountpoint,
                error = %error,
                "filesystem statistics unavailable"
            );
            return None;
        }
    };
    Some(FilesystemSample {
        mountpoint: mount.mountpoint.clone(),
        fstype: mount.fstype.clone(),
        size_bytes: space.blocks.saturating_mul(space.block_size),
        used_bytes: space
            .blocks
            .saturating_sub(space.free)
            .saturating_mul(space.block_size),
        available_bytes: space.available.saturating_mul(space.block_size),
    })
}

/// Block counts of one filesystem, in units of `block_size` bytes.
struct BlockCounts {
    blocks: u64,

    /// Free blocks, including those reserved for the superuser.
    free: u64,

    /// Free blocks an unprivileged writer can use.
    available: u64,

    block_size: u64,
}

/// Reads the block counts of the filesystem at `path` through `statvfs`.
#[cfg(target_os = "linux")]
fn block_counts(path: &CString) -> std::io::Result<BlockCounts> {
    let mut stat = MaybeUninit::<libc::statvfs>::uninit();
    // SAFETY: `path` is a NUL-terminated string and `stat` is a writable statvfs the call fills
    // when it returns zero.
    if unsafe { libc::statvfs(path.as_ptr(), stat.as_mut_ptr()) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: statvfs returned zero, so it initialized `stat`.
    let stat = unsafe { stat.assume_init() };
    Ok(BlockCounts {
        blocks: stat.f_blocks,
        free: stat.f_bfree,
        available: stat.f_bavail,
        block_size: stat.f_frsize,
    })
}

/// Reads the block counts of the filesystem at `path` through `statfs`, whose 64-bit counts never
/// truncate on large volumes, unlike the 32-bit counts of `statvfs` on macOS.
#[cfg(target_os = "macos")]
fn block_counts(path: &CString) -> std::io::Result<BlockCounts> {
    let mut stat = MaybeUninit::<libc::statfs>::uninit();
    // SAFETY: `path` is a NUL-terminated string and `stat` is a writable statfs the call fills
    // when it returns zero.
    if unsafe { libc::statfs(path.as_ptr(), stat.as_mut_ptr()) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: statfs returned zero, so it initialized `stat`.
    let stat = unsafe { stat.assume_init() };
    Ok(BlockCounts {
        blocks: stat.f_blocks,
        free: stat.f_bfree,
        available: stat.f_bavail,
        block_size: u64::from(stat.f_bsize),
    })
}

/// The id of the device holding `mountpoint`, or `None` when it cannot be read.
fn mount_point_device_id(mountpoint: &str) -> Option<u64> {
    std::fs::metadata(mountpoint)
        .map(|metadata| metadata.dev())
        .ok()
}

/// The mounted filesystems the kernel lists for this process.
#[cfg(target_os = "linux")]
fn system_mounts() -> Vec<Mount> {
    match std::fs::read_to_string("/proc/self/mounts") {
        Ok(text) => parse_mounts(&text),
        Err(error) => {
            debug!(error = %error, "mount list unavailable");
            Vec::new()
        }
    }
}

/// The local mounted filesystems, listed without waiting on any filesystem.
#[cfg(target_os = "macos")]
fn system_mounts() -> Vec<Mount> {
    // SAFETY: a null buffer only asks for the number of mounted filesystems.
    let count = unsafe { libc::getfsstat(std::ptr::null_mut(), 0, libc::MNT_NOWAIT) };
    let Ok(capacity) = usize::try_from(count) else {
        return Vec::new();
    };
    let Ok(buffer_size) =
        libc::c_int::try_from(capacity.saturating_mul(std::mem::size_of::<libc::statfs>()))
    else {
        return Vec::new();
    };
    let mut entries: Vec<libc::statfs> = Vec::with_capacity(capacity);
    // SAFETY: the buffer has room for `buffer_size` bytes of entries, and getfsstat writes at
    // most that many, returning how many entries it wrote.
    let written = unsafe { libc::getfsstat(entries.as_mut_ptr(), buffer_size, libc::MNT_NOWAIT) };
    let Ok(written) = usize::try_from(written) else {
        return Vec::new();
    };
    // SAFETY: getfsstat initialized the first `written` entries, never more than the capacity.
    unsafe { entries.set_len(written.min(capacity)) };
    entries
        .iter()
        .filter(|entry| entry.f_flags & libc::MNT_LOCAL as u32 != 0)
        .filter_map(|entry| {
            Some(Mount {
                mountpoint: c_string(&entry.f_mntonname)?,
                fstype: c_string(&entry.f_fstypename)?,
                device: c_string(&entry.f_mntfromname)?,
                browsable: entry.f_flags & libc::MNT_DONTBROWSE as u32 == 0,
            })
        })
        .collect()
}

/// A NUL-terminated UTF-8 string of a fixed-size C buffer.
#[cfg(target_os = "macos")]
fn c_string(chars: &[libc::c_char]) -> Option<String> {
    let bytes = chars
        .iter()
        .take_while(|c| **c != 0)
        .map(|c| *c as u8)
        .collect();
    String::from_utf8(bytes).ok()
}

/// Parses `/proc/self/mounts` text: source, mount point and type per line, with spaces and other
/// special characters octal-escaped.
#[cfg(any(target_os = "linux", test))]
fn parse_mounts(text: &str) -> Vec<Mount> {
    text.lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            Some(Mount {
                device: unescape_mount_field(fields.next()?),
                mountpoint: unescape_mount_field(fields.next()?),
                fstype: fields.next()?.to_owned(),
                browsable: true,
            })
        })
        .collect()
}

/// Decodes the kernel's three-digit octal escapes, such as `\040` for a space.
#[cfg(any(target_os = "linux", test))]
fn unescape_mount_field(field: &str) -> String {
    let bytes = field.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        let escape = bytes
            .get(index + 1..index + 4)
            .filter(|_| bytes[index] == b'\\')
            .and_then(|digits| std::str::from_utf8(digits).ok())
            .and_then(|digits| u8::from_str_radix(digits, 8).ok());
        match escape {
            Some(byte) => {
                decoded.push(byte);
                index += 4;
            }
            None => {
                decoded.push(bytes[index]);
                index += 1;
            }
        }
    }
    String::from_utf8_lossy(&decoded).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disk_filesystems_are_kept_and_virtual_network_and_fuse_ones_dropped() {
        let mounts = [
            mount("/dev/nvme0n1p2", "/", "ext4"),
            mount("/dev/sda1", "/data", "xfs"),
            mount("/dev/sdb1", "/srv", "btrfs"),
            mount("rpool/home", "/home", "zfs"),
            mount("/dev/disk3s1s1", "/Applications", "apfs"),
            mount("tmpfs", "/tmp", "tmpfs"),
            mount("overlay", "/merged", "overlay"),
            mount("/dev/loop0", "/snap/core/1", "squashfs"),
            mount("proc", "/proc", "proc"),
            mount("server:/export", "/mnt/nfs", "nfs4"),
            mount("//server/share", "/mnt/smb", "cifs"),
            mount("portal", "/run/user/1000/doc", "fuse.portal"),
            mount("/dev/sdc1", "/mnt/usb", "fuseblk"),
            mount("sshfs", "/mnt/ssh", "fuse"),
            mount("none", "/unknown", "somefs"),
        ];

        assert_eq!(
            mountpoints(&kept_mounts(mounts.into())),
            ["/", "/Applications", "/data", "/home", "/srv"]
        );
    }

    #[test]
    fn boot_container_runtime_and_removable_media_trees_are_dropped() {
        let mounts = [
            mount("rpool/root", "/", "zfs"),
            mount("/dev/nvme0n1p1", "/boot", "vfat"),
            mount("/dev/nvme0n1p2", "/boot/efi", "vfat"),
            mount("/dev/nvme0n1p3", "/bootstrap", "ext4"),
            mount("rpool/docker/3f2a", "/var/lib/docker/zfs/graph/3f2a", "zfs"),
            mount("rpool/docker/9b1c", "/var/lib/docker/zfs/graph/9b1c", "zfs"),
            mount("/dev/sdb1", "/var/lib/docker", "ext4"),
            mount("/dev/sdb2", "/var/lib/containers/storage", "xfs"),
            mount("/dev/sdb3", "/run/containerd/io", "ext4"),
            mount("/dev/sdc1", "/media/backup", "ext4"),
            mount("/dev/sdc2", "/run/media/ap/stick", "vfat"),
            mount("/dev/disk4s1", "/Volumes/External", "apfs"),
            mount("/dev/sdd1", "/var/lib/dockerish", "ext4"),
        ];

        assert_eq!(
            mountpoints(&kept_mounts(mounts.into())),
            ["/", "/bootstrap", "/var/lib/dockerish"]
        );
    }

    #[test]
    fn macos_system_volumes_are_dropped() {
        let mounts = [
            mount("/dev/disk3s1s1", "/", "apfs"),
            mount("devfs", "/dev", "devfs"),
            mount("/dev/disk3s6", "/System/Volumes/VM", "apfs"),
            mount("/dev/disk3s2", "/System/Volumes/Preboot", "apfs"),
            mount("/dev/disk3s4", "/System/Volumes/Update", "apfs"),
            mount("/dev/disk1s2", "/System/Volumes/xarts", "apfs"),
            mount("/dev/disk3s5", "/System/Volumes/Data", "apfs"),
            mount("map auto_home", "/System/Volumes/Data/home", "autofs"),
        ];

        assert_eq!(mountpoints(&kept_mounts(mounts.into())), ["/"]);
    }

    #[test]
    fn apfs_volumes_of_one_container_are_kept_once_under_the_shortest_mount_point() {
        let mounts = [
            mount("/dev/disk3s5", "/opt/data", "apfs"),
            mount("/dev/disk3s1s1", "/", "apfs"),
            mount("/dev/disk5s1", "/opt/models", "apfs"),
            mount("/dev/disk5s2", "/opt/cache", "apfs"),
        ];

        assert_eq!(
            kept_mounts(mounts.into()),
            [
                mount("/dev/disk3s1s1", "/", "apfs"),
                mount("/dev/disk5s2", "/opt/cache", "apfs"),
            ]
        );
    }

    #[test]
    fn datasets_of_one_zfs_pool_are_one_filesystem_under_the_shortest_mount_point() {
        let mounts = vec![
            mount("rpool/home", "/home", "zfs"),
            mount("rpool/root", "/", "zfs"),
            mount("rpool/home", "/srv/home-bind", "zfs"),
            mount("tank/data", "/tank/data", "zfs"),
        ];

        assert_eq!(
            real_filesystems(mounts),
            [
                Filesystem {
                    mount: mount("rpool/root", "/", "zfs"),
                    members: vec![
                        mount("rpool/root", "/", "zfs"),
                        mount("rpool/home", "/home", "zfs"),
                    ],
                },
                Filesystem {
                    mount: mount("tank/data", "/tank/data", "zfs"),
                    members: vec![mount("tank/data", "/tank/data", "zfs")],
                },
            ]
        );
    }

    #[test]
    fn only_a_zfs_pool_reads_several_members() {
        let mounts = vec![
            mount("/dev/disk3s1s1", "/", "apfs"),
            mount("/dev/disk3s5", "/opt/data", "apfs"),
        ];

        assert_eq!(
            real_filesystems(mounts),
            [Filesystem {
                mount: mount("/dev/disk3s1s1", "/", "apfs"),
                members: vec![mount("/dev/disk3s1s1", "/", "apfs")],
            }]
        );
    }

    #[test]
    fn a_storage_pool_is_the_apfs_container_and_otherwise_the_device() {
        let pool =
            |device: &str, fstype: &str| storage_pool(&mount(device, "/data", fstype)).to_owned();

        assert_eq!(pool("/dev/disk3s1s1", "apfs"), "/dev/disk3");
        assert_eq!(pool("/dev/disk3s5", "apfs"), "/dev/disk3");
        assert_eq!(pool("/dev/disk12s2", "apfs"), "/dev/disk12");
        assert_eq!(pool("/dev/disk3", "apfs"), "/dev/disk3");
        assert_eq!(pool("/dev/diskXs1", "apfs"), "/dev/diskXs1");
        assert_eq!(
            pool("snapshot@/dev/disk3s5", "apfs"),
            "snapshot@/dev/disk3s5"
        );
        assert_eq!(pool("rpool/root/nixos", "zfs"), "rpool");
        assert_eq!(pool("tank", "zfs"), "tank");
        assert_eq!(pool("/dev/disk3s5", "hfs"), "/dev/disk3s5");
        assert_eq!(pool("/dev/sda1", "ext4"), "/dev/sda1");
    }

    #[test]
    fn a_device_mounted_at_several_places_is_kept_once_under_its_shortest_mount_point() {
        let mounts = [
            mount("/dev/nvme0n1p2", "/nix/store", "ext4"),
            mount("/dev/nvme0n1p2", "/", "ext4"),
            mount("/dev/nvme0n1p2", "/srv/bind", "ext4"),
            mount("/dev/nvme1n1", "/mnt/b", "ext4"),
            mount("/dev/nvme1n1", "/mnt/a", "ext4"),
        ];

        assert_eq!(
            kept_mounts(mounts.into()),
            [
                mount("/dev/nvme0n1p2", "/", "ext4"),
                mount("/dev/nvme1n1", "/mnt/a", "ext4"),
            ]
        );
    }

    #[test]
    fn only_the_last_mount_at_a_mount_point_is_visible() {
        let mounts = [
            mount("/dev/sda1", "/data", "ext4"),
            mount("/dev/sda1", "/srv/archive", "ext4"),
            mount("/dev/sdb1", "/data", "xfs"),
            mount("/dev/sdc1", "/scratch", "ext4"),
            mount("tmpfs", "/scratch", "tmpfs"),
        ];

        assert_eq!(
            kept_mounts(mounts.into()),
            [
                mount("/dev/sdb1", "/data", "xfs"),
                mount("/dev/sda1", "/srv/archive", "ext4"),
            ]
        );
    }

    #[test]
    fn relocated_and_snapshotter_container_layers_are_dropped() {
        let layer = "3f2a000000000000000000000000000000000000000000000000000000000000";
        let mounts = [
            mount("rpool/root", "/", "zfs"),
            mount(
                &format!("tank/docker/{layer}"),
                &format!("/srv/docker/zfs/graph/{layer}"),
                "zfs",
            ),
            mount(
                &format!("tank/images/{layer}-init"),
                "/tank/images/layer",
                "zfs",
            ),
            mount(&format!("tank/k3s/{layer}"), "/mnt/layer", "zfs"),
            mount("rpool/k3s", "/var/lib/rancher/k3s", "zfs"),
            mount("/dev/sdb1", "/data/containerd/io", "ext4"),
            mount("tank/data", "/tank/data", "zfs"),
            mount("/dev/sdb2", "/srv/dockerish", "ext4"),
        ];

        assert_eq!(
            mountpoints(&kept_mounts(mounts.into())),
            ["/", "/srv/dockerish", "/tank/data"]
        );
        assert!(!is_container_layer_dataset("tank/3f2a"));
        assert!(!is_container_layer_dataset(&format!(
            "tank/{}",
            "g".repeat(64)
        )));
    }

    #[test]
    fn hidden_macos_images_are_dropped() {
        let hidden = |device: &str, mountpoint: &str| Mount {
            browsable: false,
            ..mount(device, mountpoint, "apfs")
        };
        let mounts = vec![
            mount("/dev/disk3s1s1", "/", "apfs"),
            hidden("/dev/disk3s5", "/System/Volumes/Data"),
            hidden(
                "/dev/disk5s1",
                "/Library/Developer/CoreSimulator/Volumes/iOS_22A",
            ),
            hidden(
                "/dev/disk2s1",
                "/private/var/run/com.apple.security.cryptexd/mnt/a",
            ),
        ];

        assert_eq!(mountpoints(&kept_mounts(mounts)), ["/"]);
    }

    #[test]
    fn a_filesystem_no_longer_at_its_mount_point_is_left_out() {
        let known = KnownMount::identify(mount("/dev/root", "/", "ext4")).expect("root device");
        assert_eq!(
            known.read_space().map(|space| space.mountpoint),
            Some("/".to_owned())
        );

        // Another device now answers at the mount point, as the parent does after an unmount.
        let replaced = KnownMount {
            device_id: known.device_id.wrapping_add(1),
            ..known
        };
        assert_eq!(replaced.read_space(), None);
        assert_eq!(
            KnownMount::identify(mount("/dev/sdz9", "/dotcore-test-missing-mount", "ext4")),
            None
        );
    }

    #[test]
    fn a_pool_adds_used_space_and_counts_its_shared_free_space_once() {
        let root = mount("rpool/root", "/", "zfs");

        assert_eq!(
            combined_space(
                &root,
                [space("/", 600, 100, 500), space("/home", 700, 200, 500)].into_iter()
            ),
            Some(space("/", 800, 300, 500))
        );
    }

    #[test]
    fn a_quota_on_one_dataset_leaves_the_pool_free_space() {
        let root = mount("rpool/root", "/", "zfs");

        assert_eq!(
            combined_space(
                &root,
                [space("/home", 150, 100, 50), space("/data", 700, 200, 500)].into_iter()
            ),
            Some(space("/", 800, 300, 500))
        );
    }

    #[test]
    fn one_member_keeps_its_space_under_the_reported_mount_point() {
        let root = mount("rpool/root", "/", "zfs");

        assert_eq!(
            combined_space(&root, [space("/home", 700, 200, 400)].into_iter()),
            Some(space("/", 700, 200, 400))
        );
        assert_eq!(combined_space(&root, [].into_iter()), None);
    }

    #[test]
    fn an_unreadable_member_is_left_out_until_no_member_remains() {
        let root = KnownMount::identify(mount("/dev/root", "/", "ext4")).expect("root device");
        let unmounted = KnownMount {
            device_id: root.device_id.wrapping_add(1),
            ..root.clone()
        };
        let filesystem = |members: Vec<KnownMount>| KnownFilesystem {
            mount: root.mount.clone(),
            members,
        };

        let alone = filesystem(vec![root.clone()]).read_space().expect("root");
        let with_unmounted = filesystem(vec![unmounted.clone(), root.clone()]).read_space();
        assert_eq!(
            with_unmounted.map(|space| space.mountpoint),
            Some(alone.mountpoint)
        );
        assert_eq!(filesystem(vec![unmounted]).read_space(), None);
        assert_eq!(
            KnownFilesystem::identify(Filesystem {
                mount: mount("/dev/sdz9", "/dotcore-test-missing-mount", "ext4"),
                members: vec![mount("/dev/sdz9", "/dotcore-test-missing-mount", "ext4")],
            }),
            None
        );
    }

    #[test]
    fn the_kernel_mount_list_is_parsed_with_its_octal_escapes() {
        let text = "/dev/nvme0n1p2 / ext4 rw,relatime 0 0\n\
            /dev/sda1 /mnt/my\\040disk xfs rw 0 0\n\
            tmpfs /run tmpfs rw,nosuid 0 0\n\
            truncated\n";

        assert_eq!(
            parse_mounts(text),
            [
                mount("/dev/nvme0n1p2", "/", "ext4"),
                mount("/dev/sda1", "/mnt/my disk", "xfs"),
                mount("tmpfs", "/run", "tmpfs"),
            ]
        );
        assert_eq!(unescape_mount_field("a\\134b\\0"), "a\\b\\0");
    }

    #[test]
    fn one_statistics_read_reports_used_and_available_within_the_size() {
        let space = read_space(&mount("/dev/root", "/", "ext4")).expect("root statistics");

        assert_eq!(space.mountpoint, "/");
        assert_eq!(space.fstype, "ext4");
        assert!(space.size_bytes > 0);
        assert!(
            space.used_bytes + space.available_bytes <= space.size_bytes,
            "{space:?}"
        );
    }

    #[test]
    fn a_missing_mount_point_has_no_statistics() {
        assert_eq!(
            read_space(&mount("/dev/sdz9", "/dotcore-test-missing-mount", "ext4")),
            None
        );
    }

    fn mount(device: &str, mountpoint: &str, fstype: &str) -> Mount {
        Mount {
            mountpoint: mountpoint.to_owned(),
            fstype: fstype.to_owned(),
            device: device.to_owned(),
            browsable: true,
        }
    }

    fn space(mountpoint: &str, size: u64, used: u64, available: u64) -> FilesystemSample {
        FilesystemSample {
            mountpoint: mountpoint.to_owned(),
            fstype: "zfs".to_owned(),
            size_bytes: size,
            used_bytes: used,
            available_bytes: available,
        }
    }

    fn kept_mounts(mounts: Vec<Mount>) -> Vec<Mount> {
        real_filesystems(mounts)
            .into_iter()
            .map(|filesystem| filesystem.mount)
            .collect()
    }

    fn mountpoints(mounts: &[Mount]) -> Vec<&str> {
        mounts
            .iter()
            .map(|mount| mount.mountpoint.as_str())
            .collect()
    }
}
