//! Installing a downloaded update in place (macOS: the .app from a DMG).

use std::path::{Path, PathBuf};

/// Whether this copy can update itself. On macOS that's when it runs from
/// an app bundle in a folder we can write to; elsewhere a package manager
/// (or `git pull`) updates it.
pub fn can_install() -> bool {
    #[cfg(target_os = "macos")]
    {
        macos::bundle().is_some_and(|b| b.parent().is_some_and(writable))
    }
    #[cfg(not(target_os = "macos"))]
    {
        false
    }
}

#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn writable(dir: &Path) -> bool {
    let probe = dir.join(format!(".dextr-write-test-{}", std::process::id()));
    let ok = std::fs::write(&probe, b"").is_ok();
    let _ = std::fs::remove_file(&probe);
    ok
}

/// Replaces the running app with the one in `package` and starts it once
/// this process exits. The caller should quit right after.
pub fn install(package: &Path) -> anyhow::Result<()> {
    #[cfg(target_os = "macos")]
    {
        macos::install(package)
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = package;
        anyhow::bail!("updates are installed by your package manager on this system")
    }
}

/// Where downloads go before they're installed.
pub fn download_dir() -> PathBuf {
    std::env::temp_dir().join("dextr-update")
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

    /// The signing team of an app, if it has one (ad-hoc signatures don't).
    fn team(app: &Path) -> Option<String> {
        let out = Command::new("codesign").args(["-dv", "--verbose=2"]).arg(app).output().ok()?;
        // codesign prints its details on stderr.
        String::from_utf8_lossy(&out.stderr)
            .lines()
            .find_map(|l| l.strip_prefix("TeamIdentifier="))
            .map(str::trim)
            .filter(|t| !t.is_empty() && *t != "not set")
            .map(str::to_owned)
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

        // Same signer as this copy, and an intact signature.
        run(Command::new("codesign").args(["--verify", "--deep", "--strict"]).arg(&new)).context("the update's signature is invalid")?;
        if let Some(ours) = team(&current)
            && team(&new).as_deref() != Some(ours.as_str())
        {
            bail!("the update isn't signed by the same developer as this copy");
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
        let script = format!("while kill -0 {} 2>/dev/null; do sleep 0.2; done; open \"$0\"", std::process::id());
        Command::new("/bin/sh").arg("-c").arg(script).arg(&current).spawn().context("scheduling the restart")?;
        Ok(())
    }
}
