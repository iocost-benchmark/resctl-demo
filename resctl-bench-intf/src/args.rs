// Copyright (c) Facebook, Inc. and its affiliates.
use anyhow::{bail, Result};
use log::error;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::process::exit;
use util::*;

use super::JobSpec;
use rd_agent_intf;

lazy_static::lazy_static! {
    static ref TOP_ARGS_STR: String = format!(
        "-d, --dir=[TOPDIR]     'Top-level dir for operation and scratch files (default: {dfl_dir})'
         -D, --dev=[DEVICE]     'Scratch device override (e.g. nvme0n1)'
         -l, --linux=[PATH]     'Path to linux.tar, downloaded automatically if not specified'
         -r, --result=[PATH]    'Record the bench results into the specified json file'
         -R, --rep-retention=[SECS] '1s report retention in seconds (default: {dfl_rep_ret:.1}h)'
         -a, --args=[FILE]      'Load base command line arguments from FILE'
         -I, --incremental      'Run incremental benchmarks if supported (see bench helps)'
             --clear-reports    'Remove existing report files'
             --keep-reports     'Don't delete expired report files'
         -v...                  'Sets the level of verbosity'",
        dfl_dir = Args::default().dir,
        dfl_rep_ret = Args::default().rep_retention,
    );
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Args {
    pub dir: String,
    pub dev: Option<String>,
    pub linux_tar: Option<String>,
    pub result: Option<String>,
    pub rep_retention: u64,
    pub job_specs: Vec<JobSpec>,

    #[serde(skip)]
    pub incremental: bool,
    #[serde(skip)]
    pub keep_reports: bool,
    #[serde(skip)]
    pub clear_reports: bool,
    #[serde(skip)]
    pub verbosity: u32,
}

impl Default for Args {
    fn default() -> Self {
        Self {
            dir: rd_agent_intf::Args::default().dir.clone(),
            dev: None,
            linux_tar: None,
            result: None,
            job_specs: Default::default(),
            rep_retention: 24 * 3600,
            incremental: false,
            keep_reports: false,
            clear_reports: false,
            verbosity: 0,
        }
    }
}

impl Args {
    fn parse_job_spec(spec: &str) -> Result<JobSpec> {
        let mut toks = spec.split(':');

        let kind = match toks.next() {
            Some(v) => v,
            None => bail!("invalid job type"),
        };

        let mut id = None;
        let mut properties: Vec<BTreeMap<String, String>> = vec![Default::default()];

        for tok in toks {
            if tok.len() == 0 {
                // "::" separates property groups. Allow only the first
                // group, which contains options which apply to all
                // following groups, to be empty.
                if properties.len() == 1 || properties.last().unwrap().len() > 0 {
                    properties.push(Default::default());
                }
                continue;
            }

            // Allow empty key and/or value.
            let mut kv = tok.splitn(2, '=').collect::<Vec<&str>>();
            while kv.len() < 2 {
                kv.push("");
            }

            match kv[0] {
                "id" => id = Some(kv[1]),
                key => {
                    properties
                        .last_mut()
                        .unwrap()
                        .insert(key.into(), kv[1].into());
                }
            }
        }

        if properties.len() > 1 && properties.last().unwrap().len() == 0 {
            properties.pop();
        }

        Ok(JobSpec::new(
            kind.into(),
            id.map(str::to_string),
            properties,
        ))
    }

    fn load_jobfile(fname: &str) -> Result<Vec<JobSpec>> {
        Ok(Self::load(fname)?.job_specs)
    }
}

impl JsonLoad for Args {}
impl JsonSave for Args {}

impl JsonArgs for Args {
    fn match_cmdline() -> clap::ArgMatches<'static> {
        clap::App::new("resctl-bench")
            .version(clap::crate_version!())
            .author(clap::crate_authors!("\n"))
            .about("Facebook Resoruce Control Benchmarks")
            .setting(clap::AppSettings::UnifiedHelpMessage)
            .setting(clap::AppSettings::DeriveDisplayOrder)
            .args_from_usage(&TOP_ARGS_STR)
            .subcommand(
                clap::SubCommand::with_name("run")
                    .about("Run benchmarks")
                    .arg(
                        clap::Arg::with_name("jobfile")
                            .long("job")
                            .short("j")
                            .multiple(true)
                            .takes_value(true)
                            .number_of_values(1)
                            .help("Benchmark job file"),
                    )
                    .arg(
                        clap::Arg::with_name("jobspec")
                            .multiple(true)
                            .help("Benchmark job spec - \"BENCH_TYPE[:KEY=VAL...]\""),
                    ),
            )
            .get_matches()
    }

    fn verbosity(matches: &clap::ArgMatches) -> u32 {
        matches.occurrences_of("v") as u32
    }

    fn process_cmdline(&mut self, matches: &clap::ArgMatches) -> bool {
        let dfl = Args::default();
        let mut updated = false;

        if let Some(v) = matches.value_of("dir") {
            self.dir = if v.len() > 0 {
                v.to_string()
            } else {
                dfl.dir.clone()
            };
            updated = true;
        }
        if let Some(v) = matches.value_of("dev") {
            self.dev = if v.len() > 0 {
                Some(v.to_string())
            } else {
                None
            };
            updated = true;
        }
        if let Some(v) = matches.value_of("linux") {
            self.linux_tar = if v.len() > 0 {
                Some(v.to_string())
            } else {
                None
            };
            updated = true;
        }
        if let Some(v) = matches.value_of("result") {
            self.result = if v.len() > 0 {
                Some(v.to_string())
            } else {
                None
            };
            updated = true;
        }
        if let Some(v) = matches.value_of("rep-retention") {
            self.rep_retention = if v.len() > 0 {
                v.parse::<u64>().unwrap()
            } else {
                dfl.rep_retention
            };
            updated = true;
        }

        self.incremental = matches.is_present("incremental");
        self.keep_reports = matches.is_present("keep-reports");
        self.clear_reports = matches.is_present("clear-reports");
        self.verbosity = Self::verbosity(matches);

        match matches.subcommand() {
            ("run", Some(subm)) => {
                let mut jobsets = BTreeMap::<usize, Vec<JobSpec>>::new();

                match (subm.indices_of("jobspec"), subm.values_of("jobspec")) {
                    (Some(idxs), Some(specs)) => {
                        for (idx, spec) in idxs.zip(specs) {
                            match Self::parse_job_spec(spec) {
                                Ok(v) => {
                                    jobsets.insert(idx, vec![v]);
                                }
                                Err(e) => {
                                    error!("jobspec {:?}: {}", spec, &e);
                                    exit(1);
                                }
                            }
                        }
                    }
                    _ => {}
                }

                match (subm.indices_of("jobfile"), subm.values_of("jobfile")) {
                    (Some(idxs), Some(fnames)) => {
                        for (idx, fname) in idxs.zip(fnames) {
                            match Self::load_jobfile(fname) {
                                Ok(v) => {
                                    jobsets.insert(idx, v);
                                }
                                Err(e) => {
                                    error!("jobfile {:?}: {}", fname, &e);
                                    exit(1);
                                }
                            }
                        }
                    }
                    _ => {}
                }

                if jobsets.len() > 0 {
                    self.job_specs = Vec::new();
                    for jobset in jobsets.values_mut() {
                        self.job_specs.append(jobset);
                    }
                    updated = true;
                }
            }
            _ => {}
        }

        updated
    }
}
