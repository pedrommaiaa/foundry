//! Test command
use crate::{
    cmd::{
        forge::{build::BuildArgs, run::RunArgs},
        Cmd,
    },
    compile::ProjectCompiler,
    opts::evm::EvmArgs,
    utils,
    utils::FoundryPathExt,
};
use ansi_term::Colour;
use clap::{AppSettings, Parser};
use ethers::solc::FileFilter;
use forge::{
    decode::decode_console_logs,
    executor::opts::EvmOpts,
    gas_report::GasReport,
    trace::{
        identifier::{EtherscanIdentifier, LocalTraceIdentifier},
        CallTraceDecoder, TraceKind,
    },
    MultiContractRunner, MultiContractRunnerBuilder, SuiteResult, TestFilter, TestKind,
};
use foundry_config::{figment::Figment, Config};
use regex::Regex;
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::mpsc::channel,
    thread,
    time::Duration,
};

#[derive(Debug, Clone, Parser)]
pub struct Filter {
    /// Only run test functions matching the specified regex pattern.
    ///
    /// Deprecated: See --match-test
    #[clap(long = "match", short = 'm')]
    pub pattern: Option<regex::Regex>,

    /// Only run test functions matching the specified regex pattern.
    #[clap(long = "match-test", alias = "mt", conflicts_with = "pattern")]
    pub test_pattern: Option<regex::Regex>,

    /// Only run test functions that do not match the specified regex pattern.
    #[clap(long = "no-match-test", alias = "nmt", conflicts_with = "pattern")]
    pub test_pattern_inverse: Option<regex::Regex>,

    /// Only run tests in contracts matching the specified regex pattern.
    #[clap(long = "match-contract", alias = "mc", conflicts_with = "pattern")]
    pub contract_pattern: Option<regex::Regex>,

    /// Only run tests in contracts that do not match the specified regex pattern.
    #[clap(long = "no-match-contract", alias = "nmc", conflicts_with = "pattern")]
    pub contract_pattern_inverse: Option<regex::Regex>,

    /// Only run tests in source files matching the specified glob pattern.
    #[clap(long = "match-path", alias = "mp", conflicts_with = "pattern")]
    pub path_pattern: Option<globset::Glob>,

    /// Only run tests in source files that do not match the specified glob pattern.
    #[clap(
        name = "no-match-path",
        long = "no-match-path",
        alias = "nmp",
        conflicts_with = "pattern"
    )]
    pub path_pattern_inverse: Option<globset::Glob>,
}

impl FileFilter for Filter {
    /// Returns true if the file regex pattern match the `file`
    ///
    /// If no file regex is set this returns true if the file ends with `.t.sol`, see
    /// [FoundryPathExr::is_sol_test()]
    fn is_match(&self, file: &Path) -> bool {
        if let Some(file) = file.as_os_str().to_str() {
            if let Some(ref glob) = self.path_pattern {
                return glob.compile_matcher().is_match(file)
            }
            if let Some(ref glob) = self.path_pattern_inverse {
                return !glob.compile_matcher().is_match(file)
            }
        }
        file.is_sol_test()
    }
}

impl TestFilter for Filter {
    fn matches_test(&self, test_name: impl AsRef<str>) -> bool {
        let mut ok = true;
        let test_name = test_name.as_ref();
        // Handle the deprecated option match
        if let Some(re) = &self.pattern {
            ok &= re.is_match(test_name);
        }
        if let Some(re) = &self.test_pattern {
            ok &= re.is_match(test_name);
        }
        if let Some(re) = &self.test_pattern_inverse {
            ok &= !re.is_match(test_name);
        }
        ok
    }

    fn matches_contract(&self, contract_name: impl AsRef<str>) -> bool {
        let mut ok = true;
        let contract_name = contract_name.as_ref();
        if let Some(re) = &self.contract_pattern {
            ok &= re.is_match(contract_name);
        }
        if let Some(re) = &self.contract_pattern_inverse {
            ok &= !re.is_match(contract_name);
        }
        ok
    }

    fn matches_path(&self, path: impl AsRef<str>) -> bool {
        let mut ok = true;
        let path = path.as_ref();
        if let Some(ref glob) = self.path_pattern {
            ok &= glob.compile_matcher().is_match(path);
        }
        if let Some(ref glob) = self.path_pattern_inverse {
            ok &= !glob.compile_matcher().is_match(path);
        }
        ok
    }
}

// Loads project's figment and merges the build cli arguments into it
foundry_config::impl_figment_convert!(TestArgs, opts, evm_opts);

