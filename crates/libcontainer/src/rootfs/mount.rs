#[cfg(feature = "v1")]
use super::symlink::Symlink;
use super::utils::{find_parent_mount, parse_mount, MountOptionConfig};
use crate::{
    syscall::{linux, syscall::create_syscall, Syscall, SyscallError},
    utils,
    utils::PathBufExt,
};
#[cfg(feature = "v2")]
use anyhow::anyhow;
use anyhow::{bail, Context, Result};
use libcgroups::common::CgroupSetup::{Hybrid, Legacy, Unified};
#[cfg(feature = "v1")]
use libcgroups::common::DEFAULT_CGROUP_ROOT;
use nix::{dir::Dir, errno::Errno, fcntl::OFlag, mount::MsFlags, sys::stat::Mode};
use oci_spec::runtime::{Mount as SpecMount, MountBuilder as SpecMountBuilder};
use procfs::process::{MountInfo, MountOptFields, Process};
use std::fs::{canonicalize, create_dir_all, OpenOptions};
use std::mem;
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};

#[cfg(feature = "v1")]
use std::{borrow::Cow, collections::HashMap};

#[derive(Debug)]
pub struct MountOptions<'a> {
    pub root: &'a Path,
    pub label: Option<&'a str>,
    pub cgroup_ns: bool,
}

pub struct Mount {
    syscall: Box<dyn Syscall>,
}

impl Default for Mount {
    fn default() -> Self {
        Self::new()
    }
}

impl Mount {
    pub fn new() -> Mount {
        Mount {
            syscall: create_syscall(),
        }
    }

    pub fn setup_mount(&self, mount: &SpecMount, options: &MountOptions) -> Result<()> {
        log::debug!("mounting {:?}", mount);
        let mut mount_option_config = parse_mount(mount);

        match mount.typ().as_deref() {
            Some("cgroup") => {
                match libcgroups::common::get_cgroup_setup()
                    .context("failed to determine cgroup setup")?
                {
                    Legacy | Hybrid => {
                        #[cfg(not(feature = "v1"))]
                        panic!("libcontainer can't run in a Legacy or Hybrid cgroup setup without the v1 feature");
                        #[cfg(feature = "v1")]
                        self.mount_cgroup_v1(mount, options)
                            .context("failed to mount cgroup v1")?
                    }
                    Unified => {
                        #[cfg(not(feature = "v2"))]
                        panic!("libcontainer can't run in a Unified cgroup setup without the v2 feature");
                        #[cfg(feature = "v2")]
                        self.mount_cgroup_v2(mount, options, &mount_option_config)
                            .context("failed to mount cgroup v2")?
                    }
                }
            }
            _ => {
                if *mount.destination() == PathBuf::from("/dev") {
                    mount_option_config.flags &= !MsFlags::MS_RDONLY;
                    self.mount_into_container(
                        mount,
                        options.root,
                        &mount_option_config,
                        options.label,
                    )
                    .with_context(|| format!("failed to mount /dev: {mount:?}"))?;
                } else {
                    self.mount_into_container(
                        mount,
                        options.root,
                        &mount_option_config,
                        options.label,
                    )
                    .with_context(|| format!("failed to mount: {mount:?}"))?;
                }
            }
        }

        Ok(())
    }

