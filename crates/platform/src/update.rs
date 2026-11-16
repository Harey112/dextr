//! Installing a downloaded update in place: on macOS the .app from a DMG;
//! on Linux the .rpm or .deb through the system's package tools, or the
//! AppImage file itself. Other installs (from source, the tarball) are
//! updated by hand.

use std::path::{Path, PathBuf};

/// The end of the release file name this copy updates from, if it can
/// update itself.
pub fn package_suffix() -> Option<String> {
    #[cfg(target_os = "macos")]
    {
        Some("-macos.dmg".into())
    }
    #[cfg(target_os = "linux")]
    {
        linux::kind().map(|k| k.suffix())
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        None
    }
}

/// Whether this copy can update itself here and now.
pub fn can_install() -> bool {
    #[cfg(target_os = "macos")]
    {
        macos::bundle().is_some_and(|b| b.parent().is_some_and(writable))
    }
    #[cfg(target_os = "linux")]
    {
        linux::kind().is_some_and(|k| k.can_install())
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        false
    }
}

#[cfg_attr(not(any(target_os = "macos", target_os = "linux")), allow(dead_code))]
fn writable(dir: &Path) -> bool {
    let probe = dir.join(format!(".dextr-write-test-{}", std::process::id()));
    let ok = std::fs::write(&probe, b"").is_ok();
    let _ = std::fs::remove_file(&probe);
    ok
}

/// Starts `program` once this process has exited.
#[cfg(unix)]
#[cfg_attr(not(any(target_os = "macos", target_os = "linux")), allow(dead_code))]
fn start_after_exit(program: &str, target: &Path) -> anyhow::Result<()> {
    use anyhow::Context as _;
    let script = format!("while kill -0 {} 2>/dev/null; do sleep 0.2; done; exec {program} \"$0\"", std::process::id());
    std::process::Command::new("/bin/sh")
        .arg("-c")
        .arg(script)
        .arg(target)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .context("scheduling the restart")?;
    Ok(())
}

/// Replaces the running app with the one in `package` and starts it once
/// this process exits. The caller should quit right after.
pub fn install(package: &Path) -> anyhow::Result<()> {
    #[cfg(target_os = "macos")]
    {
        macos::install(package)
    }
    #[cfg(target_os = "linux")]
    {
        linux::install(package)
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        let _ = package;
        anyhow::bail!("updates are installed by hand on this system")
    }
}

/// Where downloads go before they're installed.
pub fn download_dir() -> PathBuf {
    std::env::temp_dir().join("dextr-update")
}

#[cfg(target_os = "linux")]
mod linux {
    use std::{
        os::unix::fs::PermissionsExt as _,
        path::{Path, PathBuf},
        process::Command,
    };

    use anyhow::{Context as _, bail};

    const INSTALLED: &str = "/usr/bin/dextr";

    /// How this copy was installed.
    pub enum Kind {
        AppImage(PathBuf),
        Rpm,
        Deb,
    }

    fn succeeds(cmd: &mut Command) -> bool {
        cmd.stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).status().is_ok_and(|s| s.success())
    }

    /// Worked out once: it can't change while running.
    pub fn kind() -> Option<&'static Kind> {
        static KIND: std::sync::OnceLock<Option<Kind>> = std::sync::OnceLock::new();
        KIND.get_or_init(detect).as_ref()
    }

    fn detect() -> Option<Kind> {
        if let Some(image) = std::env::var_os("APPIMAGE") {
            return Some(Kind::AppImage(image.into()));
        }
        let exe = std::env::current_exe().ok()?.canonicalize().ok()?;
        if exe != Path::new(INSTALLED) {
            return None;
        }
        if succeeds(Command::new("rpm").args(["-qf", INSTALLED])) {
            Some(Kind::Rpm)
        } else if succeeds(Command::new("dpkg").args(["-S", INSTALLED])) {
            Some(Kind::Deb)
        } else {
            None
        }
    }

    fn pkexec() -> bool {
        ["/usr/bin/pkexec", "/bin/pkexec"].iter().any(|p| Path::new(p).exists())
    }

    impl Kind {
        pub fn suffix(&self) -> String {
            let arch = std::env::consts::ARCH;
            match self {
                Kind::AppImage(_) => format!("-{arch}.AppImage"),
                Kind::Rpm => format!(".{arch}.rpm"),
                Kind::Deb => format!("_{}.deb", if arch == "aarch64" { "arm64" } else { "amd64" }),
            }
        }

        pub fn can_install(&self) -> bool {
            match self {
                Kind::AppImage(path) => path.parent().is_some_and(super::writable),
                Kind::Rpm | Kind::Deb => pkexec(),
            }
        }
    }

    pub fn install(package: &Path) -> anyhow::Result<()> {
        match kind().context("this copy of dextr can't update itself")? {
            Kind::AppImage(image) => {
                // Next to the old file, then swap: the running copy keeps
                // its mounted image until it quits.
                let staged = image.with_file_name(".dextr-update.AppImage");
                std::fs::copy(package, &staged).context("copying the update")?;
                std::fs::set_permissions(&staged, std::fs::Permissions::from_mode(0o755))?;
                std::fs::rename(&staged, image).context("replacing the AppImage")?;
                let _ = std::fs::remove_file(package);
                super::start_after_exit("", image)
            }
            kind => {
                // The system asks for the administrator password.
                let mut cmd = Command::new("pkexec");
                match kind {
                    Kind::Rpm => cmd.args(["rpm", "-U"]),
                    _ => cmd.args(["dpkg", "-i"]),
                };
                let out = cmd.arg(package).output().context("running pkexec")?;
                if !out.status.success() {
                    let why = String::from_utf8_lossy(&out.stderr).trim().to_owned();
                    bail!("{}", if why.is_empty() { "installing was cancelled".to_owned() } else { why });
                }
                let _ = std::fs::remove_file(package);
                super::start_after_exit("", Path::new(INSTALLED))
            }
        }
    }
}

