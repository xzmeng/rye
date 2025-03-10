use std::borrow::Cow;
use std::env::consts::{ARCH, EXE_EXTENSION, OS};
use std::env::{join_paths, split_paths};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::{env, fs};

use anyhow::{bail, Context, Error};
use clap::{CommandFactory, Parser};
use clap_complete::Shell;
use console::style;
use minijinja::render;
use self_replace::self_delete_outside_path;
use tempfile::tempdir;

use crate::bootstrap::{
    download_url, download_url_ignore_404, ensure_self_venv, is_self_compatible_toolchain,
    update_core_shims,
};
use crate::cli::toolchain::register_toolchain;
use crate::platform::{get_app_dir, symlinks_supported};
use crate::utils::{check_checksum, CommandOutput, QuietExit};

#[cfg(windows)]
const DEFAULT_HOME: &str = "%USERPROFILE%\\.rye";
#[cfg(unix)]
const DEFAULT_HOME: &str = "$HOME/.rye";

const GITHUB_REPO: &str = "https://github.com/mitsuhiko/rye";
const UNIX_ENV_FILE: &str = r#"
# rye shell setup
{%- if custom_home %}
export RYE_HOME="{{ rye_home }}"
{%- endif %}
case ":${PATH}:" in
  *:"{{ rye_home }}/shims":*)
    ;;
  *)
    export PATH="{{ rye_home }}/shims:$PATH"
    ;;
esac

"#;

/// Rye self management
#[derive(Parser, Debug)]
pub struct Args {
    #[command(subcommand)]
    command: SubCommand,
}

/// Generates a completion script for a shell.
#[derive(Parser, Debug)]
pub struct CompletionCommand {
    /// The shell to generate a completion script for (defaults to 'bash').
    #[arg(short, long)]
    shell: Option<Shell>,
}

/// Performs an update of rye.
///
/// This currently just is an alias to running cargo install again with the
/// right arguments.
#[derive(Parser, Debug)]
pub struct UpdateCommand {
    /// Update to a specific version.
    #[arg(long)]
    version: Option<String>,
    /// Update to a specific tag.
    #[arg(long)]
    tag: Option<String>,
    /// Update to a specific git rev.
    #[arg(long, conflicts_with = "tag")]
    rev: Option<String>,
    /// Force reinstallation
    #[arg(long)]
    force: bool,
}

/// Triggers the initial installation of Rye.
///
/// This command is executed by the installation step to move Rye
/// to the intended target location and to add Rye to the environment
/// variables.
#[derive(Parser, Debug)]
pub struct InstallCommand {
    /// Skip prompts.
    #[arg(short, long)]
    yes: bool,
    /// Register a specific toolchain before bootstrap.
    #[arg(long)]
    toolchain: Option<PathBuf>,
}

#[derive(Debug, Copy, Clone)]
enum InstallMode {
    Default,
    NoPrompts,
    AutoInstall,
}

/// Uninstalls rye again.
#[derive(Parser, Debug)]
pub struct UninstallCommand {
    /// Skip safety check.
    #[arg(short, long)]
    yes: bool,
}

#[derive(Parser, Debug)]
enum SubCommand {
    Completion(CompletionCommand),
    Update(UpdateCommand),
    #[command(hide = true)]
    Install(InstallCommand),
    Uninstall(UninstallCommand),
}

pub fn execute(cmd: Args) -> Result<(), Error> {
    match cmd.command {
        SubCommand::Completion(args) => completion(args),
        SubCommand::Update(args) => update(args),
        SubCommand::Install(args) => install(args),
        SubCommand::Uninstall(args) => uninstall(args),
    }
}

fn completion(args: CompletionCommand) -> Result<(), Error> {
    clap_complete::generate(
        args.shell.unwrap_or(Shell::Bash),
        &mut super::Args::command(),
        "rye",
        &mut std::io::stdout(),
    );

    Ok(())
}

