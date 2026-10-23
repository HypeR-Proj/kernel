// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

use super::*;

fn require_ok<T, E: fmt::Debug>(result: Result<T, E>) -> T {
    match result {
        Ok(value) => value,
        Err(error) => panic!("required success, received {error:?}"),
    }
}

fn test_schema() -> Schema {
    Schema {
        title: String::from("Test"),
        choices: Vec::new(),
        symbols: vec![
            Symbol {
                name: String::from("PARENT"),
                kind: ValueKind::Bool,
                prompt: String::from("Parent"),
                default: String::from("n"),
                dependencies: Vec::new(),
                range: None,
                help: String::new(),
                choice: None,
                section: String::new(),
            },
            Symbol {
                name: String::from("CHILD"),
                kind: ValueKind::Bool,
                prompt: String::from("Child"),
                default: String::from("y"),
                dependencies: vec![Dependency {
                    symbol: String::from("PARENT"),
                    inverted: false,
                }],
                range: None,
                help: String::new(),
                choice: None,
                section: String::new(),
            },
            Symbol {
                name: String::from("COUNT"),
                kind: ValueKind::Int,
                prompt: String::from("Count"),
                default: String::from("4"),
                dependencies: Vec::new(),
                range: Some((1, 8)),
                help: String::new(),
                choice: None,
                section: String::new(),
            },
        ],
    }
}

#[test]
fn resolves_dependencies_and_integer_ranges() {
    let configuration = require_ok(resolve(&test_schema(), BTreeMap::new()));
    assert_eq!(configuration.value("CHILD"), None);

    let mut invalid = BTreeMap::new();
    invalid.insert(String::from("COUNT"), String::from("9"));
    assert!(resolve(&test_schema(), invalid).is_err());
}

#[test]
fn migration_preserves_known_values_and_reports_removed_symbols() {
    let values = BTreeMap::from([
        (String::from("PARENT"), String::from("y")),
        (String::from("COUNT"), String::from("7")),
        (String::from("REMOVED"), String::from("y")),
    ]);
    assert!(resolve(&test_schema(), values.clone()).is_err());
    let (configuration, removed) = require_ok(resolve_existing(&test_schema(), values));
    assert_eq!(configuration.value("PARENT"), Some("y"));
    assert_eq!(configuration.value("CHILD"), Some("y"));
    assert_eq!(configuration.value("COUNT"), Some("7"));
    assert_eq!(removed, ["REMOVED"]);
}

#[test]
fn migration_still_rejects_invalid_recognized_values() {
    let values = BTreeMap::from([
        (String::from("COUNT"), String::from("9")),
        (String::from("REMOVED"), String::from("y")),
    ]);
    assert!(resolve_existing(&test_schema(), values).is_err());
}

#[test]
fn parses_the_supported_kconfig_subset() {
    let path =
        std::env::temp_dir().join(format!("hyper-kconfig-test-{}-Kconfig", std::process::id()));
    require_ok(fs::write(
        &path,
        "mainmenu \"Test\"\nconfig ENABLED\n    bool \"Enabled\"\n    default y\n",
    ));
    let schema = require_ok(parse_schema(&path));
    let remove_result = fs::remove_file(&path);
    require_ok(remove_result);

    assert_eq!(schema.title, "Test");
    assert_eq!(schema.symbols.len(), 1);
    assert_eq!(schema.symbols[0].default, "y");
}

fn schema(source: &str) -> Schema {
    require_ok(parse_schema_source(Path::new("test-Kconfig"), source))
}

fn kernel_schema() -> Schema {
    schema(include_str!("../../../Kconfig"))
}

