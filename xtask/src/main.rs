//! `cargo xtask bundle <package> [cargo build args]` and
//! `cargo xtask bundle-universal <package> [cargo build args]`.
//!
//! The same as `nih_plug_xtask::main()`, except for how the workspace root is
//! found. nih_plug_xtask picks the *outermost* directory with a Cargo.toml,
//! so in a git worktree nested inside the main checkout (for example
//! `.claude/worktrees/...`) it silently builds and bundles the main
//! checkout instead. The root is simply the parent of this crate.

use std::path::Path;

fn main() -> anyhow::Result<()> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("xtask lives inside the workspace");
    std::env::set_current_dir(root)?;

    let mut args = std::env::args().skip(1);
    let usage = "Usage: cargo xtask bundle|bundle-universal <package> [-p <package>...] [cargo build args]";
    let universal = match args.next().as_deref() {
        Some("bundle") => false,
        Some("bundle-universal") => true,
        _ => anyhow::bail!("{usage}"),
    };

    // The first argument may be a package name; `-p <package>` adds more.
    let mut packages = Vec::new();
    let mut other_args = Vec::new();
    let mut rest = args.peekable();
    if let Some(first) = rest.next_if(|a| !a.starts_with('-')) {
        packages.push(first);
    }
    while let Some(arg) = rest.next() {
        match arg.as_str() {
            "-p" | "--package" => packages.extend(rest.next()),
            _ => other_args.push(arg),
        }
    }
    if packages.is_empty() {
        anyhow::bail!("{usage}");
    }

    let metadata = cargo_metadata::MetadataCommand::new()
        .manifest_path("./Cargo.toml")
        .no_deps()
        .exec()?;
    let target_dir = metadata.target_directory.as_std_path();

    if universal {
        if other_args.iter().any(|a| a == "--target" || a.starts_with("--target=")) {
            anyhow::bail!("bundle-universal can't be combined with --target");
        }
        for target in ["x86_64-apple-darwin", "aarch64-apple-darwin"] {
            let mut target_args = other_args.clone();
            target_args.push(format!("--target={target}"));
            nih_plug_xtask::build(&packages, &target_args)?;
        }
    } else {
        nih_plug_xtask::build(&packages, &other_args)?;
    }
    for package in &packages {
        nih_plug_xtask::bundle(target_dir, package, &other_args, universal)?;
    }
    Ok(())
}