fn update(args: UpdateCommand) -> Result<(), Error> {
    // make sure to read the exe before self_replace as otherwise we might read
    // a bad executable name on Linux where the move is picked up.
    let current_exe = env::current_exe()?;

    // git based installation with cargo
    if args.rev.is_some() || args.tag.is_some() {
        let mut cmd = Command::new("cargo");
        let tmp = tempdir()?;
        cmd.arg("install")
            .arg("--git")
            .arg("https://github.com/mitsuhiko/rye")
            .arg("--root")
            .env(
                "PATH",
                join_paths(
                    Some(tmp.path().join("bin"))
                        .into_iter()
                        .chain(split_paths(&env::var_os("PATH").unwrap_or_default())),
                )?,
            )
            .arg(tmp.path());
        if let Some(ref rev) = args.rev {
            cmd.arg("--rev");
            cmd.arg(rev);
        } else if let Some(ref tag) = args.tag {
            cmd.arg("--tag");
            cmd.arg(tag);
        }
        if args.force {
            cmd.arg("--force");
        }
        cmd.arg("rye");
        let status = cmd.status().context("unable to update via cargo-install")?;
        if !status.success() {
            bail!("failed to self-update via cargo-install");
        }
        update_exe_and_shims(
            &tmp.path()
                .join("bin")
                .join("rye")
                .with_extension(EXE_EXTENSION),
        )?;
    } else {
        let version = args.version.as_deref().unwrap_or("latest");
        echo!("Updating to {version}");
        let binary = format!("rye-{ARCH}-{OS}");
        let ext = if cfg!(unix) { ".gz" } else { ".exe" };
        let url = if version == "latest" {
            format!("{GITHUB_REPO}/releases/latest/download/{binary}{ext}")
        } else {
            format!("{GITHUB_REPO}/releases/download/{version}/{binary}{ext}")
        };
        let sha256_url = format!("{}.sha256", url);
        let bytes = download_url(&url, CommandOutput::Normal)
            .with_context(|| format!("could not download release {version} for this platform"))?;
        if let Some(sha256_bytes) = download_url_ignore_404(&sha256_url, CommandOutput::Normal)? {
            let checksum = String::from_utf8_lossy(&sha256_bytes);
            echo!("Checking checksum");
            check_checksum(&bytes, checksum.trim())
                .with_context(|| format!("hash check of {} failed", url))?;
        } else {
            echo!("Checksum check skipped (no hash available)");
        }

        let tmp = tempfile::NamedTempFile::new()?;

        // unix currently comes compressed, windows comes uncompressed
        #[cfg(unix)]
        {
            use std::io::Read;
            let mut decoder = flate2::bufread::GzDecoder::new(&bytes[..]);
            let mut rv = Vec::new();
            decoder.read_to_end(&mut rv)?;
            fs::write(tmp.path(), rv)?;
        }
        #[cfg(windows)]
        {
            fs::write(tmp.path(), bytes)?;
        }
        update_exe_and_shims(tmp.path())?;
    }

    echo!("Updated!");
    echo!();
    Command::new(current_exe).arg("--version").status()?;

    Ok(())
}

fn update_exe_and_shims(new_exe: &Path) -> Result<(), Error> {
    let app_dir = get_app_dir().canonicalize()?;
    let current_exe = env::current_exe()?.canonicalize()?;
    let shims = app_dir.join("shims");

    self_replace::self_replace(new_exe)?;

    // if the shims have been created before (they really should have)
    // we want to make sure that they point to the new executable now.
    // for symlinks that probably is not necessary, but for hardlinks
    // that's very important.
    if shims.is_dir() {
        update_core_shims(&shims, &current_exe)?;
    }

    Ok(())
}

fn install(args: InstallCommand) -> Result<(), Error> {
    perform_install(
        if args.yes {
            InstallMode::NoPrompts
        } else {
            InstallMode::Default
        },
        args.toolchain.as_deref(),
    )
}

fn remove_dir_all_if_exists(path: &Path) -> Result<(), Error> {
    if path.is_dir() {
        fs::remove_dir_all(path)?;
    }
    Ok(())
}