#[test]
fn scheduling_windows_preserve_native_floor_and_valid_budgets() {
    let schema = kernel_schema();
    for overrides in [
        vec![("SCHED_VCPU_BUDGET_MS", "21")],
        vec![("SCHED_VCPU_BURST_MS", "21")],
        vec![("SCHED_PRIORITY_ALLOWANCE_MS", "18")],
        vec![("SCHED_PROTECTED_MIN_MS", "16")],
    ] {
        let config = require_ok(resolve(&schema, values(&overrides)));
        assert!(validate_kernel_configuration(&config, None).is_err());
    }
    for backend in ["SCHED_FAIR_STRIDE", "SCHED_FAIR_RR"] {
        for preference in ["SCHED_PREFER_GUEST", "SCHED_PREFER_NATIVE"] {
            let config = require_ok(resolve(
                &schema,
                values(&[
                    (backend, "y"),
                    (preference, "y"),
                    ("SCHED_VCPU_BUDGET_MS", "20"),
                ]),
            ));
            require_ok(validate_kernel_configuration(&config, None));
        }
    }
}

fn values(items: &[(&str, &str)]) -> BTreeMap<String, String> {
    items
        .iter()
        .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
        .collect()
}

#[test]
fn architecture_is_one_choice_not_three_independent_defaults() {
    let schema = kernel_schema();
    let choices: Vec<_> = schema
        .choices
        .iter()
        .filter(|choice| choice.name == "ARCH")
        .collect();
    assert_eq!(choices.len(), 1);
    assert_eq!(
        choices[0].members,
        ["ARCH_AARCH64", "ARCH_RISCV64", "ARCH_X86_64"]
    );
    for selected in ["ARCH_AARCH64", "ARCH_RISCV64", "ARCH_X86_64"] {
        let configuration = require_ok(resolve(&schema, values(&[(selected, "y")])));
        for name in &choices[0].members {
            assert_eq!(
                configuration.value(name),
                Some(if name == selected { "y" } else { "n" })
            );
        }
        require_ok(validate_kernel_configuration(&configuration, None));
    }
    assert!(
        resolve(
            &schema,
            values(&[("ARCH_AARCH64", "y"), ("ARCH_RISCV64", "y")])
        )
        .is_err()
    );
    assert!(
        resolve(
            &schema,
            values(&[
                ("ARCH_AARCH64", "n"),
                ("ARCH_RISCV64", "n"),
                ("ARCH_X86_64", "n")
            ])
        )
        .is_err()
    );
    assert_eq!(
        require_ok(resolve(&schema, BTreeMap::new())).value("ARCH_AARCH64"),
        Some("y")
    );
}

#[test]
fn inactive_numeric_and_boolean_settings_are_absent() {
    let schema = kernel_schema();
    for arch in ["ARCH_RISCV64", "ARCH_X86_64"] {
        let configuration = require_ok(resolve(&schema, values(&[(arch, "y")])));
        for name in [
            "ARM64_VA_BITS",
            "ARM64_PA_BITS",
            "ARM64_IPA_BITS",
            "RANDOMIZE_BASE",
        ] {
            assert_eq!(configuration.value(name), None);
        }
        assert!(resolve(&schema, values(&[(arch, "y"), ("ARM64_PA_BITS", "48")])).is_err());
        assert!(resolve(&schema, values(&[(arch, "y"), ("RANDOMIZE_BASE", "y")])).is_err());
    }
}

