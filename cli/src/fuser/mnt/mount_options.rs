use std::io;
use std::io::ErrorKind;

/// Mount options accepted by the FUSE filesystem type
/// See 'man mount.fuse' for details.
#[derive(Debug, Eq, PartialEq, Hash, Clone)]
pub enum MountOption {
    /// Set the name of the source in mtab
    FSName(String),

    /// Allow all users to access files on this filesystem. By default access is restricted to the
    /// user who mounted it
    AllowOther,
    /// Allow the root user to access this filesystem, in addition to the user who mounted it
    AllowRoot,
    /// Automatically unmount when the mounting process exits
    ///
    /// `AutoUnmount` requires `AllowOther` or `AllowRoot`. If `AutoUnmount` is set and neither `Allow...` is set, the FUSE configuration must permit `allow_other`, otherwise mounting will fail.
    AutoUnmount,
    /// Enable permission checking in the kernel
    DefaultPermissions,
}

pub fn check_option_conflicts(options: &[MountOption]) -> Result<(), io::Error> {
    if options.contains(&MountOption::AllowOther) && options.contains(&MountOption::AllowRoot) {
        Err(io::Error::new(
            ErrorKind::InvalidInput,
            "Conflicting mount options found: AllowOther and AllowRoot",
        ))
    } else {
        Ok(())
    }
}

// Format option to be passed to libfuse or kernel
pub fn option_to_string(option: &MountOption) -> String {
    match option {
        MountOption::FSName(name) => format!("fsname={name}"),
        MountOption::AutoUnmount => "auto_unmount".to_string(),
        MountOption::AllowRoot |
        // AllowRoot is implemented by allowing everyone access and then restricting to
        // root + owner within fuser
        MountOption::AllowOther => "allow_other".to_string(),
        MountOption::DefaultPermissions => "default_permissions".to_string(),
    }
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn option_checking() {
        assert!(
            check_option_conflicts(&[MountOption::AllowOther, MountOption::AllowRoot]).is_err()
        );
        assert!(check_option_conflicts(&[
            MountOption::FSName("agentfs".to_owned()),
            MountOption::DefaultPermissions,
        ])
        .is_ok());
    }

    #[test]
    fn option_strings() {
        use super::MountOption::*;

        assert_eq!(
            option_to_string(&FSName("agentfs".to_owned())),
            "fsname=agentfs"
        );
        assert_eq!(option_to_string(&AllowOther), "allow_other");
        assert_eq!(option_to_string(&AllowRoot), "allow_other");
        assert_eq!(option_to_string(&AutoUnmount), "auto_unmount");
        assert_eq!(option_to_string(&DefaultPermissions), "default_permissions");
    }
}