    #[cfg(feature = "v1")]
    fn mount_cgroup_v1(&self, cgroup_mount: &SpecMount, options: &MountOptions) -> Result<()> {
        log::debug!("Mounting cgroup v1 filesystem");
        // create tmpfs into which the cgroup subsystems will be mounted
        let tmpfs = SpecMountBuilder::default()
            .source("tmpfs")
            .typ("tmpfs")
            .destination(cgroup_mount.destination())
            .options(
                ["noexec", "nosuid", "nodev", "mode=755"]
                    .iter()
                    .map(|o| o.to_string())
                    .collect::<Vec<String>>(),
            )
            .build()
            .context("failed to build tmpfs for cgroup")?;

        self.setup_mount(&tmpfs, options)
            .context("failed to mount tmpfs for cgroup")?;

        // get all cgroup mounts on the host system
        let host_mounts: Vec<PathBuf> = libcgroups::v1::util::list_subsystem_mount_points()
            .context("failed to get subsystem mount points")?
            .into_iter()
            .filter(|p| p.as_path().starts_with(DEFAULT_CGROUP_ROOT))
            .collect();
        log::debug!("cgroup mounts: {:?}", host_mounts);

        // get process cgroups
        let process_cgroups: HashMap<String, String> = Process::myself()?
            .cgroups()
            .context("failed to get process cgroups")?
            .into_iter()
            .map(|c| (c.controllers.join(","), c.pathname))
            .collect();
        log::debug!("Process cgroups: {:?}", process_cgroups);

        let cgroup_root = options
            .root
            .join_safely(cgroup_mount.destination())
            .context("could not join rootfs path with cgroup mount destination")?;
        log::debug!("cgroup root: {:?}", cgroup_root);

        let symlink = Symlink::new();

        // setup cgroup mounts for container
        for host_mount in &host_mounts {
            if let Some(subsystem_name) = host_mount.file_name().and_then(|n| n.to_str()) {
                if options.cgroup_ns {
                    self.setup_namespaced_subsystem(
                        cgroup_mount,
                        options,
                        subsystem_name,
                        subsystem_name == "systemd",
                    )?;
                } else {
                    self.setup_emulated_subsystem(
                        cgroup_mount,
                        options,
                        subsystem_name,
                        subsystem_name == "systemd",
                        host_mount,
                        &process_cgroups,
                    )?;
                }

                symlink.setup_comount_symlinks(&cgroup_root, subsystem_name)?;
            } else {
                log::warn!("could not get subsystem name from {:?}", host_mount);
            }
        }

        Ok(())
    }

    // On some distros cgroup subsystems are comounted e.g. cpu,cpuacct or net_cls,net_prio. These systems
    // have to be comounted in the container as well as the kernel will reject trying to mount them separately.
    #[cfg(feature = "v1")]
    fn setup_namespaced_subsystem(
        &self,
        cgroup_mount: &SpecMount,
        options: &MountOptions,
        subsystem_name: &str,
        named: bool,
    ) -> Result<()> {
        log::debug!(
            "Mounting (namespaced) {:?} cgroup subsystem",
            subsystem_name
        );
        let subsystem_mount = SpecMountBuilder::default()
            .source("cgroup")
            .typ("cgroup")
            .destination(cgroup_mount.destination().join(subsystem_name))
            .options(
                ["noexec", "nosuid", "nodev"]
                    .iter()
                    .map(|o| o.to_string())
                    .collect::<Vec<String>>(),
            )
            .build()
            .with_context(|| format!("failed to build {subsystem_name}"))?;

        let data: Cow<str> = if named {
            format!("name={subsystem_name}").into()
        } else {
            subsystem_name.into()
        };

        let mount_options_config = MountOptionConfig {
            flags: MsFlags::MS_NOEXEC | MsFlags::MS_NOSUID | MsFlags::MS_NODEV,
            data: data.to_string(),
            rec_attr: None,
        };

        self.mount_into_container(
            &subsystem_mount,
            options.root,
            &mount_options_config,
            options.label,
        )
        .with_context(|| format!("failed to mount {subsystem_mount:?}"))
    }

    #[cfg(feature = "v1")]
    fn setup_emulated_subsystem(
        &self,
        cgroup_mount: &SpecMount,
        options: &MountOptions,
        subsystem_name: &str,
        named: bool,
        host_mount: &Path,
        process_cgroups: &HashMap<String, String>,
    ) -> Result<()> {
        log::debug!("Mounting (emulated) {:?} cgroup subsystem", subsystem_name);
        let named_hierarchy: Cow<str> = if named {
            format!("name={subsystem_name}").into()
        } else {
            subsystem_name.into()
        };

        if let Some(proc_path) = process_cgroups.get(named_hierarchy.as_ref()) {
            let emulated = SpecMountBuilder::default()
                .source(
                    host_mount
                        .join_safely(proc_path.as_str())
                        .with_context(|| {
                            format!("failed to join mount source for {subsystem_name} subsystem")
                        })?,
                )
                .destination(
                    cgroup_mount
                        .destination()
                        .join_safely(subsystem_name)
                        .with_context(|| {
                            format!(
                                "failed to join mount destination for {subsystem_name} subsystem"
                            )
                        })?,
                )
                .typ("bind")
                .options(
                    ["rw", "rbind"]
                        .iter()
                        .map(|o| o.to_string())
                        .collect::<Vec<String>>(),
                )
                .build()?;
            log::debug!("Mounting emulated cgroup subsystem: {:?}", emulated);

            self.setup_mount(&emulated, options)
                .with_context(|| format!("failed to mount {subsystem_name} cgroup hierarchy"))?;
        } else {
            log::warn!("Could not mount {:?} cgroup subsystem", subsystem_name);
        }

        Ok(())
    }