#[derive(Debug, Clone, Parser)]
#[clap(global_setting = AppSettings::DeriveDisplayOrder)]
pub struct TestArgs {
    #[clap(flatten)]
    filter: Filter,

    /// Run a test in the debugger.
    ///
    /// The argument passed to this flag is the name of the test function you want to run, and it
    /// works the same as --match-test.
    ///
    /// If more than one test matches your specified criteria, you must add additional filters
    /// until only one test is found (see --match-contract and --match-path).
    ///
    /// The matching test will be opened in the debugger regardless of the outcome of the test.
    ///
    /// If the matching test is a fuzz test, then it will open the debugger on the first failure
    /// case.
    /// If the fuzz test does not fail, it will open the debugger on the last fuzz case.
    ///
    /// For more fine-grained control of which fuzz case is run, see forge run.
    #[clap(long, value_name = "TEST FUNCTION")]
    debug: Option<Regex>,

    /// Print a gas report.
    #[clap(long, env = "FORGE_GAS_REPORT")]
    gas_report: bool,

    /// Force the process to exit with code 0, even if the tests fail.
    #[clap(long, env = "FORGE_ALLOW_FAILURE")]
    allow_failure: bool,

    /// Output test results in JSON format.
    #[clap(long, short)]
    json: bool,

    #[clap(flatten, next_help_heading = "EVM OPTIONS")]
    evm_opts: EvmArgs,

    #[clap(flatten, next_help_heading = "BUILD OPTIONS")]
    opts: BuildArgs,
}

impl TestArgs {
    /// Returns the flattened [`BuildArgs`]
    pub fn build_args(&self) -> &BuildArgs {
        &self.opts
    }

    /// Returns the flattened [`Filter`] arguments
    pub fn filter(&self) -> &Filter {
        &self.filter
    }

    /// Returns the currently configured [Config] and the extracted [EvmOpts] from that config
    pub fn config_and_evm_opts(&self) -> eyre::Result<(Config, EvmOpts)> {
        // merge all configs
        let figment: Figment = self.into();
        let evm_opts = figment.extract()?;
        let config = Config::from_provider(figment).sanitized();
        Ok((config, evm_opts))
    }
}

impl Cmd for TestArgs {
    type Output = TestOutcome;

    fn run(self) -> eyre::Result<Self::Output> {
        custom_run(self, true)
    }
}

/// The result of a single test
#[derive(Debug, Clone)]
pub struct Test {
    /// The identifier of the artifact/contract in the form of `<artifact file name>:<contract
    /// name>`
    pub artifact_id: String,
    /// The signature of the solidity test
    pub signature: String,
    /// Result of the executed solidity test
    pub result: forge::TestResult,
}

impl Test {
    pub fn gas_used(&self) -> u64 {
        self.result.kind.gas_used().gas()
    }

    /// Returns the contract name of the artifact id
    pub fn contract_name(&self) -> &str {
        utils::get_contract_name(&self.artifact_id)
    }

    /// Returns the file name of the artifact id
    pub fn file_name(&self) -> &str {
        utils::get_file_name(&self.artifact_id)
    }
}

/// Represents the bundled results of all tests
pub struct TestOutcome {
    /// Whether failures are allowed
    pub allow_failure: bool,
    /// Results for each suite of tests `contract -> SuiteResult`
    pub results: BTreeMap<String, SuiteResult>,
}

impl TestOutcome {
    fn new(results: BTreeMap<String, SuiteResult>, allow_failure: bool) -> Self {
        Self { results, allow_failure }
    }

    /// Iterator over all succeeding tests and their names
    pub fn successes(&self) -> impl Iterator<Item = (&String, &forge::TestResult)> {
        self.tests().filter(|(_, t)| t.success)
    }

    /// Iterator over all failing tests and their names
    pub fn failures(&self) -> impl Iterator<Item = (&String, &forge::TestResult)> {
        self.tests().filter(|(_, t)| !t.success)
    }

    /// Iterator over all tests and their names
    pub fn tests(&self) -> impl Iterator<Item = (&String, &forge::TestResult)> {
        self.results.values().flat_map(|SuiteResult { test_results, .. }| test_results.iter())
    }

    /// Returns an iterator over all `Test`
    pub fn into_tests(self) -> impl Iterator<Item = Test> {
        self.results
            .into_iter()
            .flat_map(|(file, SuiteResult { test_results, .. })| {
                test_results.into_iter().map(move |t| (file.clone(), t))
            })
            .map(|(artifact_id, (signature, result))| Test { artifact_id, signature, result })
    }