#[test]
fn olddefconfig_removes_obsolete_and_inactive_keys_without_resetting_tuning() {
    let schema = kernel_schema();
    let old = values(&[
        ("ARCH_AARCH64", "n"),
        ("ARCH_RISCV64", "y"),
        ("ARM64_PA_BITS", "40"),
        ("RANDOMIZE_BASE", "n"),
        ("ARM64_MAX_GIC_REDISTRIBUTOR_REGIONS", "16"),
        ("MAX_KERNEL_STACKS", "1024"),
        ("SCHED_MAX_THREADS", "1024"),
        ("MAX_CPUS", "16"),
        ("TIMER_HZ", "250"),
        ("LOG_LINE_MAX", "1024"),
        ("KERNEL_STACK_SIZE_KB", "16"),
    ]);
    let (configuration, removed) = require_ok(resolve_existing(&schema, old));
    assert_eq!(removed.len(), 6);
    for name in [
        "ARM64_PA_BITS",
        "RANDOMIZE_BASE",
        "ARM64_MAX_GIC_REDISTRIBUTOR_REGIONS",
        "MAX_KERNEL_STACKS",
        "SCHED_MAX_THREADS",
        "TIMER_HZ",
    ] {
        assert!(removed.iter().any(|item| item == name));
        assert_eq!(configuration.value(name), None);
    }
    assert_eq!(configuration.value("ARCH_RISCV64"), Some("y"));
    assert_eq!(configuration.value("MAX_CPUS"), Some("16"));
    assert_eq!(configuration.value("LOG_LINE_MAX"), Some("1024"));
    assert_eq!(configuration.value("KERNEL_STACK_SIZE_KB"), Some("16"));
    require_ok(validate_kernel_configuration(&configuration, None));
}

#[test]
fn dependencies_are_transitive_and_independent_of_declaration_order() {
    let schema = schema(
        "config COUNT\n int \"Count\"\n default 8\n depends on CHILD\nconfig INVERSE\n bool\n default y\n depends on !CHILD\nconfig CHILD\n bool\n default y\n depends on PARENT\nconfig PARENT\n bool \"Parent\"\n default n\n",
    );
    let configuration = require_ok(resolve(&schema, BTreeMap::new()));
    assert_eq!(configuration.value("CHILD"), None);
    assert_eq!(configuration.value("COUNT"), None);
    assert_eq!(configuration.value("INVERSE"), Some("y"));
    let configuration = require_ok(resolve(&schema, values(&[("PARENT", "y")])));
    assert_eq!(configuration.value("COUNT"), Some("8"));
    assert_eq!(configuration.value("INVERSE"), None);
    assert!(resolve(&schema, values(&[("COUNT", "9")])).is_err());
}

#[test]
fn invalid_schemas_fail_before_resolution() {
    for source in [
        "config A\n bool\nconfig A\n bool\n",
        "config A\n bool\n depends on MISSING\n",
        "config A\n bool\n depends on B\nconfig B\n bool\n depends on A\n",
        "config A\n bool\n depends on A\n",
        "choice ARCH\n default MISSING\nconfig A\n bool\nconfig B\n bool\nendchoice\n",
        "choice ARCH\n default A\nconfig A\n int\n default 4\nconfig B\n bool\nendchoice\n",
        "choice ARCH\n default A\nconfig A\n bool\n",
        "endchoice\nconfig A\n bool\n",
        "menu \"Unfinished\"\nconfig A\n bool\n",
    ] {
        assert!(
            parse_schema_source(Path::new("test-Kconfig"), source).is_err(),
            "{source}"
        );
    }
}

#[test]
fn kernel_validation_rejects_unsupported_stack_sizes_and_pa_widths() {
    let schema = kernel_schema();
    for (name, value) in [
        ("KERNEL_STACK_SIZE_KB", "24"),
        ("IRQ_STACK_SIZE_KB", "48"),
        ("EMERGENCY_STACK_SIZE_KB", "96"),
        ("ARM64_PA_BITS", "41"),
    ] {
        let config = require_ok(resolve(&schema, values(&[(name, value)])));
        assert!(
            validate_kernel_configuration(&config, None).is_err(),
            "{name}"
        );
    }
    for value in ["16", "32", "64", "128", "256"] {
        let config = require_ok(resolve(&schema, values(&[("KERNEL_STACK_SIZE_KB", value)])));
        require_ok(validate_kernel_configuration(&config, None));
    }
}