    #[cfg(feature = "v2")]
    fn mount_cgroup_v2(
        &self,
        cgroup_mount: &SpecMount,
        options: &MountOptions,
        mount_option_config: &MountOptionConfig,
    ) -> Result<()> {
        log::debug!("Mounting cgroup v2 filesystem");

        let cgroup_mount = SpecMountBuilder::default()
            .typ("cgroup2")
            .source("cgroup")
            .destination(cgroup_mount.destination())
            .options(Vec::new())
            .build()?;
        log::debug!("{:?}", cgroup_mount);

        if self
            .mount_into_container(
                &cgroup_mount,
                options.root,
                mount_option_config,
                options.label,
            )
            .context("failed to mount into container")
            .is_err()
        {
            let host_mount = libcgroups::v2::util::get_unified_mount_point()
                .context("failed to get unified mount point")?;

            let process_cgroup = Process::myself()?
                .cgroups()
                .context("failed to get process cgroups")?
                .into_iter()
                .find(|c| c.hierarchy == 0)
                .map(|c| PathBuf::from(c.pathname))
                .ok_or_else(|| anyhow!("failed to find unified process cgroup"))?;

            let bind_mount = SpecMountBuilder::default()
                .typ("bind")
                .source(host_mount.join_safely(process_cgroup)?)
                .destination(cgroup_mount.destination())
                .options(Vec::new())
                .build()
                .context("failed to build cgroup bind mount")?;
            log::debug!("{:?}", bind_mount);

            let mut mount_option_config = (*mount_option_config).clone();
            mount_option_config.flags |= MsFlags::MS_BIND;
            self.mount_into_container(
                &bind_mount,
                options.root,
                &mount_option_config,
                options.label,
            )
            .context("failed to bind mount cgroup hierarchy")?;
        }

        Ok(())
    }

    /// Make parent mount of rootfs private if it was shared, which is required by pivot_root.
    /// It also makes sure following bind mount does not propagate in other namespaces.
    pub fn make_parent_mount_private(&self, rootfs: &Path) -> Result<Option<MountInfo>> {
        let mount_infos = Process::myself()?.mountinfo()?;
        let parent_mount = find_parent_mount(rootfs, mount_infos)?;

        // check parent mount has 'shared' propagation type
        if parent_mount
            .opt_fields
            .iter()
            .any(|field| matches!(field, MountOptFields::Shared(_)))
        {
            self.syscall.mount(
                None,
                &parent_mount.mount_point,
                None,
                MsFlags::MS_PRIVATE,
                None,
            )?;
            Ok(Some(parent_mount))
        } else {
            Ok(None)
        }
    }

