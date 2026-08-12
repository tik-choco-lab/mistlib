use std::{env, process::Command};

fn git_output(args: &[&str]) -> Option<String> {
    let output = Command::new("git").args(args).output().ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

fn main() {
    println!("cargo:rerun-if-env-changed=MISTLIB_BUILD_COMMIT");
    println!("cargo:rerun-if-env-changed=MISTLIB_BUILD_DIRTY");
    println!("cargo:rerun-if-env-changed=GITHUB_SHA");
    println!("cargo:rerun-if-changed=../.git/HEAD");
    println!("cargo:rerun-if-changed=../.git/index");

    let commit = env::var("MISTLIB_BUILD_COMMIT")
        .ok()
        .or_else(|| env::var("GITHUB_SHA").ok())
        .or_else(|| git_output(&["rev-parse", "--short=12", "HEAD"]))
        .unwrap_or_else(|| "unknown".to_owned());

    let dirty = env::var("MISTLIB_BUILD_DIRTY").unwrap_or_else(|_| {
        git_output(&["status", "--porcelain", "--untracked-files=no"])
            .map(|status| (!status.is_empty()).to_string())
            .unwrap_or_else(|| "false".to_owned())
    });

    println!("cargo:rustc-env=MISTLIB_BUILD_COMMIT={commit}");
    println!("cargo:rustc-env=MISTLIB_BUILD_DIRTY={dirty}");
    println!(
        "cargo:rustc-env=MISTLIB_BUILD_TARGET={}",
        env::var("TARGET").unwrap_or_else(|_| "unknown".to_owned())
    );
    println!(
        "cargo:rustc-env=MISTLIB_BUILD_PROFILE={}",
        env::var("PROFILE").unwrap_or_else(|_| "unknown".to_owned())
    );
}
