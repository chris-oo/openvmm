// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Run one exact VMM test with native nextest inside one FVP invocation.

use crate::run_cargo_nextest_run::NextestProfile;
use crate::write_incubator_target_runner::FVP_SHARE_MANIFEST;
use crate::write_incubator_target_runner::IncubatorPlatform;
use crate::write_incubator_target_runner::fvp_share_inputs;
use flowey::node::prelude::*;
use std::collections::BTreeMap;
use std::path::Path;

const CONFIG: &str = "fvp-single-boot-nextest.toml";
const JUNIT: &str = "nextest-single-boot.xml";
const RESULT_FILE: &str = "session-result.json";
const RUNS_DIRECTORY: &str = "fvp-single-boot-runs";
const EXTRA_INPUTS: &[&str] = &[
    "cargo-nextest",
    "vmm_tests.tar.zst",
    ".config/nextest.toml",
    "Cargo.toml",
    "vmm_tests/vmm_tests/Cargo.toml",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReportedTestOutcome {
    Passed,
    Failed,
    NotRun,
    Unavailable,
}

#[derive(Serialize, Deserialize)]
pub struct SingleBootResults {
    pub run_directory: PathBuf,
    pub junit_xml: Option<PathBuf>,
    pub reported_test_outcome: ReportedTestOutcome,
    pub errors: Vec<String>,
}

impl SingleBootResults {
    pub(crate) fn test_results(&self) -> flowey_lib_common::run_cargo_nextest_run::TestResults {
        flowey_lib_common::run_cargo_nextest_run::TestResults {
            all_tests_passed: self.reported_test_outcome == ReportedTestOutcome::Passed
                && self.errors.is_empty(),
            junit_xml: self.junit_xml.clone(),
        }
    }
}

flowey_request! {
    pub struct Request {
        pub platform: IncubatorPlatform,
        /// Fixed optional guest inventory, available only after checked staging.
        pub cca_tdisp_guest: Option<ReadVar<SideEffect>>,
        pub incubator: ReadVar<PathBuf>,
        pub env: ReadVar<BTreeMap<String, String>>,
        pub content_dir: PathBuf,
        pub test_name: String,
        pub profile: NextestProfile,
        pub pre_run_deps: Vec<ReadVar<SideEffect>>,
        pub results: WriteVar<SingleBootResults>,
    }
}

new_simple_flow_node!(struct Node);

impl SimpleFlowNode for Node {
    type Request = Request;

    fn imports(_ctx: &mut ImportCtx<'_>) {}

    fn process_request(request: Request, ctx: &mut NodeCtx<'_>) -> anyhow::Result<()> {
        let Request {
            platform,
            cca_tdisp_guest,
            incubator,
            env,
            content_dir,
            test_name,
            profile,
            pre_run_deps,
            results,
        } = request;
        anyhow::ensure!(platform.is_fvp(), "single-boot execution requires FVP");
        crate::cca_tdisp_guest::validate_options(
            cca_tdisp_guest.is_some(),
            Some(platform),
            false,
            false,
        )?;
        anyhow::ensure!(
            matches!(ctx.platform(), FlowPlatform::Linux(_)),
            "FVP requires a Linux host"
        );
        single_test_filter(&test_name)?;
        let has_cca_tdisp_guest = cca_tdisp_guest.is_some();
        ctx.emit_rust_step("run exact VMM test in one FVP boot", |ctx| {
            cca_tdisp_guest.claim(ctx);
            pre_run_deps.claim(ctx);
            let incubator = incubator.claim(ctx);
            let env = env.claim(ctx);
            let results = results.claim(ctx);
            move |rt| {
                let source = fs_err::canonicalize(&content_dir)?;
                let run_directory = create_run_directory(&source.join(RUNS_DIRECTORY))?;
                let inputs = run_directory.join("inputs");
                let outputs = run_directory.join("outputs");
                let host_temp = run_directory.join("host-tmp");
                fs_err::create_dir(&outputs)?;
                fs_err::create_dir(&host_temp)?;
                prepare_inputs(&source, &inputs, profile, has_cca_tdisp_guest)?;
                let mut env = rt.read(env);
                configure_environment(&mut env, &inputs, &outputs, &host_temp);
                let incubator = rt.read(incubator);
                let nextest = inputs.join("cargo-nextest");
                let args = native_nextest_args(&test_name, profile)?;
                fs_err::write(
                    run_directory.join("invocation.json"),
                    serde_json::to_vec_pretty(&serde_json::json!({
                        "test_name": test_name,
                        "program": nextest,
                        "arguments": args,
                        "result_file": outputs.join(RESULT_FILE),
                    }))?,
                )?;
                log::info!("single-boot results: {}", run_directory.display());
                let run = {
                    let _dir = rt.sh.push_dir(&inputs);
                    flowey::shell_cmd!(
                        rt,
                        "{incubator} --guest-env TMPDIR=/tmp -- {nextest} {args...}"
                    )
                    .envs(env)
                    .run()
                };
                let mut outcome = assess_execution(
                    run_directory,
                    &outputs,
                    &test_name,
                    run.err()
                        .map(|error| format!("FVP invocation failed: {error:#}")),
                );
                log::info!("native JUnit reports: {:?}", outcome.reported_test_outcome);
                if let Err(error) = fs_err::write(
                    outcome.run_directory.join("single-boot-result.json"),
                    serde_json::to_vec_pretty(&outcome)?,
                ) {
                    outcome
                        .errors
                        .push(format!("cannot persist single-boot summary: {error}"));
                }
                for error in &outcome.errors {
                    log::error!("{error}");
                }
                rt.write(results, &outcome);
                Ok(())
            }
        });
        Ok(())
    }
}

fn assess_execution(
    run_directory: PathBuf,
    outputs: &Path,
    test_name: &str,
    invocation_error: Option<String>,
) -> SingleBootResults {
    let mut errors: Vec<_> = invocation_error.into_iter().collect();
    let mut junit_xml = None;
    let mut reported_test_outcome = ReportedTestOutcome::Unavailable;
    match registered_junit(outputs) {
        Ok(path) => match read_test_outcome(&path, test_name) {
            Ok(outcome) => {
                reported_test_outcome = outcome;
                junit_xml = Some(path);
            }
            Err(error) => errors.push(format!("invalid native JUnit: {error:#}")),
        },
        Err(error) => errors.push(format!("native JUnit unavailable: {error:#}")),
    }
    if reported_test_outcome != ReportedTestOutcome::Passed {
        errors.push(format!("requested test outcome: {reported_test_outcome:?}"));
    }
    SingleBootResults {
        run_directory,
        junit_xml,
        reported_test_outcome,
        errors,
    }
}

/// Only an exact, unambiguous name in the `tests` binary is accepted.
pub fn single_test_filter(name: &str) -> anyhow::Result<String> {
    anyhow::ensure!(
        !name.is_empty()
            && name.split("::").all(|part| {
                !part.is_empty() && part.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
            }),
        "single-boot test name must contain only identifiers separated by ::"
    );
    Ok(format!("binary(=tests) & test(={name})"))
}

fn create_run_directory(parent: &Path) -> anyhow::Result<PathBuf> {
    fs_err::create_dir_all(parent)?;
    let time = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_nanos();
    let path = parent.join(format!("single-boot-{}-{time}", std::process::id()));
    // Exclusive creation rejects collisions rather than reusing stale results.
    fs_err::create_dir(&path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs_err::set_permissions(&path, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(path)
}

fn prepare_inputs(
    source: &Path,
    destination: &Path,
    profile: NextestProfile,
    cca_tdisp_guest: bool,
) -> anyhow::Result<()> {
    crate::cca_tdisp_guest::validate_staged(source, cca_tdisp_guest)?;
    copy_inputs(
        source,
        destination,
        profile,
        &fvp_share_inputs(cca_tdisp_guest),
    )?;
    if cca_tdisp_guest {
        crate::cca_tdisp_guest::validate_private_copy(destination)?;
        crate::cca_tdisp_guest::validate_staged(source, true)?;
    }
    Ok(())
}

fn copy_inputs(
    source: &Path,
    destination: &Path,
    profile: NextestProfile,
    share_inputs: &[&str],
) -> anyhow::Result<()> {
    fs_err::create_dir(destination)?;
    let names: Vec<_> = share_inputs.iter().chain(EXTRA_INPUTS).copied().collect();
    let mut hashes = Vec::new();
    for name in &names {
        let path = source.join(name);
        anyhow::ensure!(
            fs_err::symlink_metadata(&path)?.is_file(),
            "input is not a regular file: {name}"
        );
        hashes.push(crate::cca_artifacts::sha256_file(&path)?);
    }
    for (name, hash) in names.iter().zip(&hashes) {
        let target = destination.join(name);
        fs_err::create_dir_all(target.parent().context("input has no parent")?)?;
        fs_err::copy(source.join(name), &target)?;
        crate::cca_artifacts::verify_sha256(&target, hash, name)?;
    }
    for (name, hash) in names.iter().zip(&hashes) {
        crate::cca_artifacts::verify_sha256(&source.join(name), hash, name)?;
    }
    let config = derived_config(
        &fs_err::read_to_string(destination.join(".config/nextest.toml"))?,
        profile,
    )?;
    fs_err::write(destination.join(CONFIG), config)?;
    destination.join("cargo-nextest").make_executable()?;
    // The source config is retained for diagnosis, but only the derived config
    // is exposed to the L1 runner. No directory is copied wholesale.
    let mut inventory: Vec<_> = names
        .into_iter()
        .filter(|name| *name != ".config/nextest.toml")
        .collect();
    inventory.push(CONFIG);
    fs_err::write(
        destination.join(FVP_SHARE_MANIFEST),
        serde_json::to_vec(&inventory)?,
    )?;
    fs_err::create_dir(destination.join("test_results"))?;
    Ok(())
}

fn derived_config(text: &str, profile: NextestProfile) -> anyhow::Result<String> {
    let mut config: toml_edit::DocumentMut = text.parse()?;
    let junit = &mut config["profile"][profile.as_str()]["junit"];
    junit["path"] = toml_edit::value(format!("/share/test_results/{JUNIT}"));
    junit["store-success-output"] = toml_edit::value(true);
    junit["store-failure-output"] = toml_edit::value(true);
    Ok(config.to_string())
}

fn configure_environment(
    env: &mut BTreeMap<String, String>,
    inputs: &Path,
    outputs: &Path,
    host_temp: &Path,
) {
    env.retain(|key, _| !(key.starts_with("CARGO_TARGET_") && key.ends_with("_RUNNER")));
    env.insert("INCUBATOR_SHARE".into(), inputs.display().to_string());
    env.insert("INCUBATOR_OUTPUT_DIR".into(), outputs.display().to_string());
    env.insert(
        "INCUBATOR_FVP_RESULT_FILE".into(),
        outputs.join(RESULT_FILE).display().to_string(),
    );
    env.insert("INCUBATOR_GUEST_CURRENT_DIR".into(), "/share".into());
    env.insert("INCUBATOR_GUEST_PIPETTE".into(), "/share/pipette".into());
    env.insert("VMM_TESTS_CONTENT_DIR".into(), inputs.display().to_string());
    env.insert(
        "TEST_OUTPUT_PATH".into(),
        inputs.join("test_results").display().to_string(),
    );
    env.insert("TMPDIR".into(), host_temp.display().to_string());
}

fn native_nextest_args(name: &str, profile: NextestProfile) -> anyhow::Result<Vec<String>> {
    Ok([
        "nextest".into(),
        "run".into(),
        "--profile".into(),
        profile.as_str().into(),
        "--config-file".into(),
        format!("/share/{CONFIG}"),
        "--workspace-remap".into(),
        "/share".into(),
        "--archive-file".into(),
        "/share/vmm_tests.tar.zst".into(),
        "--filter-expr".into(),
        single_test_filter(name)?,
        "--test-threads".into(),
        "1".into(),
        "--retries".into(),
        "0".into(),
        "--no-tests".into(),
        "fail".into(),
    ]
    .into())
}

fn registered_junit(outputs: &Path) -> anyhow::Result<PathBuf> {
    let path = outputs.join(RESULT_FILE);
    let metadata = fs_err::symlink_metadata(&path)?;
    anyhow::ensure!(
        metadata.is_file() && metadata.len() <= 1024 * 1024,
        "invalid session result file"
    );
    let report: serde_json::Value = serde_json::from_reader(fs_err::File::open(path)?)?;
    anyhow::ensure!(
        report["schema_version"] == 2,
        "unsupported session result schema"
    );
    let run_id = report["run_id"]
        .as_str()
        .context("session result has no run ID")?;
    anyhow::ensure!(
        run_id.len() == 64 && run_id.bytes().all(|b| b.is_ascii_hexdigit()),
        "invalid session run ID"
    );
    let registered = PathBuf::from(
        report["run_output_dir"]
            .as_str()
            .context("session result has no registered output")?,
    );
    anyhow::ensure!(
        registered == outputs.join(format!("fvp-{run_id}")),
        "session result points outside this invocation"
    );
    anyhow::ensure!(
        fs_err::symlink_metadata(&registered)?.is_dir(),
        "invalid registered output directory"
    );
    let directory = registered.join("test_results");
    anyhow::ensure!(
        fs_err::symlink_metadata(&directory)?.is_dir(),
        "native results not safely preserved"
    );
    let junit = directory.join(JUNIT);
    anyhow::ensure!(
        fs_err::symlink_metadata(&junit)?.is_file(),
        "native JUnit is not a regular file"
    );
    Ok(junit)
}

fn read_test_outcome(path: &Path, expected: &str) -> anyhow::Result<ReportedTestOutcome> {
    anyhow::ensure!(
        fs_err::metadata(path)?.len() <= 16 * 1024 * 1024,
        "JUnit exceeds 16 MiB"
    );
    parse_test_outcome(&fs_err::read_to_string(path)?, expected)
}

fn parse_test_outcome(text: &str, expected: &str) -> anyhow::Result<ReportedTestOutcome> {
    let document = roxmltree::Document::parse_with_options(
        text,
        roxmltree::ParsingOptions {
            allow_dtd: false,
            nodes_limit: 100_000,
        },
    )?;
    let root = document.root_element();
    anyhow::ensure!(root.has_tag_name("testsuites"), "missing testsuites root");
    let count = |name| -> anyhow::Result<usize> {
        Ok(root
            .attribute(name)
            .with_context(|| format!("missing JUnit {name}"))?
            .parse()?)
    };
    let tests = count("tests")?;
    let failures = count("failures")?;
    let errors = count("errors")?;
    let cases: Vec<_> = document
        .descendants()
        .filter(|node| node.has_tag_name("testcase"))
        .collect();
    anyhow::ensure!(
        tests == cases.len() && tests <= 1,
        "JUnit does not describe one exact test"
    );
    if cases.is_empty() {
        anyhow::ensure!(failures == 0 && errors == 0, "inconsistent empty JUnit");
        return Ok(ReportedTestOutcome::NotRun);
    }
    let case = cases[0];
    let suite = case.parent().context("testcase has no suite")?;
    anyhow::ensure!(
        suite.has_tag_name("testsuite")
            && suite.parent() == Some(root)
            && suite.attribute("name") == Some("vmm_tests::tests")
            && case.attribute("name") == Some(expected),
        "JUnit does not identify the requested tests-binary test"
    );
    let has = |tag| case.children().any(|node| node.has_tag_name(tag));
    let failed = has("failure");
    let errored = has("error");
    let skipped = has("skipped");
    anyhow::ensure!(
        failures == usize::from(failed) && errors == usize::from(errored),
        "inconsistent JUnit failure counts"
    );
    anyhow::ensure!(
        !has("flakyFailure") && !has("flakyError") && !has("rerunFailure") && !has("rerunError"),
        "single-boot qualification must not use retries"
    );
    Ok(if skipped {
        ReportedTestOutcome::NotRun
    } else if failed || errored {
        ReportedTestOutcome::Failed
    } else {
        ReportedTestOutcome::Passed
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_with_tracing::test;

    fn report(name: &str, status: &str, failures: usize, errors: usize) -> String {
        format!(
            "<testsuites tests=\"1\" failures=\"{failures}\" errors=\"{errors}\"><testsuite name=\"vmm_tests::tests\"><testcase name=\"{name}\">{status}</testcase></testsuite></testsuites>"
        )
    }

    #[test]
    fn exact_filter_cannot_inject_a_broader_selection() {
        assert_eq!(
            single_test_filter("aarch64_exclusive::case").unwrap(),
            "binary(=tests) & test(=aarch64_exclusive::case)"
        );
        for bad in [
            "",
            "::case",
            "case::",
            "case)",
            "case | all()",
            "case:name",
            "case\n",
        ] {
            assert!(single_test_filter(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn native_arguments_reject_empty_runs_and_retries() {
        let args = native_nextest_args("case", NextestProfile::Default).unwrap();
        assert!(args.windows(2).any(|pair| pair == ["--no-tests", "fail"]));
        assert!(args.windows(2).any(|pair| pair == ["--retries", "0"]));
        assert!(args.windows(2).any(|pair| pair == ["--test-threads", "1"]));
        assert!(
            !args
                .iter()
                .any(|arg| arg == "--run-ignored" || arg == "--list")
        );
    }

    #[test]
    fn junit_requires_the_exact_non_skipped_completed_test() {
        assert_eq!(
            parse_test_outcome(&report("case", "", 0, 0), "case").unwrap(),
            ReportedTestOutcome::Passed
        );
        assert_eq!(
            parse_test_outcome(&report("case", "<failure/>", 1, 0), "case").unwrap(),
            ReportedTestOutcome::Failed
        );
        assert_eq!(
            parse_test_outcome(&report("case", "<error/>", 0, 1), "case").unwrap(),
            ReportedTestOutcome::Failed
        );
        assert_eq!(
            parse_test_outcome(&report("case", "<skipped/>", 0, 0), "case").unwrap(),
            ReportedTestOutcome::NotRun
        );
        assert_eq!(
            parse_test_outcome(
                "<testsuites tests=\"0\" failures=\"0\" errors=\"0\"/>",
                "case"
            )
            .unwrap(),
            ReportedTestOutcome::NotRun
        );
        for bad in [
            report("other", "", 0, 0),
            report("case", "", 1, 0),
            report("case", "<flakyFailure/>", 0, 0),
            report("case", "", 0, 0).replace("vmm_tests::tests", "other::tests"),
            report("case", "", 0, 0)
                .trim_end_matches("</testsuites>")
                .to_owned(),
            format!("{}<extra/>", report("case", "", 0, 0)),
            report("case", "", 0, 0).replace("tests=\"1\"", "tests=\"2\""),
        ] {
            assert!(parse_test_outcome(&bad, "case").is_err(), "{bad}");
        }
    }

    #[test]
    fn derived_config_does_not_change_source_policy() {
        let text = "[profile.default]\nfail-fast = true\n";
        let result: toml_edit::DocumentMut = derived_config(text, NextestProfile::Default)
            .unwrap()
            .parse()
            .unwrap();
        assert_eq!(
            result["profile"]["default"]["fail-fast"].as_bool(),
            Some(true)
        );
        assert_eq!(
            result["profile"]["default"]["junit"]["path"].as_str(),
            Some("/share/test_results/nextest-single-boot.xml")
        );
        assert!(!text.contains("junit"));
    }

    #[test]
    fn private_inputs_and_manifest_do_not_reuse_prior_outputs() {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("source");
        fs_err::create_dir(&source).unwrap();
        for name in fvp_share_inputs(false).iter().chain(EXTRA_INPUTS) {
            let path = source.join(name);
            fs_err::create_dir_all(path.parent().unwrap()).unwrap();
            fs_err::write(
                path,
                if *name == ".config/nextest.toml" {
                    "[profile.default]\n"
                } else {
                    "fixture"
                },
            )
            .unwrap();
        }
        let a = create_run_directory(&root.path().join("outputs")).unwrap();
        let b = create_run_directory(&root.path().join("outputs")).unwrap();
        assert_ne!(a, b);
        prepare_inputs(&source, &a.join("inputs"), NextestProfile::Default, false).unwrap();
        prepare_inputs(&source, &b.join("inputs"), NextestProfile::Default, false).unwrap();
        assert!(
            prepare_inputs(&source, &a.join("inputs"), NextestProfile::Default, false).is_err()
        );
        let inventory: Vec<String> = serde_json::from_slice(
            &fs_err::read(a.join("inputs").join(FVP_SHARE_MANIFEST)).unwrap(),
        )
        .unwrap();
        assert!(inventory.contains(&"cargo-nextest".into()));
        assert!(inventory.contains(&"vmm_tests.tar.zst".into()));
        assert!(
            !inventory
                .iter()
                .any(|path| path.starts_with("test_results"))
        );
        assert_eq!(
            fs_err::read_to_string(source.join(".config/nextest.toml")).unwrap(),
            "[profile.default]\n"
        );
    }

    #[test]
    fn environment_uses_private_inputs_and_no_nested_runner() {
        let mut env = BTreeMap::from([(
            "CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_RUNNER".into(),
            "host-incubator".into(),
        )]);
        configure_environment(
            &mut env,
            Path::new("/owned/inputs"),
            Path::new("/owned/outputs"),
            Path::new("/owned/host-tmp"),
        );
        assert!(!env.keys().any(|key| key.ends_with("_RUNNER")));
        assert_eq!(env["INCUBATOR_SHARE"], "/owned/inputs");
        assert_eq!(env["TEST_OUTPUT_PATH"], "/owned/inputs/test_results");
        assert_eq!(
            env["INCUBATOR_FVP_RESULT_FILE"],
            "/owned/outputs/session-result.json"
        );
        assert_eq!(env["TMPDIR"], "/owned/host-tmp");
    }

    #[test]
    fn private_optional_guest_inventory_copies_only_controlled_inputs() {
        let root = tempfile::tempdir_in(std::env::current_dir().unwrap()).unwrap();
        let source = root.path().join("source");
        for name in fvp_share_inputs(true).iter().chain(EXTRA_INPUTS) {
            let path = source.join(name);
            fs_err::create_dir_all(path.parent().unwrap()).unwrap();
            fs_err::write(
                &path,
                if *name == ".config/nextest.toml" {
                    "[profile.default]\n"
                } else {
                    "fixture"
                },
            )
            .unwrap();
        }
        fs_err::write(
            source.join("cca-tdisp-guest/provenance.json"),
            b"not shared",
        )
        .unwrap();
        fs_err::write(source.join("old-output"), b"not shared").unwrap();
        let destination = root.path().join("inputs");
        copy_inputs(
            &source,
            &destination,
            NextestProfile::Default,
            &fvp_share_inputs(true),
        )
        .unwrap();
        let inventory: Vec<String> =
            serde_json::from_slice(&fs_err::read(destination.join(FVP_SHARE_MANIFEST)).unwrap())
                .unwrap();
        for name in crate::cca_tdisp_guest::SHARE_INPUTS {
            assert!(inventory.iter().any(|entry| entry == name));
            assert_eq!(fs_err::read(destination.join(name)).unwrap(), b"fixture");
        }
        assert!(!destination.join("cca-tdisp-guest/provenance.json").exists());
        assert!(!destination.join("old-output").exists());
        fs_err::write(source.join("cca-tdisp-guest/Image"), b"later source edit").unwrap();
        assert_eq!(
            fs_err::read(destination.join("cca-tdisp-guest/Image")).unwrap(),
            b"fixture"
        );
        fs_err::remove_file(source.join("cca-tdisp-guest/initrd")).unwrap();
        assert!(
            copy_inputs(
                &source,
                &root.path().join("missing-input"),
                NextestProfile::Default,
                &fvp_share_inputs(true)
            )
            .is_err()
        );
        for enabled in [false, true] {
            assert!(
                prepare_inputs(
                    &source,
                    &root.path().join("invalid-input"),
                    NextestProfile::Default,
                    enabled
                )
                .is_err()
            );
        }
    }

    #[test]
    fn shared_environment_cleanup_does_not_remove_private_runs() {
        let root = tempfile::tempdir().unwrap();
        let run = create_run_directory(&root.path().join(RUNS_DIRECTORY)).unwrap();
        fs_err::write(run.join("evidence.json"), b"current run").unwrap();
        for cleared in ["test_results", "temp"] {
            let directory = root.path().join(cleared);
            fs_err::create_dir(&directory).unwrap();
            fs_err::write(directory.join("old"), b"old").unwrap();
            fs_err::remove_dir_all(&directory).unwrap();
            fs_err::create_dir(&directory).unwrap();
        }
        assert_eq!(
            fs_err::read(run.join("evidence.json")).unwrap(),
            b"current run"
        );
    }

    fn saved_report(outputs: &Path, content: &str) -> PathBuf {
        let id = "a".repeat(64);
        let registered = outputs.join(format!("fvp-{id}"));
        fs_err::create_dir_all(registered.join("test_results")).unwrap();
        let junit = registered.join("test_results").join(JUNIT);
        fs_err::write(&junit, content).unwrap();
        fs_err::write(
            outputs.join(RESULT_FILE),
            serde_json::to_vec(&serde_json::json!({
                "schema_version": 2, "run_id": id, "run_output_dir": registered,
            }))
            .unwrap(),
        )
        .unwrap();
        junit
    }

    #[test]
    fn passed_test_and_failed_shutdown_are_separate_outcomes() {
        let root = tempfile::tempdir().unwrap();
        let junit = saved_report(root.path(), &report("case", "", 0, 0));
        let result = assess_execution(
            root.path().to_owned(),
            root.path(),
            "case",
            Some("model shutdown failed".into()),
        );
        assert_eq!(result.reported_test_outcome, ReportedTestOutcome::Passed);
        assert_eq!(result.junit_xml, Some(junit));
        assert_eq!(result.errors, ["model shutdown failed"]);
        let published = result.test_results();
        assert!(!published.all_tests_passed);
        assert_eq!(published.junit_xml, result.junit_xml);
    }

    #[test]
    fn publication_requires_both_native_and_session_success() {
        for outcome in [
            ReportedTestOutcome::Passed,
            ReportedTestOutcome::Failed,
            ReportedTestOutcome::NotRun,
            ReportedTestOutcome::Unavailable,
        ] {
            for session_failed in [false, true] {
                let result = SingleBootResults {
                    run_directory: "run".into(),
                    junit_xml: Some("run/native.xml".into()),
                    reported_test_outcome: outcome,
                    errors: if session_failed {
                        vec!["session failed".into()]
                    } else {
                        vec![]
                    },
                };
                let published = result.test_results();
                assert_eq!(
                    published.all_tests_passed,
                    outcome == ReportedTestOutcome::Passed && !session_failed
                );
                assert_eq!(published.junit_xml, result.junit_xml);
            }
        }
    }

    #[test]
    fn stale_or_malformed_evidence_cannot_pass() {
        let root = tempfile::tempdir().unwrap();
        let old = root.path().join("old");
        saved_report(&old, &report("case", "", 0, 0));
        let current = root.path().join("current");
        fs_err::create_dir(&current).unwrap();
        let result = assess_execution(current.clone(), &current, "case", None);
        assert_eq!(
            result.reported_test_outcome,
            ReportedTestOutcome::Unavailable
        );
        assert!(!result.errors.is_empty());
        fs_err::copy(old.join(RESULT_FILE), current.join(RESULT_FILE)).unwrap();
        assert!(
            registered_junit(&current)
                .unwrap_err()
                .to_string()
                .contains("outside this invocation")
        );
        let junit = saved_report(&current, "<testsuites tests=\"1\">");
        let result = assess_execution(current.clone(), &current, "case", None);
        assert_eq!(
            result.reported_test_outcome,
            ReportedTestOutcome::Unavailable
        );
        assert!(result.junit_xml.is_none());
        assert!(!result.errors.is_empty());
        fs_err::remove_file(junit).unwrap();
        assert!(registered_junit(&current).is_err());
    }

    #[test]
    fn mixed_or_ignored_cases_do_not_qualify_the_requested_test() {
        let mixed = "<testsuites tests=\"2\" failures=\"0\" errors=\"0\"><testsuite name=\"vmm_tests::tests\"><testcase name=\"case\"><skipped/></testcase><testcase name=\"other\"/></testsuite></testsuites>";
        assert!(parse_test_outcome(mixed, "case").is_err());
        let root = tempfile::tempdir().unwrap();
        saved_report(root.path(), &report("case", "<skipped/>", 0, 0));
        let result = assess_execution(root.path().to_owned(), root.path(), "case", None);
        assert_eq!(result.reported_test_outcome, ReportedTestOutcome::NotRun);
        assert!(!result.errors.is_empty());
    }

    #[test]
    #[cfg(unix)]
    fn result_symlinks_are_rejected() {
        let root = tempfile::tempdir().unwrap();
        let junit = saved_report(root.path(), &report("case", "", 0, 0));
        fs_err::remove_file(&junit).unwrap();
        std::os::unix::fs::symlink(root.path().join(RESULT_FILE), &junit).unwrap();
        assert!(registered_junit(root.path()).is_err());
    }
}