fn uninstall(args: UninstallCommand) -> Result<(), Error> {
    if !args.yes
        && !dialoguer::Confirm::new()
            .with_prompt("Do you want to uninstall rye?")
            .interact()?
    {
        return Ok(());
    }

    let app_dir = get_app_dir();
    if app_dir.is_dir() {
        let real_exe = env::current_exe()?.canonicalize()?;
        let real_app_dir = app_dir.canonicalize()?;

        // try to delete all shims that can be found.  Ignore if deletes don't work.
        // The delete of the current executable for instance will fail on windows.
        let shim_dir = app_dir.join("shims");
        if let Ok(dir) = shim_dir.read_dir() {
            for entry in dir.flatten() {
                fs::remove_file(&entry.path()).ok();
            }
        }

        remove_dir_all_if_exists(&app_dir.join("self"))?;
        remove_dir_all_if_exists(&app_dir.join("py"))?;
        remove_dir_all_if_exists(&app_dir.join("pip-tools"))?;

        // special deleting logic if we are placed in the app dir and the shim deletion
        // did not succeed.  This is likely the case on windows where we then use the
        // `self_delete` crate.
        if real_exe.strip_prefix(&real_app_dir).is_ok() && real_exe.is_file() {
            self_delete_outside_path(&real_app_dir)?;
        }

        // at this point the remaining shim folder should be deletable
        remove_dir_all_if_exists(&app_dir.join("shims"))?;

        // leave this empty behind in case someone sourced it.  The config also stays around.
        let env_file = app_dir.join("env");
        if env_file.is_file() {
            fs::write(env_file, "")?;
        }
    }

    echo!("Done!");
    echo!();

    let rye_home = env::var("RYE_HOME")
        .map(Cow::Owned)
        .unwrap_or(Cow::Borrowed(DEFAULT_HOME));
    if cfg!(unix) {
        echo!(
            "Don't forget to remove the sourcing of {} from your shell config.",
            Path::new(&rye_home as &str).join("env").display()
        );
    } else {
        echo!(
            "Don't forget to remove {} from your PATH",
            Path::new(&rye_home as &str).join("shims").display()
        )
    }

    Ok(())
}

#[cfg(unix)]
fn is_fish() -> bool {
    use whattheshell::Shell;
    Shell::infer().map_or(false, |x| matches!(x, Shell::Fish))
}