    /// Checks if there are any failures and failures are disallowed
    pub fn ensure_ok(&self) -> eyre::Result<()> {
        if !self.allow_failure {
            let failures = self.failures().count();
            if failures > 0 {
                println!();
                println!("Failed tests:");
                for (name, result) in self.failures() {
                    short_test_result(name, result);
                }
                println!();

                let successes = self.successes().count();
                println!(
                    "Encountered a total of {} failing tests, {} tests succeeded",
                    Colour::Red.paint(failures.to_string()),
                    Colour::Green.paint(successes.to_string())
                );
                std::process::exit(1);
            }
        }
        Ok(())
    }

    pub fn duration(&self) -> Duration {
        self.results
            .values()
            .fold(Duration::ZERO, |acc, SuiteResult { duration, .. }| acc + *duration)
    }

    pub fn summary(&self) -> String {
        let failed = self.failures().count();
        let result =
            if failed == 0 { Colour::Green.paint("ok") } else { Colour::Red.paint("FAILED") };
        format!(
            "Test result: {}. {} passed; {} failed; finished in {:.2?}",
            result,
            self.successes().count(),
            failed,
            self.duration()
        )
    }
}

fn short_test_result(name: &str, result: &forge::TestResult) {
    let status = if result.success {
        Colour::Green.paint("[PASS]")
    } else {
        let txt = match (&result.reason, &result.counterexample) {
            (Some(ref reason), Some(ref counterexample)) => {
                format!("[FAIL. Reason: {}. Counterexample: {}]", reason, counterexample)
            }
            (None, Some(ref counterexample)) => {
                format!("[FAIL. Counterexample: {}]", counterexample)
            }
            (Some(ref reason), None) => {
                format!("[FAIL. Reason: {}]", reason)
            }
            (None, None) => "[FAIL]".to_string(),
        };

        Colour::Red.paint(txt)
    };

    println!("{} {} {}", status, name, result.kind.gas_used());
}