#[test]
fn authoring_and_build_use_the_same_architecture_constraints() {
    let schema = kernel_schema();
    for (name, target) in [
        ("ARCH_AARCH64", "aarch64-unknown-none-softfloat"),
        ("ARCH_RISCV64", "riscv64imac-unknown-none-elf"),
        ("ARCH_X86_64", "x86_64-unknown-none"),
    ] {
        let config = require_ok(resolve(&schema, values(&[(name, "y")])));
        require_ok(validate_kernel_configuration(&config, Some(target)));
        // Portable host tests deliberately compile a selected platform model.
        require_ok(validate_kernel_configuration(
            &config,
            Some("aarch64-apple-darwin"),
        ));
        if name != "ARCH_AARCH64" {
            assert!(
                validate_kernel_configuration(&config, Some("aarch64-unknown-none-softfloat"))
                    .is_err()
            );
        }
    }
}

#[test]
fn generated_configs_keep_only_the_selected_architecture_and_round_trip() {
    let schema = kernel_schema();
    let directory =
        std::env::temp_dir().join(format!("hyper-config-roundtrip-{}", std::process::id()));
    require_ok(fs::create_dir_all(&directory));
    for arch in ["aarch64", "riscv64", "x86_64"] {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join(format!("../../configs/qemu_{arch}_defconfig"));
        let config = require_ok(resolve(&schema, require_ok(load_fragment(&path))));
        require_ok(validate_kernel_configuration(&config, None));
        let output = directory.join(arch);
        require_ok(write_config(&output, &schema, &config));
        let text = require_ok(fs::read_to_string(&output));
        assert_eq!(
            text.lines()
                .filter(|line| line.contains("CONFIG_ARCH_"))
                .count(),
            1
        );
        assert!(!text.contains("CONFIG_MAX_KERNEL_STACKS"));
        assert!(!text.contains("CONFIG_ARM64_MAX_GIC_REDISTRIBUTOR_REGIONS"));
        assert!(text.contains("# Stack sizes"));
        if arch != "aarch64" {
            assert!(!text.contains("CONFIG_ARM64_"));
            assert!(!text.contains("CONFIG_RANDOMIZE_BASE"));
        }
        assert_eq!(
            require_ok(resolve(&schema, require_ok(load_fragment(&output)))),
            config
        );
    }
    require_ok(fs::remove_dir_all(directory));
}

#[test]
fn duplicate_assignments_do_not_silently_override_previous_values() {
    let path = std::env::temp_dir().join(format!("hyper-config-duplicates-{}", std::process::id()));
    for source in [
        "CONFIG_ARCH_AARCH64=y\nCONFIG_ARCH_AARCH64=n\n",
        "CONFIG_PARENT=y\n# CONFIG_PARENT is not set\n",
    ] {
        require_ok(fs::write(&path, source));
        assert!(load_fragment(&path).is_err());
    }
    require_ok(fs::remove_file(path));
}

#[test]
fn conjunctions_enforce_all_prerequisites_and_migration_drops_dependents() {
    let schema = schema(
        "config APP\n bool \"App\"\n default y\n depends on HELPER && OTHER\n\
         config HELPER\n bool \"Helper\"\n default y\n depends on CORE\n\
         config OTHER\n bool \"Other\"\n default y\n\
         config CORE\n bool \"Core\"\n default y\n",
    );
    let config = require_ok(resolve(&schema, values(&[("CORE", "n")])));
    assert_eq!(config.value("HELPER"), None);
    assert_eq!(config.value("APP"), None);
    assert!(resolve(&schema, values(&[("CORE", "n"), ("APP", "y")])).is_err());
    assert!(resolve(&schema, values(&[("OTHER", "n"), ("APP", "y")])).is_err());
    let (config, removed) = require_ok(resolve_existing(
        &schema,
        values(&[("HELPER", "n"), ("APP", "y")]),
    ));
    assert_eq!(config.value("HELPER"), Some("n"));
    assert_eq!(config.value("APP"), None);
    assert_eq!(removed, ["APP"]);
}