fn perform_install(mode: InstallMode, toolchain_path: Option<&Path>) -> Result<(), Error> {
    let exe = env::current_exe()?;
    let app_dir = get_app_dir();
    let shims = app_dir.join("shims");
    let target = shims.join("rye").with_extension(EXE_EXTENSION);

    echo!("{}", style("Welcome to Rye!").bold());

    if matches!(mode, InstallMode::AutoInstall) {
        echo!();
        echo!("Rye has detected that it's not installed on this computer yet and");
        echo!("automatically started the installer for you.  For more information");
        echo!(
            "read {}",
            style("https://rye-up.com/guide/installation/").yellow()
        );
    }

    echo!();
    echo!(
        "This installer will install rye to {}",
        style(app_dir.display()).cyan()
    );
    echo!(
        "This path can be changed by exporting the {} environment variable.",
        style("RYE_HOME").cyan()
    );
    echo!();
    echo!("{}", style("Details:").bold());
    echo!("  Rye Version: {}", style(env!("CARGO_PKG_VERSION")).cyan());
    echo!("  Platform: {} ({})", style(OS).cyan(), style(ARCH).cyan());

    if cfg!(windows) && !symlinks_supported() {
        echo!();
        warn!("your Windows configuration does not support symlinks.");
        echo!();
        echo!("It's strongly recommended that you enable developer mode in Windows to");
        echo!("enable symlinks.  You need to enable this before continuing the setup.");
        echo!(
            "Learn more at {}",
            style("https://rye-up.com/guide/faq/#windows-developer-mode").yellow()
        );
    }

    echo!();
    if !matches!(mode, InstallMode::NoPrompts)
        && !dialoguer::Confirm::new()
            .with_prompt("Continue?")
            .interact()?
    {
        elog!("Installation cancelled!");
        return Err(QuietExit(1).into());
    }

    // place executable in rye home folder
    fs::create_dir_all(&shims).ok();
    if target.is_file() {
        fs::remove_file(&target)?;
    }
    fs::copy(exe, &target)?;
    echo!("Installed binary to {}", style(target.display()).cyan());

    // write an env file we can source later.  Prefer $HOME/.rye over
    // the expanded path, if not overridden.
    let (custom_home, rye_home) = env::var("RYE_HOME")
        .map(|x| (true, Cow::Owned(x)))
        .unwrap_or((false, Cow::Borrowed(DEFAULT_HOME)));

    if cfg!(unix) {
        fs::write(
            app_dir.join("env"),
            render!(UNIX_ENV_FILE, custom_home, rye_home),
        )?;
    }

    // Register a toolchain if provided.
    if let Some(toolchain_path) = toolchain_path {
        echo!(
            "Registering toolchain at {}",
            style(toolchain_path.display()).cyan()
        );
        let version = register_toolchain(toolchain_path, None, |ver| {
            if ver.name != "cpython" {
                bail!("Only cpython toolchains are allowed, got '{}'", ver.name);
            } else if !is_self_compatible_toolchain(ver) {
                bail!(
                    "Toolchain {} is not version compatible for internal use.",
                    ver
                );
            }
            Ok(())
        })?;
        echo!("Registered toolchain as {}", style(version).cyan());
    }

    // Ensure internals next
    let self_path = ensure_self_venv(CommandOutput::Normal)?;
    echo!(
        "Updated self-python installation at {}",
        style(self_path.display()).cyan()
    );

    #[cfg(unix)]
    {
        if !env::split_paths(&env::var_os("PATH").unwrap())
            .any(|x| same_file::is_same_file(x, &shims).unwrap_or(false))
        {
            echo!();
            echo!(
                "The rye directory {} was not detected on {}.",
                style(shims.display()).cyan(),
                style("PATH").cyan()
            );
            echo!("It is highly recommended that you add it.");
            echo!("Add this at the end of your .profile, .zprofile or similar:");
            echo!();
            echo!("    source \"{}/env\"", rye_home);
            echo!();
            if is_fish() {
                echo!("To make it work with fish, run this once instead:");
                echo!();
                echo!("    set -Ua fish_user_paths \"{}/shims\"", rye_home);
                echo!();
            }
            echo!("Note: after adding rye to your path, restart your shell for it to take effect.");
        }
    }
    #[cfg(windows)]
    {
        echo!();
        echo!("Note: You need to manually add {DEFAULT_HOME} to your PATH.");
    }

    echo!("For more information read https://mitsuhiko.github.io/rye/guide/installation");

    echo!();
    echo!("{}", style("All done!").green());

    Ok(())
}

pub fn auto_self_install() -> Result<bool, Error> {
    // disables self installation
    if env::var("RYE_NO_AUTO_INSTALL").ok().as_deref() == Some("1") {
        return Ok(false);
    }

    // auto install reads RYE_TOOLCHAIN to pre-register a
    // regular toolchain.
    let toolchain_path = env::var_os("RYE_TOOLCHAIN");

    let app_dir = get_app_dir();
    let rye_exe = app_dir
        .join("shims")
        .join("rye")
        .with_extension(EXE_EXTENSION);

    // it's already installed, don't install
    if app_dir.is_dir() && rye_exe.is_file() {
        Ok(false)
    } else {
        // in auto installation we want to show a continue prompt before we shut down
        // so that the cmd.exe does not close.
        #[cfg(windows)]
        {
            crate::request_continue_prompt();
        }

        perform_install(
            InstallMode::AutoInstall,
            toolchain_path.as_ref().map(Path::new),
        )?;
        Ok(true)
    }
}