pub fn custom_run(mut args: TestArgs, include_fuzz_tests: bool) -> eyre::Result<TestOutcome> {
    // Merge all configs
    let (config, mut evm_opts) = args.config_and_evm_opts()?;

    // Setup the fuzzer
    // TODO: Add CLI Options to modify the persistence
    let cfg = proptest::test_runner::Config {
        failure_persistence: None,
        cases: config.fuzz_runs,
        max_local_rejects: config.fuzz_max_local_rejects,
        max_global_rejects: config.fuzz_max_global_rejects,
        ..Default::default()
    };
    let fuzzer = proptest::test_runner::TestRunner::new(cfg);

    // Set up the project
    let project = config.project()?;
    let compiler = ProjectCompiler::default();
    let output = if config.sparse_mode {
        compiler.compile_sparse(&project, args.filter.clone())
    } else {
        compiler.compile(&project)
    }?;

    // Determine print verbosity and executor verbosity
    let verbosity = evm_opts.verbosity;
    if args.gas_report && evm_opts.verbosity < 3 {
        evm_opts.verbosity = 3;
    }

    // Prepare the test builder
    let evm_spec = crate::utils::evm_spec(&config.evm_version);
    let mut runner = MultiContractRunnerBuilder::default()
        .fuzzer(fuzzer)
        .initial_balance(evm_opts.initial_balance)
        .evm_spec(evm_spec)
        .sender(evm_opts.sender)
        .with_fork(utils::get_fork(&evm_opts, &config.rpc_storage_caching))
        .build(project.paths.root, output, evm_opts)?;

    if args.debug.is_some() {
        args.filter.test_pattern = args.debug;
        match runner.count_filtered_tests(&args.filter) {
                1 => {
                    // Run the test
                    let results = runner.test(&args.filter, None, true)?;

                    // Get the result of the single test
                    let (id, sig, test_kind, counterexample) = results.iter().map(|(id, SuiteResult{ test_results, .. })| {
                        let (sig, result) = test_results.iter().next().unwrap();

                        (id.clone(), sig.clone(), result.kind.clone(), result.counterexample.clone())
                    }).next().unwrap();

                    // Build debugger args if this is a fuzz test
                    let sig = match test_kind {
                        TestKind::Fuzz(cases) => {
                            if let Some(counterexample) = counterexample {
                                counterexample.calldata.to_string()
                            } else {
                                cases.cases().first().expect("no fuzz cases run").calldata.to_string()
                            }
                        },
                        _ => sig,
                    };

                    // Run the debugger
                    let debugger = RunArgs {
                        path: PathBuf::from(runner.source_paths.get(&id).unwrap()),
                        target_contract: Some(utils::get_contract_name(&id).to_string()),
                        sig,
                        args: Vec::new(),
                        debug: true,
                        opts: args.opts,
                        evm_opts: args.evm_opts,
                    };
                    debugger.run()?;

                    Ok(TestOutcome::new(results, args.allow_failure))
                }
                n =>
                    Err(
                    eyre::eyre!("{} tests matched your criteria, but exactly 1 test must match in order to run the debugger.\n
                        \n
                        Use --match-contract and --match-path to further limit the search.", n))
            }
    } else {
        let TestArgs { filter, .. } = args;
        test(
            config,
            runner,
            verbosity,
            filter,
            args.json,
            args.allow_failure,
            include_fuzz_tests,
            args.gas_report,
        )
    }
}

/// Runs all the tests
#[allow(clippy::too_many_arguments)]
fn test(
    config: Config,
    mut runner: MultiContractRunner,
    verbosity: u8,
    filter: Filter,
    json: bool,
    allow_failure: bool,
    include_fuzz_tests: bool,
    gas_reporting: bool,
) -> eyre::Result<TestOutcome> {
    if json {
        let results = runner.test(&filter, None, include_fuzz_tests)?;
        println!("{}", serde_json::to_string(&results)?);
        Ok(TestOutcome::new(results, allow_failure))
    } else {
        // Set up identifiers
        let local_identifier = LocalTraceIdentifier::new(&runner.known_contracts);
        let remote_chain_id = runner.evm_opts.get_remote_chain_id();
        // Do not re-query etherscan for contracts that you've already queried today.
        // TODO: Make this configurable.
        let cache_ttl = Duration::from_secs(24 * 60 * 60);
        let etherscan_identifier = EtherscanIdentifier::new(
            remote_chain_id,
            config.etherscan_api_key,
            remote_chain_id.and_then(Config::foundry_etherscan_cache_dir),
            cache_ttl,
        );

        // Set up test reporter channel
        let (tx, rx) = channel::<(String, SuiteResult)>();

        // Run tests
        let handle =
            thread::spawn(move || runner.test(&filter, Some(tx), include_fuzz_tests).unwrap());

        let mut results: BTreeMap<String, SuiteResult> = BTreeMap::new();
        let mut gas_report = GasReport::new(config.gas_reports);
        for (contract_name, suite_result) in rx {
            let mut tests = suite_result.test_results.clone();
            println!();
            if !tests.is_empty() {
                let term = if tests.len() > 1 { "tests" } else { "test" };
                println!("Running {} {} for {}", tests.len(), term, contract_name);
            }
            for (name, result) in &mut tests {
                short_test_result(name, result);

                // We only display logs at level 2 and above
                if verbosity >= 2 {
                    // We only decode logs from Hardhat and DS-style console events
                    let console_logs = decode_console_logs(&result.logs);
                    if !console_logs.is_empty() {
                        println!("Logs:");
                        for log in console_logs {
                            println!("  {}", log);
                        }
                        println!();
                    }
                }

                if !result.traces.is_empty() {
                    // Identify addresses in each trace
                    let mut decoder =
                        CallTraceDecoder::new_with_labels(result.labeled_addresses.clone());

                    // Decode the traces
                    let mut decoded_traces = Vec::new();
                    for (kind, trace) in &mut result.traces {
                        decoder.identify(trace, &local_identifier);
                        decoder.identify(trace, &etherscan_identifier);

                        let should_include = match kind {
                            // At verbosity level 3, we only display traces for failed tests
                            // At verbosity level 4, we also display the setup trace for failed
                            // tests At verbosity level 5, we display
                            // all traces for all tests
                            TraceKind::Setup => {
                                (verbosity >= 5) || (verbosity == 4 && !result.success)
                            }
                            TraceKind::Execution => {
                                verbosity > 3 || (verbosity == 3 && !result.success)
                            }
                            _ => false,
                        };

                        // We decode the trace if we either need to build a gas report or we need
                        // to print it
                        if should_include || gas_reporting {
                            decoder.decode(trace);
                        }

                        if should_include {
                            decoded_traces.push(trace.to_string());
                        }
                    }

                    if !decoded_traces.is_empty() {
                        println!("Traces:");
                        decoded_traces.into_iter().for_each(|trace| println!("{}", trace));
                    }

                    if gas_reporting {
                        gas_report.analyze(&result.traces);
                    }
                }
            }
            let block_outcome = TestOutcome::new(
                [(contract_name.clone(), suite_result.clone())].into(),
                allow_failure,
            );
            println!("{}", block_outcome.summary());
            results.insert(contract_name, suite_result);
        }

        if gas_reporting {
            println!("{}", gas_report.finalize());
        }

        // reattach the thread
        let _ = handle.join();

        Ok(TestOutcome::new(results, allow_failure))
    }
}