#[cfg(target_os = "macos")]
mod macos {
    use std::{
        path::{Path, PathBuf},
        process::Command,
    };

    use anyhow::{Context as _, bail};

    /// `/Applications/dextr.app` when running from `…/dextr.app/Contents/MacOS/dextr`.
    pub fn bundle() -> Option<PathBuf> {
        let exe = std::env::current_exe().ok()?.canonicalize().ok()?;
        let bundle = exe.parent()?.parent()?.parent()?;
        (bundle.extension()? == "app").then(|| bundle.to_path_buf())
    }

    fn run(cmd: &mut Command) -> anyhow::Result<String> {
        let out = cmd.output().with_context(|| format!("running {cmd:?}"))?;
        if !out.status.success() {
            bail!("{cmd:?} failed: {}", String::from_utf8_lossy(&out.stderr).trim());
        }
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    }

    /// Who signed an app, as macOS's "designated requirement" (for example
    /// `identifier "dev.dextr.dextr" and certificate leaf = H"…"`). None for
    /// an ad-hoc signature, which names no signer.
    fn requirement(app: &Path) -> Option<String> {
        let info = Command::new("codesign").args(["-dv"]).arg(app).output().ok()?;
        // codesign prints its details on stderr.
        if String::from_utf8_lossy(&info.stderr).contains("Signature=adhoc") {
            return None;
        }
        let out = Command::new("codesign").args(["-d", "--requirements", "-"]).arg(app).output().ok()?;
        let text = String::from_utf8_lossy(&out.stdout).into_owned() + &String::from_utf8_lossy(&out.stderr);
        text.lines().find_map(|l| l.strip_prefix("designated => ")).map(|r| r.trim().to_owned())
    }

    struct Mount(PathBuf);

    impl Drop for Mount {
        fn drop(&mut self) {
            let _ = Command::new("hdiutil").args(["detach", "-quiet", "-force"]).arg(&self.0).output();
        }
    }

    pub fn install(dmg: &Path) -> anyhow::Result<()> {
        let current = bundle().context("dextr isn't running from an app bundle")?;
        let folder = current.parent().context("no folder around the app")?.to_path_buf();

        let point = super::download_dir().join("mount");
        let _ = std::fs::create_dir_all(&point);
        run(Command::new("hdiutil").args(["attach", "-nobrowse", "-readonly", "-quiet", "-mountpoint"]).arg(&point).arg(dmg))
            .context("opening the update")?;
        let mount = Mount(point);

        let new = std::fs::read_dir(&mount.0)?
            .filter_map(|e| e.ok().map(|e| e.path()))
            .find(|p| p.extension().is_some_and(|e| e == "app"))
            .context("the update has no app in it")?;

        // An intact signature, from whoever signed this copy (an ad-hoc
        // copy names no signer, so there's nothing to compare with).
        run(Command::new("codesign").args(["--verify", "--deep", "--strict"]).arg(&new)).context("the update's signature is invalid")?;
        if let Some(ours) = requirement(&current) {
            run(Command::new("codesign").args(["--verify", "--deep", "--strict"]).arg(format!("-R={ours}")).arg(&new))
                .context("the update isn't signed by the same developer as this copy")?;
        }

        // Copy next to the current app, then swap: the running program keeps
        // working from its open files until it quits.
        let staged = folder.join(".dextr-update.app");
        let old = folder.join(".dextr-old.app");
        let _ = std::fs::remove_dir_all(&staged);
        let _ = std::fs::remove_dir_all(&old);
        run(Command::new("ditto").arg(&new).arg(&staged)).context("copying the update")?;
        drop(mount);
        std::fs::rename(&current, &old).context("moving the current app aside")?;
        if let Err(e) = std::fs::rename(&staged, &current) {
            let _ = std::fs::rename(&old, &current);
            return Err(e).context("putting the new app in place");
        }
        let _ = std::fs::remove_dir_all(&old);
        let _ = std::fs::remove_file(dmg);

        // Start the new version once this one has quit.
        super::start_after_exit("open", &current)
    }
}
