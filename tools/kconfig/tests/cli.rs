// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Exercise the authoring CLI, not just its in-memory resolver.

use std::{
    fs,
    io::Write,
    path::PathBuf,
    process::{Command, Output, Stdio},
    sync::atomic::{AtomicUsize, Ordering},
};

static NEXT: AtomicUsize = AtomicUsize::new(0);
struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "hyper-config-cli-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        require(fs::create_dir_all(&path));
        Self(path)
    }
    fn run(&self, command: &str, input: &str) -> Output {
        let mut child = require(
            Command::new(env!("CARGO_BIN_EXE_hyper-kconfig"))
                .current_dir(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../.."))
                .arg(command)
                .arg(self.0.join("input"))
                .arg(self.0.join("output"))
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn(),
        );
        if let Some(mut stdin) = child.stdin.take() {
            require(stdin.write_all(input.as_bytes()));
        }
        require(child.wait_with_output())
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
fn require<T, E: std::fmt::Debug>(result: Result<T, E>) -> T {
    match result {
        Ok(value) => value,
        Err(error) => panic!("{error:?}"),
    }
}

#[test]
fn schema_sources_preserve_menus_and_file_diagnostics() {
    let fixture = Fixture::new();
    require(fs::create_dir(fixture.0.join("app")));
    require(fs::write(
        fixture.0.join("Kconfig"),
        "mainmenu \"Product\"\nmenu \"Apps\"\nsource \"app/Kconfig\"\nendmenu\n",
    ));
    require(fs::write(
        fixture.0.join("app/Kconfig"),
        "config EDITOR\n bool \"Editor\"\n default y\n help\n  source \"not-an-include\"\n",
    ));
    let schema = require(hyper_kconfig::parse_schema(&fixture.0.join("Kconfig")));
    assert_eq!(schema.symbols[0].section, "Apps");
    assert!(schema.symbols[0].help.contains("not-an-include"));
    require(fs::write(
        fixture.0.join("app/Kconfig"),
        "invalid-property\n",
    ));
    let error = match hyper_kconfig::parse_schema(&fixture.0.join("Kconfig")) {
        Ok(_) => panic!("invalid included schema accepted"),
        Err(error) => error.to_string(),
    };
    assert!(error.contains("app/Kconfig:1"), "{error}");
    require(fs::write(
        fixture.0.join("app/Kconfig"),
        "source \"Kconfig\"\n",
    ));
    assert!(hyper_kconfig::parse_schema(&fixture.0.join("Kconfig")).is_err());
    require(fs::remove_file(fixture.0.join("app/Kconfig")));
    assert!(hyper_kconfig::parse_schema(&fixture.0.join("Kconfig")).is_err());
}

#[test]
fn generic_schema_protocol_applies_defaults_without_kernel_constraints() {
    let fixture = Fixture::new();
    require(fs::write(
        fixture.0.join("Kconfig"),
        "config EDITOR\n bool \"Editor\"\n default y\nconfig POLICY\n string \"Policy\"\n default \"\"\n",
    ));
    let policy = "a path/with \\".to_owned() + " and \"quotes\"";
    require(fs::write(
        fixture.0.join("input"),
        format!("CONFIG_POLICY={}\n", hyper_kconfig::quote(&policy)),
    ));
    let schema = require(hyper_kconfig::parse_schema(&fixture.0.join("Kconfig")));
    let config = require(hyper_kconfig::resolve(
        &schema,
        require(hyper_kconfig::load_fragment(&fixture.0.join("input"))),
    ));
    assert_eq!(config.value("POLICY"), Some(policy.as_str()));
    require(hyper_kconfig::write_config(
        &fixture.0.join("output"),
        &schema,
        &config,
    ));
    let roundtrip = require(hyper_kconfig::resolve(
        &schema,
        require(hyper_kconfig::load_fragment(&fixture.0.join("output"))),
    ));
    assert_eq!(roundtrip, config);
    let output = require(
        Command::new(env!("CARGO_BIN_EXE_hyper-kconfig"))
            .arg("--schema")
            .arg(fixture.0.join("Kconfig"))
            .arg("resolve")
            .arg(fixture.0.join("input"))
            .output(),
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("\"CONFIG_EDITOR\":\"y\""));
}

#[test]
fn interactive_choice_changes_architecture_and_preserves_common_tuning() {
    let fixture = Fixture::new();
    require(fs::write(
        fixture.0.join("input"),
        "CONFIG_ARCH_AARCH64=y\nCONFIG_ARM64_PA_BITS=40\nCONFIG_LOG_LINE_MAX=1024\n",
    ));
    let output = fixture.run("config", &format!("2\n{}", "\n".repeat(32)));
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let text = require(fs::read_to_string(fixture.0.join("output")));
    assert!(text.contains("CONFIG_ARCH_RISCV64=y"));
    assert!(text.contains("CONFIG_LOG_LINE_MAX=1024"));
    assert!(!text.contains("CONFIG_ARCH_AARCH64"));
    assert!(!text.contains("CONFIG_ARM64_"));
    let prompts = String::from_utf8_lossy(&output.stdout);
    assert!(prompts.contains("Selection [1]"));
    assert!(!prompts.contains("AArch64 host virtual-address width"));
}

#[test]
fn invalid_configuration_never_overwrites_the_output() {
    for source in [
        "CONFIG_ARCH_RISCV64=y\nCONFIG_ARM64_PA_BITS=40\n",
        "CONFIG_ARCH_AARCH64=y\nCONFIG_ARCH_X86_64=y\n",
        "CONFIG_KERNEL_STACK_SIZE_KB=24\n",
        "CONFIG_ARM64_PA_BITS=41\n",
    ] {
        let fixture = Fixture::new();
        require(fs::write(fixture.0.join("input"), source));
        require(fs::write(
            fixture.0.join("output"),
            "keep this configuration",
        ));
        let output = fixture.run("defconfig", "");
        assert!(!output.status.success());
        assert_eq!(
            require(fs::read_to_string(fixture.0.join("output"))),
            "keep this configuration"
        );
    }
}

#[test]
fn interactive_eof_does_not_write_a_partial_configuration() {
    let fixture = Fixture::new();
    let output = fixture.run("config", "");
    assert!(!output.status.success());
    assert!(!fixture.0.join("output").exists());
}

#[test]
fn migration_warns_and_removes_old_architecture_leakage() {
    let fixture = Fixture::new();
    require(fs::write(
        fixture.0.join("input"),
        "CONFIG_ARCH_AARCH64=n\nCONFIG_ARCH_X86_64=y\nCONFIG_ARM64_PA_BITS=48\nCONFIG_MAX_KERNEL_STACKS=1024\nCONFIG_SCHED_MAX_THREADS=1024\nCONFIG_MAX_CPUS=16\n",
    ));
    let output = fixture.run("olddefconfig", "");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let warnings = String::from_utf8_lossy(&output.stderr);
    assert!(warnings.contains("CONFIG_MAX_KERNEL_STACKS"));
    assert!(warnings.contains("CONFIG_SCHED_MAX_THREADS"));
    assert!(warnings.contains("CONFIG_ARM64_PA_BITS"));
    let text = require(fs::read_to_string(fixture.0.join("output")));
    assert!(text.contains("CONFIG_MAX_CPUS=16"));
    assert!(!text.contains("CONFIG_SCHED_MAX_THREADS"));
    assert!(!text.contains("CONFIG_ARM64_"));
}

#[test]
fn menuconfig_terminal_and_configuration_contracts() {
    let output = require(
        Command::new("python3")
            .arg("-B")
            .arg(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/menuconfig.py"))
            .arg(env!("CARGO_BIN_EXE_hyper-kconfig"))
            .output(),
    );
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
}