    fn mount_into_container(
        &self,
        m: &SpecMount,
        rootfs: &Path,
        mount_option_config: &MountOptionConfig,
        label: Option<&str>,
    ) -> Result<()> {
        let typ = m.typ().as_deref();
        let mut d = mount_option_config.data.to_string();

        if let Some(l) = label {
            if typ != Some("proc") && typ != Some("sysfs") {
                match mount_option_config.data.is_empty() {
                    true => d = format!("context=\"{l}\""),
                    false => d = format!("{},context=\"{}\"", mount_option_config.data, l),
                }
            }
        }

        let dest_for_host = utils::secure_join(rootfs, m.destination())
            .with_context(|| format!("failed to join {:?} with {:?}", rootfs, m.destination()))?;

        let dest = Path::new(&dest_for_host);
        let source = m
            .source()
            .as_ref()
            .with_context(|| "no source in mount spec".to_string())?;
        let src = if typ == Some("bind") {
            let src = canonicalize(source)
                .with_context(|| format!("failed to canonicalize: {source:?}"))?;
            let dir = if src.is_file() {
                Path::new(&dest).parent().unwrap()
            } else {
                Path::new(&dest)
            };

            create_dir_all(dir)
                .with_context(|| format!("failed to create dir for bind mount: {dir:?}"))?;

            if src.is_file() {
                OpenOptions::new()
                    .create(true)
                    .write(true)
                    .open(dest)
                    .with_context(|| format!("failed to create file for bind mount: {src:?}"))?;
            }

            src
        } else {
            create_dir_all(dest).with_context(|| format!("Failed to create device: {dest:?}"))?;

            PathBuf::from(source)
        };

        if let Err(err) =
            self.syscall
                .mount(Some(&*src), dest, typ, mount_option_config.flags, Some(&*d))
        {
            if let SyscallError::Mount { source: errno, .. } = err {
                if !matches!(errno, Errno::EINVAL) {
                    bail!("mount of {:?} failed. {}", m.destination(), errno);
                }
            }

            self.syscall
                .mount(
                    Some(&*src),
                    dest,
                    typ,
                    mount_option_config.flags,
                    Some(&mount_option_config.data),
                )
                .with_context(|| format!("failed to mount {src:?} to {dest:?}"))?;
        }

        if typ == Some("bind")
            && mount_option_config.flags.intersects(
                !(MsFlags::MS_REC
                    | MsFlags::MS_REMOUNT
                    | MsFlags::MS_BIND
                    | MsFlags::MS_PRIVATE
                    | MsFlags::MS_SHARED
                    | MsFlags::MS_SLAVE),
            )
        {
            self.syscall
                .mount(
                    Some(dest),
                    dest,
                    None,
                    mount_option_config.flags | MsFlags::MS_REMOUNT,
                    None,
                )
                .with_context(|| format!("Failed to remount: {dest:?}"))?;
        }

        if let Some(mount_attr) = &mount_option_config.rec_attr {
            let open_dir = Dir::open(dest, OFlag::O_DIRECTORY, Mode::empty())?;
            let dir_fd_pathbuf = PathBuf::from(format!("/proc/self/fd/{}", open_dir.as_raw_fd()));
            self.syscall.mount_setattr(
                -1,
                &dir_fd_pathbuf,
                linux::AT_RECURSIVE,
                mount_attr,
                mem::size_of::<linux::MountAttr>(),
            )?;
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    #[cfg(feature = "v1")]
    use std::fs;

    use super::*;
    use crate::syscall::test::{MountArgs, TestHelperSyscall};
    use anyhow::Result;

    #[test]
    fn test_mount_to_container() {
        let tmp_dir = tempfile::tempdir().unwrap();
        {
            let m = Mount::new();
            let mount = &SpecMountBuilder::default()
                .destination(PathBuf::from("/dev/pts"))
                .typ("devpts")
                .source(PathBuf::from("devpts"))
                .options(vec![
                    "nosuid".to_string(),
                    "noexec".to_string(),
                    "newinstance".to_string(),
                    "ptmxmode=0666".to_string(),
                    "mode=0620".to_string(),
                    "gid=5".to_string(),
                ])
                .build()
                .unwrap();
            let mount_option_config = parse_mount(mount);

            assert!(m
                .mount_into_container(
                    mount,
                    tmp_dir.path(),
                    &mount_option_config,
                    Some("defaults")
                )
                .is_ok());

            let want = vec![MountArgs {
                source: Some(PathBuf::from("devpts")),
                target: tmp_dir.path().join("dev/pts"),
                fstype: Some("devpts".to_string()),
                flags: MsFlags::MS_NOSUID | MsFlags::MS_NOEXEC,
                data: Some(
                    "newinstance,ptmxmode=0666,mode=0620,gid=5,context=\"defaults\"".to_string(),
                ),
            }];
            let got = &m
                .syscall
                .as_any()
                .downcast_ref::<TestHelperSyscall>()
                .unwrap()
                .get_mount_args();
            assert_eq!(want, *got);
            assert_eq!(got.len(), 1);
        }
        {
            let m = Mount::new();
            let mount = &SpecMountBuilder::default()
                .destination(PathBuf::from("/dev/null"))
                .typ("bind")
                .source(tmp_dir.path().join("null"))
                .options(vec!["ro".to_string()])
                .build()
                .unwrap();
            let mount_option_config = parse_mount(mount);
            OpenOptions::new()
                .create(true)
                .write(true)
                .open(tmp_dir.path().join("null"))
                .unwrap();

            assert!(m
                .mount_into_container(mount, tmp_dir.path(), &mount_option_config, None)
                .is_ok());

            let want = vec![
                MountArgs {
                    source: Some(tmp_dir.path().join("null")),
                    target: tmp_dir.path().join("dev/null"),
                    fstype: Some("bind".to_string()),
                    flags: MsFlags::MS_RDONLY,
                    data: Some("".to_string()),
                },
                // remount one
                MountArgs {
                    source: Some(tmp_dir.path().join("dev/null")),
                    target: tmp_dir.path().join("dev/null"),
                    fstype: None,
                    flags: MsFlags::MS_RDONLY | MsFlags::MS_REMOUNT,
                    data: None,
                },
            ];
            let got = &m
                .syscall
                .as_any()
                .downcast_ref::<TestHelperSyscall>()
                .unwrap()
                .get_mount_args();
            assert_eq!(want, *got);
            assert_eq!(got.len(), 2);
        }
    }

    #[test]
    fn test_make_parent_mount_private() {
        let tmp_dir = tempfile::tempdir().unwrap();
        let m = Mount::new();
        let result = m.make_parent_mount_private(tmp_dir.path());
        assert!(result.is_ok());

        if let Ok(Some(_)) = result {
            let set = m
                .syscall
                .as_any()
                .downcast_ref::<TestHelperSyscall>()
                .unwrap()
                .get_mount_args();

            assert_eq!(set.len(), 1);

            let got = &set[0];
            assert_eq!(got.source, None);
            assert_eq!(got.fstype, None);
            assert_eq!(got.flags, MsFlags::MS_PRIVATE);
            assert_eq!(got.data, None);

            // This can be either depending on the system, some systems mount tmpfs at /tmp others it's
            // a plain directory. See https://github.com/containers/youki/issues/471
            assert!(got.target == PathBuf::from("/") || got.target == PathBuf::from("/tmp"));
        }
    }

    #[test]
    #[cfg(feature = "v1")]
    fn test_namespaced_subsystem_success() -> Result<()> {
        let tmp = tempfile::tempdir().unwrap();
        let container_cgroup = Path::new("/container_cgroup");

        let mounter = Mount::new();

        let spec_cgroup_mount = SpecMountBuilder::default()
            .destination(container_cgroup)
            .source("cgroup")
            .typ("cgroup")
            .build()
            .context("failed to build cgroup mount")?;

        let mount_opts = MountOptions {
            root: tmp.path(),
            label: None,
            cgroup_ns: true,
        };

        let subsystem_name = "cpu";

        mounter
            .setup_namespaced_subsystem(&spec_cgroup_mount, &mount_opts, subsystem_name, false)
            .context("failed to setup namespaced subsystem")?;

        let expected = MountArgs {
            source: Some(PathBuf::from("cgroup")),
            target: tmp
                .path()
                .join_safely(container_cgroup)?
                .join(subsystem_name),
            fstype: Some("cgroup".to_owned()),
            flags: MsFlags::MS_NOEXEC | MsFlags::MS_NOSUID | MsFlags::MS_NODEV,
            data: Some("cpu".to_owned()),
        };

        let got = mounter
            .syscall
            .as_any()
            .downcast_ref::<TestHelperSyscall>()
            .unwrap()
            .get_mount_args();

        assert_eq!(got.len(), 1);
        assert_eq!(expected, got[0]);

        Ok(())
    }

    #[test]
    #[cfg(feature = "v1")]
    fn test_emulated_subsystem_success() -> Result<()> {
        // arrange
        let tmp = tempfile::tempdir().unwrap();
        let host_cgroup_mount = tmp.path().join("host_cgroup");
        let host_cgroup = host_cgroup_mount.join("cpu/container1");
        fs::create_dir_all(&host_cgroup)?;

        let container_cgroup = Path::new("/container_cgroup");
        let mounter = Mount::new();

        let spec_cgroup_mount = SpecMountBuilder::default()
            .destination(container_cgroup)
            .source("cgroup")
            .typ("cgroup")
            .build()
            .context("failed to build cgroup mount")?;

        let mount_opts = MountOptions {
            root: tmp.path(),
            label: None,
            cgroup_ns: false,
        };

        let subsystem_name = "cpu";
        let mut process_cgroups = HashMap::new();
        process_cgroups.insert("cpu".to_owned(), "container1".to_owned());

        // act
        mounter
            .setup_emulated_subsystem(
                &spec_cgroup_mount,
                &mount_opts,
                subsystem_name,
                false,
                &host_cgroup_mount.join(subsystem_name),
                &process_cgroups,
            )
            .context("failed to setup emulated subsystem")?;

        // assert
        let expected = MountArgs {
            source: Some(host_cgroup),
            target: tmp
                .path()
                .join_safely(container_cgroup)?
                .join(subsystem_name),
            fstype: Some("bind".to_owned()),
            flags: MsFlags::MS_BIND | MsFlags::MS_REC,
            data: Some("".to_owned()),
        };

        let got = mounter
            .syscall
            .as_any()
            .downcast_ref::<TestHelperSyscall>()
            .unwrap()
            .get_mount_args();

        assert_eq!(got.len(), 1);
        assert_eq!(expected, got[0]);

        Ok(())
    }

    #[test]
    #[cfg(feature = "v1")]
    fn test_mount_cgroup_v1() -> Result<()> {
        // arrange
        let tmp = tempfile::tempdir().unwrap();
        let container_cgroup = PathBuf::from("/sys/fs/cgroup");

        let spec_cgroup_mount = SpecMountBuilder::default()
            .destination(&container_cgroup)
            .source("cgroup")
            .typ("cgroup")
            .build()
            .context("failed to build cgroup mount")?;

        let mount_opts = MountOptions {
            root: tmp.path(),
            label: None,
            cgroup_ns: true,
        };

        let mounter = Mount::new();

        // act
        mounter
            .mount_cgroup_v1(&spec_cgroup_mount, &mount_opts)
            .context("failed to mount cgroup v1")?;

        // assert
        let mut got = mounter
            .syscall
            .as_any()
            .downcast_ref::<TestHelperSyscall>()
            .unwrap()
            .get_mount_args()
            .into_iter();

        let host_mounts = libcgroups::v1::util::list_subsystem_mount_points()?;
        assert_eq!(got.len(), host_mounts.len() + 1);

        let expected = MountArgs {
            source: Some(PathBuf::from("tmpfs".to_owned())),
            target: tmp.path().join_safely(&container_cgroup)?,
            fstype: Some("tmpfs".to_owned()),
            flags: MsFlags::MS_NOEXEC | MsFlags::MS_NOSUID | MsFlags::MS_NODEV,
            data: Some("mode=755".to_owned()),
        };
        assert_eq!(expected, got.next().unwrap());

        for (host_mount, act) in host_mounts.iter().zip(got) {
            let subsystem_name = host_mount.file_name().and_then(|f| f.to_str()).unwrap();
            let expected = MountArgs {
                source: Some(PathBuf::from("cgroup".to_owned())),
                target: tmp
                    .path()
                    .join_safely(&container_cgroup)?
                    .join(subsystem_name),
                fstype: Some("cgroup".to_owned()),
                flags: MsFlags::MS_NOEXEC | MsFlags::MS_NOSUID | MsFlags::MS_NODEV,
                data: Some(
                    if subsystem_name == "systemd" {
                        format!("name={subsystem_name}")
                    } else {
                        subsystem_name.to_string()
                    }
                    .to_owned(),
                ),
            };
            assert_eq!(expected, act);
        }

        Ok(())
    }

    #[test]
    #[cfg(feature = "v2")]
    fn test_mount_cgroup_v2() -> Result<()> {
        // arrange
        let tmp = tempfile::tempdir().unwrap();
        let container_cgroup = PathBuf::from("/sys/fs/cgroup");

        let spec_cgroup_mount = SpecMountBuilder::default()
            .destination(&container_cgroup)
            .source("cgroup")
            .typ("cgroup")
            .build()
            .context("failed to build cgroup mount")?;

        let mount_opts = MountOptions {
            root: tmp.path(),
            label: None,
            cgroup_ns: true,
        };

        let mounter = Mount::new();
        let flags = MsFlags::MS_NOEXEC | MsFlags::MS_NOSUID | MsFlags::MS_NODEV;

        // act
        let mount_option_config = MountOptionConfig {
            flags,
            data: String::new(),
            rec_attr: None,
        };
        mounter
            .mount_cgroup_v2(&spec_cgroup_mount, &mount_opts, &mount_option_config)
            .context("failed to mount cgroup v2")?;

        // assert
        let expected = MountArgs {
            source: Some(PathBuf::from("cgroup".to_owned())),
            target: tmp.path().join_safely(container_cgroup)?,
            fstype: Some("cgroup2".to_owned()),
            flags: MsFlags::MS_NOEXEC | MsFlags::MS_NOSUID | MsFlags::MS_NODEV,
            data: Some("".to_owned()),
        };

        let got = mounter
            .syscall
            .as_any()
            .downcast_ref::<TestHelperSyscall>()
            .unwrap()
            .get_mount_args();

        assert_eq!(got.len(), 1);
        assert_eq!(expected, got[0]);

        Ok(())
    }
}
